//! Task tool + subagent spawner — see meta/issues/task-tool-subagents.md.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::agent::{
    LOOP_EVENT_CAPACITY, LoopConfig, LoopEvent, LoopEventSender, TurnOutcome, loop_event_channel,
    run_turn,
};
use crate::config::system_prompt_for;
use crate::config::{AgentDefinition, AgentWorkspaceMode};
use crate::provider::ProviderResolver;
use crate::session::{ContentBlock, SessionMeta, SessionStore, new_id};
use crate::tools::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, ToolRegistry, ToolStartObserver,
    WorkspaceAccess,
};
use anyhow::Context;
use serde::Deserialize;

const NOTIFICATION_CAPACITY: usize = 64;
const ACTIVITY_CAPACITY: usize = 256;
/// How long a cancelled or stalled background child may take to finish
/// its graceful abort. That path only appends a partial step to the
/// session log and publishes the terminal event, so seconds are already
/// generous — the bound exists so a provider or tool that ignores
/// cancellation cannot wedge `shutdown`, which waits for these tasks.
const BACKGROUND_ABORT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
/// Poll interval and cap for a notification whose parent session is
/// locked by another turn. A lease that outlives ~3s is held by a turn
/// that is going to keep it, so the notification goes back to the queue
/// instead of spinning at 40 attempts a second until the heat death of
/// the universe.
const NOTIFICATION_LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(25);
const NOTIFICATION_LOCK_ATTEMPTS: usize = 120;

/// A completed background task's notification — the synthetic user
/// message that re-invokes the parent loop.
#[derive(Debug, Clone)]
pub struct Notification {
    pub parent_session_id: String,
    pub description: String,
    pub text: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct SubagentActivity {
    pub parent_session_id: String,
    pub parent_call_id: String,
    pub child_session_id: String,
    pub event: LoopEvent,
}

pub enum RouteOutcome {
    Complete,
    Propagate(Notification),
    Requeue(Notification),
}

/// Spawns child agent loops with their own sessions. Shared across a
/// session's turns (concurrency slot counter) and cloned into children
/// (depth+1) for nesting up to the depth cap.
pub struct SubagentSpawner {
    resolver: Arc<dyn ProviderResolver>,
    store: SessionStore,
    agents: Vec<AgentDefinition>,
    user_config_dir: std::path::PathBuf,
    workspace_location: crate::tools::WorkspaceLocation,
    depth: usize,
    max_concurrent: usize,
    max_depth: usize,
    running: Arc<AtomicUsize>,
    active_sessions: Arc<Mutex<std::collections::HashSet<String>>>,
    active_sessions_changed: tokio::sync::watch::Sender<u64>,
    /// Tasks working right now, nested ones included: the registry is
    /// shared with every child spawner.
    running_tasks: Arc<Mutex<Vec<RunningTask>>>,
    /// Background completions land here; the session owner consumes.
    notify_tx: tokio::sync::mpsc::Sender<Notification>,
    /// The single notification receiver, handed out by `subscribe`.
    notify_rx: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<Notification>>>>,
    activity_tx: tokio::sync::broadcast::Sender<SubagentActivity>,
    stall_timeout: std::time::Duration,
    /// Abort handles for detached background tasks.
    background_tasks: Arc<Mutex<BackgroundRegistry>>,
    workspace: crate::tools::WorkspaceScheduler,
    background_tool_timeout: std::time::Duration,
    loop_config: LoopConfig,
    /// Root session's service manager, shared with mutable child agents.
    services: Option<std::sync::Arc<crate::tools::service::ServiceManager>>,
    /// Models available for per-task overrides and the models tool.
    available_models: Vec<&'static crate::model::ModelInfo>,
}

/// A subagent that is working right now, for anything that wants to
/// show live delegation — the TUI sidebar reads this every frame.
#[derive(Debug, Clone)]
pub struct RunningTask {
    pub session_id: String,
    pub description: String,
    pub agent: String,
    pub background: bool,
    pub started: std::time::Instant,
}

/// Removes its task from the running registry however the run ends —
/// completion, error, abort, or a dropped background future.
struct RunningTaskGuard {
    session_id: String,
    registry: Arc<Mutex<Vec<RunningTask>>>,
}

impl Drop for RunningTaskGuard {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap()
            .retain(|task| task.session_id != self.session_id);
    }
}

struct BackgroundTask {
    id: String,
    handle: tokio::task::JoinHandle<()>,
    cancel: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
struct BackgroundRegistry {
    tasks: Vec<BackgroundTask>,
    closed: bool,
}

impl SubagentSpawner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        resolver: Arc<dyn ProviderResolver>,
        store: SessionStore,
        agents: Vec<AgentDefinition>,
        cwd: std::path::PathBuf,
        depth: usize,
        max_concurrent: usize,
        max_depth: usize,
    ) -> Self {
        let (notify_tx, notify_rx) = tokio::sync::mpsc::channel(NOTIFICATION_CAPACITY);
        let (activity_tx, _) = tokio::sync::broadcast::channel(ACTIVITY_CAPACITY);
        let workspace_location = crate::tools::WorkspaceLocation::shared(cwd);
        let workspace = crate::tools::WorkspaceScheduler::for_location(&workspace_location);
        let (active_sessions_changed, _) = tokio::sync::watch::channel(0);
        Self {
            notify_rx: Arc::new(Mutex::new(Some(notify_rx))),
            resolver,
            store,
            agents,
            user_config_dir: std::path::PathBuf::from("/nonexistent"),
            workspace_location,
            depth,
            max_concurrent,
            max_depth,
            running: Arc::new(AtomicUsize::new(0)),
            active_sessions: Arc::new(Mutex::new(std::collections::HashSet::new())),
            active_sessions_changed,
            running_tasks: Arc::new(Mutex::new(Vec::new())),
            notify_tx,
            activity_tx,
            stall_timeout: std::time::Duration::from_secs(600),
            background_tasks: Arc::new(Mutex::new(BackgroundRegistry::default())),
            workspace,
            background_tool_timeout: std::time::Duration::from_secs(600),
            loop_config: LoopConfig::default(),
            services: None,
            available_models: Vec::new(),
        }
    }

    /// Override the background stall watchdog timeout (tests).
    pub fn with_stall_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.stall_timeout = timeout;
        self
    }

    pub fn with_user_config_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.user_config_dir = dir;
        self
    }

    pub fn with_background_tool_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.background_tool_timeout = timeout;
        self
    }

    pub fn with_available_models(mut self, models: Vec<&'static crate::model::ModelInfo>) -> Self {
        self.available_models = models;
        self
    }

    pub fn with_services(
        mut self,
        services: std::sync::Arc<crate::tools::service::ServiceManager>,
    ) -> Self {
        self.services = Some(services);
        self
    }

    pub fn with_loop_config(mut self, config: LoopConfig) -> Self {
        self.loop_config = config;
        self
    }

    pub fn background_tool_timeout(&self) -> std::time::Duration {
        self.background_tool_timeout
    }

    pub fn workspace(&self) -> crate::tools::WorkspaceScheduler {
        self.workspace.clone()
    }

    pub fn workspace_location(&self) -> crate::tools::WorkspaceLocation {
        self.workspace_location.clone()
    }

    /// Receiver for background-task notifications (single consumer;
    /// second call returns an already-closed receiver).
    pub fn subscribe(&self) -> tokio::sync::mpsc::Receiver<Notification> {
        self.notify_rx
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| tokio::sync::mpsc::channel::<Notification>(1).1)
    }

    pub fn subscribe_activity(&self) -> tokio::sync::broadcast::Receiver<SubagentActivity> {
        self.activity_tx.subscribe()
    }

    /// Abort every detached background task (the pending manager's
    /// cancel; quitting goes through `shutdown`).
    pub fn abort_all(&self) {
        let tasks = self.background_tasks.lock().unwrap();
        for task in &tasks.tasks {
            task.cancel.cancel();
        }
    }

    pub async fn shutdown(&self) {
        let tasks = {
            let mut registry = self.background_tasks.lock().unwrap();
            registry.closed = true;
            for task in &registry.tasks {
                task.cancel.cancel();
            }
            registry
                .tasks
                .drain(..)
                .map(|task| task.handle)
                .collect::<Vec<_>>()
        };
        // Every task was cancelled at once, and each may spend up to
        // BACKGROUND_ABORT_GRACE finishing its abort: wait for them
        // together so quitting costs one grace, not one per child.
        let _ = futures::future::join_all(tasks).await;
    }

    /// Number of live detached background tasks.
    pub fn running_background(&self) -> usize {
        let mut registry = self.background_tasks.lock().unwrap();
        registry.tasks.retain(|task| !task.handle.is_finished());
        registry.tasks.len()
    }

    pub fn resolver(&self) -> Arc<dyn ProviderResolver> {
        self.resolver.clone()
    }

    pub fn agents(&self) -> &[AgentDefinition] {
        &self.agents
    }

    /// A spawner sharing every collaborator with this one — slot counter,
    /// session claims, notification channel, background registry — but
    /// bound to another workspace and depth. The only place the fields are
    /// enumerated: both derivation sites go through it, so a new field
    /// cannot be forgotten on one of them.
    fn derived(
        &self,
        workspace_location: crate::tools::WorkspaceLocation,
        workspace: crate::tools::WorkspaceScheduler,
        depth: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            resolver: self.resolver.clone(),
            store: self.store.clone(),
            agents: self.agents.clone(),
            user_config_dir: self.user_config_dir.clone(),
            workspace_location,
            depth,
            max_concurrent: self.max_concurrent,
            max_depth: self.max_depth,
            running: self.running.clone(),
            active_sessions: self.active_sessions.clone(),
            active_sessions_changed: self.active_sessions_changed.clone(),
            running_tasks: self.running_tasks.clone(),
            notify_tx: self.notify_tx.clone(),
            notify_rx: self.notify_rx.clone(),
            activity_tx: self.activity_tx.clone(),
            stall_timeout: self.stall_timeout,
            background_tasks: self.background_tasks.clone(),
            workspace,
            background_tool_timeout: self.background_tool_timeout,
            loop_config: self.loop_config.clone(),
            services: self.services.clone(),
            available_models: self.available_models.clone(),
        })
    }

    /// The tool registry an agent runs with under this spawner. A
    /// read-only agent gets the enforced read-only set — no delegation,
    /// no shell; a mutable one gets the builtins plus delegation,
    /// services and the model listing. Either way the agent definition's
    /// `tools` allowlist narrows the result. Called on the spawner the
    /// agent will itself delegate through, so its task tool and its
    /// services can never come from two different spawners.
    fn agent_registry(
        self: &Arc<Self>,
        agent: &AgentDefinition,
    ) -> Result<ToolRegistry, crate::tools::DuplicateToolError> {
        let registry = match agent.workspace_mode {
            AgentWorkspaceMode::ReadOnly => ToolRegistry::read_only(),
            AgentWorkspaceMode::Mutable => {
                let registry = ToolRegistry::builtin().with_subagents(self.clone())?;
                let registry = match self.services.clone() {
                    Some(services) => registry.with_services(services)?,
                    None => registry,
                };
                registry.with_models(self.available_models.clone())?
            }
        };
        Ok(match &agent.tools {
            Some(tools) => registry.restricted_to(tools),
            None => registry,
        })
    }

    /// The system prompt an agent runs with under this spawner: the
    /// context of the workspace it will work in, plus its own prompt.
    fn agent_system_prompt(
        &self,
        agent: &AgentDefinition,
        cwd: &std::path::Path,
    ) -> anyhow::Result<String> {
        Ok(crate::runtime::with_agent_prompt(
            system_prompt_for(&self.user_config_dir, cwd)?,
            agent,
        ))
    }

    /// Run one subagent task; returns its final text as the tool output.
    pub async fn run_task(self: &Arc<Self>, input: TaskInput, ctx: &ToolContext) -> ToolOutput {
        self.run_task_observed(input, ctx, None).await
    }

    async fn run_task_observed(
        self: &Arc<Self>,
        input: TaskInput,
        ctx: &ToolContext,
        mut on_start: Option<ToolStartObserver>,
    ) -> ToolOutput {
        if self.depth >= self.max_depth {
            return ToolOutput::error(format!(
                "Subagent nesting limit reached (depth {} of {}). Complete this task directly with your tools instead of spawning another agent.",
                self.depth, self.max_depth
            ));
        }
        let Some(agent) = self.agents.iter().find(|a| a.name == input.subagent_type) else {
            let available: Vec<&str> = self.agents.iter().map(|a| a.name.as_str()).collect();
            return ToolOutput::error(format!(
                "unknown subagent_type {:?}; available: {}",
                input.subagent_type,
                available.join(", ")
            ));
        };
        let workspace_access = match agent.workspace_mode {
            AgentWorkspaceMode::Mutable => WorkspaceAccess::Mutating,
            AgentWorkspaceMode::ReadOnly => WorkspaceAccess::ReadOnly,
        };
        let child_location = match &input.workspace {
            Some(workspace) => {
                let TaskWorkspaceIsolation::GitWorktree = workspace.isolation;
                match crate::tools::WorkspaceLocation::validated_git_worktree(
                    &ctx.location,
                    workspace.cwd.clone(),
                )
                .await
                {
                    Ok(location) => location,
                    Err(error) => {
                        return ToolOutput::error(format!("invalid task workspace: {error:#}"));
                    }
                }
            }
            None => ctx.location.clone(),
        };
        let same_workspace = child_location.id() == ctx.location.id();
        let cross_workspace_nested = !same_workspace && ctx.has_workspace_lease();
        if !same_workspace
            && ctx
                .workspace_ancestry
                .iter()
                .any(|id| id == child_location.id())
        {
            return ToolOutput::error(
                "task workspace is already held by an ancestor; finish the intervening task before returning to it",
            );
        }
        if input.background == Some(true) && same_workspace && ctx.has_workspace_lease() {
            return ToolOutput::error(
                "background tasks cannot outlive a parent workspace lease; use a foreground task or validated worktree",
            );
        }
        if workspace_access == WorkspaceAccess::Mutating
            && same_workspace
            && ctx.has_workspace_lease()
        {
            return ToolOutput::error(
                "nested mutable tasks cannot reuse their parent checkout; use a validated worktree",
            );
        }
        let inherited_lease = if same_workspace {
            match ctx.workspace_coverage(workspace_access) {
                crate::tools::WorkspaceCoverage::Covered => ctx.workspace_lease.clone(),
                crate::tools::WorkspaceCoverage::Absent => None,
                crate::tools::WorkspaceCoverage::Incompatible => {
                    return ToolOutput::error(
                        "mutable task cannot run inside a read-only child workspace",
                    );
                }
            }
        } else {
            None
        };
        let notification_permit = if input.background == Some(true) {
            match self.notify_tx.clone().try_reserve_owned() {
                Ok(permit) => Some(permit),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    return ToolOutput::error(
                        "background task capacity is full; retry after a notification is handled",
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    return ToolOutput::error("background notification receiver is closed");
                }
            }
        } else {
            None
        };
        let child_workspace = ctx.workspace.scoped(&child_location);
        let system_prompt = match self.agent_system_prompt(agent, child_location.cwd()) {
            Ok(prompt) => prompt,
            Err(error) => {
                return ToolOutput::error(format!("loading subagent context: {error:#}"));
            }
        };

        let mut active_session = match &input.task_id {
            Some(id) => match self.claim_session(id) {
                Some(claim) => Some(claim),
                None => {
                    return ToolOutput::error(format!(
                        "task session {id:?} is already active; wait for it to finish before resuming"
                    ));
                }
            },
            None => None,
        };

        // Concurrency slot: Claude Code semantics — over cap is a soft
        // error the model must not retry.
        if self.running.fetch_add(1, Ordering::SeqCst) >= self.max_concurrent {
            self.running.fetch_sub(1, Ordering::SeqCst);
            return ToolOutput::error(format!(
                "Concurrent subagent limit reached ({}/{}). Do not retry. Finish other work first, then try again.",
                self.max_concurrent, self.max_concurrent
            ));
        }
        let _guard = SlotGuard(self.running.clone());

        // Session: resume task_id if given and loadable, else a fresh child.
        let session_id = match &input.task_id {
            Some(id) => match self.store.load(id) {
                Ok(session) => {
                    let Some(meta) = session.meta() else {
                        return ToolOutput::error(format!(
                            "resuming task session {id:?}: session has no metadata"
                        ));
                    };
                    if meta.agent != input.subagent_type {
                        return ToolOutput::error(format!(
                            "resuming task session {id:?}: persisted agent does not match {:?}",
                            input.subagent_type
                        ));
                    }
                    if meta.parent_id.as_deref() != Some(ctx.session_id.as_str()) {
                        return ToolOutput::error(format!(
                            "resuming task session {id:?}: persisted parent does not match the invoking session"
                        ));
                    }
                    match &meta.workspace {
                        Some(persisted) => {
                            let restored = if persisted == &ctx.location {
                                ctx.location.clone()
                            } else if input.workspace.is_none() {
                                return ToolOutput::error(format!(
                                    "resuming task session {id:?}: workspace differs from its parent; provide its explicit workspace"
                                ));
                            } else {
                                match crate::tools::WorkspaceLocation::revalidate(
                                    &ctx.location,
                                    persisted,
                                )
                                .await
                                {
                                    Ok(location) => location,
                                    Err(error) => {
                                        return ToolOutput::error(format!(
                                            "resuming task session {id:?}: persisted workspace is invalid: {error:#}"
                                        ));
                                    }
                                }
                            };
                            if restored != *persisted {
                                return ToolOutput::error(format!(
                                    "resuming task session {id:?}: persisted workspace metadata does not match its canonical location"
                                ));
                            }
                            if restored != child_location {
                                return ToolOutput::error(format!(
                                    "resuming task session {id:?}: workspace does not match; provide its validated worktree"
                                ));
                            }
                        }
                        None if input.workspace.is_some() => {
                            return ToolOutput::error(format!(
                                "resuming task session {id:?}: session has no workspace metadata and cannot adopt an isolated workspace"
                            ));
                        }
                        None => {}
                    }
                    id.clone()
                }
                Err(error) => {
                    return ToolOutput::error(format!("resuming task session {id:?}: {error}"));
                }
            },
            None => {
                let id = new_id();
                let requested_variant = input.reasoning.clone().map(TaskVariant::from_input);
                let (model, child_variant) = match (&input.model, &agent.model) {
                    (Some(model), _) | (None, Some(model)) => (model.clone(), requested_variant),
                    (None, None) => match self.store.load(&ctx.session_id) {
                        Ok(parent) => (
                            parent.effective_model(),
                            requested_variant.or_else(|| {
                                parent.effective_variant().map(TaskVariant::from_parent)
                            }),
                        ),
                        Err(error) => {
                            return ToolOutput::error(format!(
                                "loading parent session {:?}: {error}",
                                ctx.session_id
                            ));
                        }
                    },
                };
                if input.model.is_some() {
                    let known = self
                        .available_models
                        .iter()
                        .any(|candidate| candidate.full_id() == model)
                        || (self.available_models.is_empty()
                            && crate::model::find(&model).is_some());
                    if !known {
                        return ToolOutput::error(format!(
                            "unknown or unavailable model {model:?}; call the models tool for the list"
                        ));
                    }
                    if let Err(error) = self.resolver.resolve_provider(&model) {
                        return ToolOutput::error(format!(
                            "no provider configured for {model}: {error:#}"
                        ));
                    }
                }
                if let Some(variant) = &child_variant
                    && crate::model::variant_options(&model, Some(&variant.id)).is_err()
                {
                    return ToolOutput::error(variant.rejection(&model));
                }
                let created = self.store.create(SessionMeta {
                    session_id: id.clone(),
                    parent_id: Some(ctx.session_id.clone()),
                    agent: input.subagent_type.clone(),
                    model,
                    workspace: Some(child_location.clone()),
                });
                match created {
                    Ok(mut session) => {
                        let child_model = session.effective_model();
                        if let Some(variant) = child_variant
                            && let Err(error) =
                                session.append(crate::session::SessionEvent::ModelChange {
                                    id: new_id(),
                                    model: child_model,
                                    variant: Some(variant.id),
                                    ts: chrono::Utc::now(),
                                })
                        {
                            drop(session);
                            rollback_created_session(&self.store, &id);
                            return ToolOutput::error(format!(
                                "persisting subagent reasoning variant: {error}"
                            ));
                        }
                        drop(session);
                    }
                    Err(error) => {
                        return ToolOutput::error(format!("creating subagent session: {error}"));
                    }
                };
                id
            }
        };
        if active_session.is_none() {
            active_session = self.claim_session(&session_id);
        }
        let _active_session = active_session.expect("new session id must be unique");
        // From here the child is working: everything below either runs
        // it or moves the guard into the task that will.
        let _running_task = self.register_running(RunningTask {
            session_id: session_id.clone(),
            description: input.description.clone(),
            agent: agent.name.clone(),
            background: input.background == Some(true),
            started: std::time::Instant::now(),
        });
        let parent_call_id = ctx.call_id.clone().unwrap_or_default();

        // The child delegates one level deeper, sharing this spawner's
        // slot counter, session claims and notification channel.
        let child_spawner = self.derived(
            child_location.clone(),
            child_workspace.clone(),
            self.depth + 1,
        );
        let registry = match child_spawner.agent_registry(agent) {
            Ok(registry) => registry,
            Err(error) => {
                return ToolOutput::error(format!("building child tool registry: {error}"));
            }
        };
        let mut workspace_ancestry = ctx.workspace_ancestry.clone();
        if !workspace_ancestry
            .iter()
            .any(|id| id == child_location.id())
        {
            workspace_ancestry.push(child_location.id().clone());
        }
        let mut child_ctx = ToolContext {
            cwd: child_location.cwd().to_path_buf(),
            session_id: session_id.clone(),
            call_id: ctx.call_id.clone(),
            depth: self.depth + 1,
            subagent: Some(child_spawner),
            workspace: child_workspace.clone(),
            location: child_location.clone(),
            workspace_lease: None,
            workspace_ancestry,
            cancel: ctx.cancel.clone(),
            output_tail: None,
        };

        if input.background == Some(true) {
            let notification_permit = notification_permit.expect("reserved for background task");
            // Detached: run the child on a spawned task with a stall
            // watchdog; completion lands as a notification for the parent
            // loop; the tool call returns immediately.
            let spawner = Arc::clone(self);
            let description = input.description.clone();
            let prompt = input.prompt.clone();
            let parent_session_id = ctx.session_id.clone();
            let stall_timeout = self.stall_timeout;
            // A child of the turn's token, so one token stands for "this
            // task should stop": the parent turn ending cancels it, and
            // `abort_all`/`shutdown` can still cancel it alone.
            let background_cancel = ctx.cancel.child_token();
            let task_cancel = background_cancel.clone();
            let root_cancel = ctx.cancel.clone();
            let workspace = child_workspace.clone();
            let parent_location = ctx.location.clone();
            let leased_location = child_location.clone();
            let mut background_registry = self.background_tasks.lock().unwrap();
            if background_registry.closed {
                return ToolOutput::error("background runtime is shutting down");
            }
            background_registry
                .tasks
                .retain(|task| !task.handle.is_finished());
            let registry_id = new_id();
            let task_registry_id = registry_id.clone();
            let task_registry = self.background_tasks.clone();
            let activity = ActivityPublisher {
                tx: self.activity_tx.clone(),
                parent_session_id: parent_session_id.clone(),
                parent_call_id,
                child_session_id: session_id.clone(),
            };
            let returned_session_id = session_id.clone();
            let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(async move {
                let mut notification_permit = Some(notification_permit);
                if registered_rx.await.is_err() {
                    return;
                }
                let _background_task = BackgroundTaskGuard {
                    id: task_registry_id,
                    registry: task_registry,
                };
                let _active_session = _active_session;
                let _running_task = _running_task; // deregisters when the task ends
                let _slot = _guard; // hold the concurrency slot for the run
                // Nobody is waiting on a background task, so it stops the
                // moment it is told to — including part-way through the
                // revalidation, which may be a git call.
                let acquired = tokio::select! {
                    outcome = acquire_task_lease(
                        &workspace,
                        workspace_access,
                        inherited_lease,
                        cross_workspace_nested,
                        &parent_location,
                        &leased_location,
                        &task_cancel,
                    ) => outcome,
                    () = task_cancel.cancelled() => LeaseOutcome::Cancelled,
                };
                let lease = match acquired {
                    LeaseOutcome::Acquired(lease) => lease,
                    LeaseOutcome::Cancelled => {
                        publish_reserved_notification(
                            &mut notification_permit,
                            cancelled_task_notification(&parent_session_id, &description),
                        );
                        return;
                    }
                    LeaseOutcome::Failed(failure) => {
                        publish_reserved_notification(
                            &mut notification_permit,
                            task_notification(
                                &parent_session_id,
                                &description,
                                &format!("Task \"{description}\" failed: {}", failure.message()),
                                true,
                            ),
                        );
                        return;
                    }
                };
                child_ctx.workspace_lease = Some(lease);
                // Deliberately not a child of `task_cancel`: stopping the
                // task must go through the select below, which cancels
                // this token itself and then waits out the graceful
                // abort. Wiring it to `task_cancel` would let the turn
                // report a plain abort before the select noticed.
                let cancel = root_cancel.child_token();
                let (tx, mut rx_evt) = loop_event_channel(LOOP_EVENT_CAPACITY);
                // Activity tracker: any child event counts as progress.
                let last_activity = Arc::new(Mutex::new(std::time::Instant::now()));
                let watcher_last = last_activity.clone();
                let watcher_activity = activity.clone();
                let watcher = tokio::spawn(async move {
                    while let Some(event) = rx_evt.recv().await {
                        *watcher_last.lock().unwrap() = std::time::Instant::now();
                        watcher_activity.publish(event);
                    }
                });
                let stall_watch = async {
                    loop {
                        tokio::time::sleep(stall_timeout / 2).await;
                        if last_activity.lock().unwrap().elapsed() >= stall_timeout {
                            return;
                        }
                    }
                };
                let mut turn = Box::pin(run_turn(
                    spawner.resolver.as_ref(),
                    &registry,
                    &spawner.store,
                    &session_id,
                    &prompt,
                    &[],
                    Some(&system_prompt),
                    spawner.loop_config.clone(),
                    tx,
                    cancel.clone(),
                    child_ctx,
                    // Subagents have no interactive user to steer them.
                    None,
                ));
                // Whichever way this ends, the child stops the same way a
                // foreground one does: the token is cancelled and the turn
                // is awaited, never dropped mid-flight.
                let stopped = async {
                    tokio::select! {
                        () = stall_watch => false,
                        () = task_cancel.cancelled() => true,
                    }
                };
                let (outcome, was_cancelled) = tokio::select! {
                    outcome = &mut turn => (Some(outcome), false),
                    was_cancelled = stopped => {
                        cancel.cancel();
                        let _ = tokio::time::timeout(BACKGROUND_ABORT_GRACE, &mut turn).await;
                        (None, was_cancelled)
                    }
                };
                // The event channel closes with the turn; the watcher ends
                // with it.
                drop(turn);
                let _ = watcher.await;
                let outcome = match outcome {
                    Some(result) => TaskOutcome::from_turn(result),
                    None if was_cancelled => TaskOutcome::Cancelled,
                    None => TaskOutcome::Stalled,
                };
                activity.turn_done(outcome.activity());

                let failed =
                    |body: String| task_notification(&parent_session_id, &description, &body, true);
                let notification = match &outcome {
                    TaskOutcome::Completed => {
                        let text = final_assistant_text(&spawner.store, &session_id)
                            .unwrap_or_else(|| "(finished with no text)".into());
                        task_notification(
                            &parent_session_id,
                            &description,
                            &format!(
                                "Task \"{description}\" completed (task_id: {session_id}).\n<result>\n{text}\n</result>"
                            ),
                            false,
                        )
                    }
                    TaskOutcome::Aborted => failed(format!("Task \"{description}\" was aborted.")),
                    TaskOutcome::MaxIterations => failed(format!(
                        "Task \"{description}\" failed: subagent reached its iteration limit."
                    )),
                    TaskOutcome::Failed(error) => {
                        failed(format!("Task \"{description}\" failed: {error:#}"))
                    }
                    TaskOutcome::Cancelled => {
                        cancelled_task_notification(&parent_session_id, &description)
                    }
                    TaskOutcome::Stalled => failed(format!(
                        "Task \"{description}\" stalled: no progress for {}s. It has been stopped.",
                        stall_timeout.as_secs()
                    )),
                };
                publish_reserved_notification(&mut notification_permit, notification);
            });
            background_registry.tasks.push(BackgroundTask {
                id: registry_id,
                handle,
                cancel: background_cancel,
            });
            let _ = registered_tx.send(());
            if let Some(on_start) = on_start.take() {
                on_start();
            }
            return ToolOutput::text(format!(
                "Deferred background task started (task_id: {returned_session_id}). Completion \
will trigger a separate follow-up turn. Do not sleep, poll, or check on it. Do not perform this \
task's scope yourself; continue only clearly disjoint work."
            ))
            .with_child_session(returned_session_id);
        }

        let lease = match acquire_task_lease(
            &child_workspace,
            workspace_access,
            inherited_lease,
            cross_workspace_nested,
            &ctx.location,
            &child_location,
            &ctx.cancel,
        )
        .await
        {
            LeaseOutcome::Acquired(lease) => lease,
            LeaseOutcome::Cancelled => {
                return ToolOutput::error("subagent cancelled while waiting for workspace");
            }
            LeaseOutcome::Failed(failure) => return ToolOutput::error(failure.message()),
        };
        child_ctx.workspace_lease = Some(lease);
        if let Some(on_start) = on_start.take() {
            on_start();
        }
        let (tx, mut rx_evt) = loop_event_channel(LOOP_EVENT_CAPACITY);
        let activity = ActivityPublisher {
            tx: self.activity_tx.clone(),
            parent_session_id: ctx.session_id.clone(),
            parent_call_id,
            child_session_id: session_id.clone(),
        };
        let turn = run_turn(
            self.resolver.as_ref(),
            &registry,
            &self.store,
            &session_id,
            &input.prompt,
            &[],
            Some(&system_prompt),
            self.loop_config.clone(),
            tx,
            ctx.cancel.clone(),
            child_ctx,
            // Subagents have no interactive user to steer them.
            None,
        );
        tokio::pin!(turn);
        let outcome = loop {
            tokio::select! {
                event = rx_evt.recv() => {
                    if let Some(event) = event {
                        activity.publish(event);
                    }
                }
                outcome = &mut turn => break outcome,
            }
        };
        while let Ok(event) = rx_evt.try_recv() {
            activity.publish(event);
        }
        let outcome = TaskOutcome::from_turn(outcome);
        activity.turn_done(outcome.activity());

        let output = match outcome {
            TaskOutcome::Completed => {
                let text = final_assistant_text(&self.store, &session_id)
                    .unwrap_or_else(|| "(subagent finished with no text)".into());
                ToolOutput::text(text)
            }
            TaskOutcome::MaxIterations => {
                ToolOutput::error("subagent failed: iteration limit reached")
            }
            TaskOutcome::Failed(error) => ToolOutput::error(format!("subagent failed: {error:#}")),
            // `from_turn` never yields the last two: a foreground task
            // has no watchdog, and its cancellation arrives as an
            // aborted turn.
            TaskOutcome::Aborted | TaskOutcome::Cancelled | TaskOutcome::Stalled => {
                ToolOutput::error("subagent aborted")
            }
        };
        // The session outlives the call, so name it: without this the
        // model cannot resume a task it just ran, and the resume path
        // tells it never to invent an id. A failed run is worth naming
        // too — an iteration-limited task is the one most worth
        // resuming.
        output
            .with_appended_text(&format!("\n\n(task_id: {session_id})"))
            .with_child_session(session_id.clone())
    }

    pub async fn spawn_background_tool(
        self: &Arc<Self>,
        parent_session_id: String,
        description: String,
        timeout: std::time::Duration,
        future: ToolFuture,
        access: WorkspaceAccess,
        root_cancel: tokio_util::sync::CancellationToken,
    ) -> ToolOutput {
        let notification_permit = match self.notify_tx.clone().try_reserve_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                return ToolOutput::error(
                    "background task capacity is full; retry after a notification is handled",
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                return ToolOutput::error("background notification receiver is closed");
            }
        };
        let job_id = new_id();
        let notification_id = job_id.clone();
        let background_cancel = tokio_util::sync::CancellationToken::new();
        let task_cancel = background_cancel.clone();
        let workspace = self.workspace.clone();
        {
            let mut background_registry = self.background_tasks.lock().unwrap();
            if background_registry.closed {
                return ToolOutput::error("background runtime is shutting down");
            }
            background_registry
                .tasks
                .retain(|task| !task.handle.is_finished());
            let registry_id = new_id();
            let task_registry_id = registry_id.clone();
            let task_registry = self.background_tasks.clone();
            let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(async move {
                let mut notification_permit = Some(notification_permit);
                if registered_rx.await.is_err() {
                    return;
                }
                let _background_task = BackgroundTaskGuard {
                    id: task_registry_id,
                    registry: task_registry,
                };
                let outcome = tokio::select! {
                    outcome = tokio::time::timeout(timeout, async move {
                        let _permit = workspace.acquire(access).await;
                        future.await
                    }) => Some(outcome),
                    () = task_cancel.cancelled() => None,
                    () = root_cancel.cancelled() => None,
                };
                let (text, is_error) = match outcome {
                    Some(Ok(output)) if output.is_error => (
                        format!(
                            "<tool-notification>\nBackground job {notification_id} (\"{description}\") failed.\n<result>\n{}\n</result>\n</tool-notification>",
                            output.content
                        ),
                        true,
                    ),
                    Some(Ok(output)) => (
                        format!(
                            "<tool-notification>\nBackground job {notification_id} (\"{description}\") completed.\n<result>\n{}\n</result>\n</tool-notification>",
                            output.content
                        ),
                        false,
                    ),
                    Some(Err(_)) => (
                        format!(
                            "<tool-notification>\nBackground job {notification_id} (\"{description}\") timed out after {}ms and was stopped.\n</tool-notification>",
                            timeout.as_millis()
                        ),
                        true,
                    ),
                    None => (
                        format!(
                            "<tool-notification>\nBackground job {notification_id} (\"{description}\") was cancelled.\n</tool-notification>"
                        ),
                        true,
                    ),
                };
                publish_reserved_notification(
                    &mut notification_permit,
                    Notification {
                        parent_session_id,
                        description,
                        text,
                        is_error,
                    },
                );
            });
            background_registry.tasks.push(BackgroundTask {
                id: registry_id,
                handle,
                cancel: background_cancel,
            });
            let _ = registered_tx.send(());
        }
        ToolOutput::text(format!(
            "Background job {job_id} started. You will be notified when it completes. Do not poll or sleep; continue other work or end your response."
        ))
    }

    /// Run a queued completion against its declared inactive parent. Child
    /// parents propagate one synthesized completion to their own parent.
    pub async fn route_notification(
        self: &Arc<Self>,
        notification: Notification,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<RouteOutcome> {
        let parent = match self.store.load(&notification.parent_session_id) {
            Ok(parent) => parent,
            Err(_) => return Ok(RouteOutcome::Requeue(notification)),
        };
        let meta = parent
            .meta()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("notification parent has no metadata"))?;
        let agent = self
            .agents
            .iter()
            .find(|agent| agent.name == meta.agent)
            .ok_or_else(|| anyhow::anyhow!("unknown persisted agent {:?}", meta.agent))?;
        let (workspace_location, depth) = match session_workspace_location(
            &self.store,
            &notification.parent_session_id,
            &self.workspace_location,
        )
        .await
        {
            Ok(location) => location,
            Err(error) => {
                return workspace_route_failure(&meta, notification, error);
            }
        };
        let workspace = self.workspace.scoped(&workspace_location);
        let runtime = self.derived(workspace_location.clone(), workspace.clone(), depth);
        let Some(_active_session) = self
            .wait_for_session_claim(&notification.parent_session_id, &cancel)
            .await
        else {
            return Ok(RouteOutcome::Requeue(notification));
        };
        let workspace_access = match agent.workspace_mode {
            AgentWorkspaceMode::Mutable => WorkspaceAccess::Mutating,
            AgentWorkspaceMode::ReadOnly => WorkspaceAccess::ReadOnly,
        };
        let registry = runtime.agent_registry(agent)?;
        let system_prompt = match self.agent_system_prompt(agent, workspace_location.cwd()) {
            Ok(prompt) => prompt,
            Err(error) => return context_route_failure(&meta, notification, error),
        };
        let lease = tokio::select! {
            lease = workspace.acquire_lease(workspace_access) => lease,
            () = cancel.cancelled() => return Ok(RouteOutcome::Requeue(notification)),
        };
        let (leased_location, leased_depth) = match revalidate_after_lease_for_session(
            &self.store,
            &notification.parent_session_id,
            &self.workspace_location,
        )
        .await
        {
            Ok(location) => location,
            Err(error) => return workspace_route_failure(&meta, notification, error),
        };
        if leased_location != workspace_location || leased_depth != depth {
            return workspace_route_failure(
                &meta,
                notification,
                anyhow::anyhow!("workspace changed while waiting for its lease"),
            );
        }
        let mut lock_attempts = 0;
        let outcome = loop {
            if cancel.is_cancelled() {
                return Ok(RouteOutcome::Requeue(notification));
            }
            let result = run_turn(
                self.resolver.as_ref(),
                &registry,
                &self.store,
                &notification.parent_session_id,
                &notification.text,
                &[],
                Some(&system_prompt),
                self.loop_config.clone(),
                discarded_event_sender(),
                cancel.clone(),
                ToolContext {
                    cwd: workspace_location.cwd().to_path_buf(),
                    session_id: notification.parent_session_id.clone(),
                    call_id: None,
                    depth,
                    subagent: Some(runtime.clone()),
                    workspace: workspace.clone(),
                    location: workspace_location.clone(),
                    workspace_lease: Some(lease.clone()),
                    workspace_ancestry: vec![workspace_location.id().clone()],
                    cancel: cancel.clone(),
                    output_tail: None,
                },
                // Subagents have no interactive user to steer them.
                None,
            )
            .await;
            match result {
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
                {
                    // The lease belongs to another turn and nothing was
                    // appended: wait for it briefly, then hand the
                    // notification back like every other transient
                    // failure here rather than spinning on the lock.
                    lock_attempts += 1;
                    if lock_attempts >= NOTIFICATION_LOCK_ATTEMPTS {
                        return Ok(RouteOutcome::Requeue(notification));
                    }
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(RouteOutcome::Requeue(notification)),
                        () = tokio::time::sleep(NOTIFICATION_LOCK_RETRY) => {}
                    }
                }
                result => break result,
            }
        };
        let Some(grandparent_id) = meta.parent_id else {
            return match outcome {
                Ok(TurnOutcome::Completed) => Ok(RouteOutcome::Complete),
                Ok(TurnOutcome::Aborted) => {
                    Err(anyhow::anyhow!("notification parent turn was aborted"))
                }
                Ok(TurnOutcome::MaxIterations) => Err(anyhow::anyhow!(
                    "notification parent reached its iteration limit"
                )),
                Err(error) => Err(error),
            };
        };
        let (text, is_error) = match outcome {
            Ok(TurnOutcome::Aborted) => ("Nested parent turn was cancelled.".to_string(), true),
            Ok(TurnOutcome::MaxIterations) => (
                "Nested parent turn reached its iteration limit.".to_string(),
                true,
            ),
            Ok(TurnOutcome::Completed) => (
                final_assistant_text(&self.store, &notification.parent_session_id)
                    .unwrap_or_else(|| "(finished with no text)".into()),
                false,
            ),
            Err(error) => (format!("Nested parent turn failed: {error:#}"), true),
        };
        let status = if is_error { "failed" } else { "completed" };
        Ok(RouteOutcome::Propagate(Notification {
            parent_session_id: grandparent_id,
            description: notification.description,
            text: format!(
                "<task-notification>\nNested task {status}.\n<result>\n{text}\n</result>\n</task-notification>"
            ),
            is_error,
        }))
    }

    /// Whether a session is running right now — a claimed session is one
    /// a turn is driving, and the listing says so rather than showing a
    /// stale "last word".
    pub fn session_is_active(&self, session_id: &str) -> bool {
        self.active_sessions.lock().unwrap().contains(session_id)
    }

    /// The subagents working right now, oldest first, across every
    /// depth — one shared registry, so a nested task shows up too.
    pub fn running_tasks(&self) -> Vec<RunningTask> {
        self.running_tasks.lock().unwrap().clone()
    }

    fn register_running(&self, task: RunningTask) -> RunningTaskGuard {
        let session_id = task.session_id.clone();
        self.running_tasks.lock().unwrap().push(task);
        RunningTaskGuard {
            session_id,
            registry: self.running_tasks.clone(),
        }
    }

    fn claim_session(&self, session_id: &str) -> Option<ActiveSessionGuard> {
        let mut active = self.active_sessions.lock().unwrap();
        if !active.insert(session_id.to_string()) {
            return None;
        }
        Some(ActiveSessionGuard {
            session_id: session_id.to_string(),
            active: self.active_sessions.clone(),
            changed: self.active_sessions_changed.clone(),
        })
    }

    async fn wait_for_session_claim(
        &self,
        session_id: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<ActiveSessionGuard> {
        let mut changed = self.active_sessions_changed.subscribe();
        loop {
            if let Some(claim) = self.claim_session(session_id) {
                return Some(claim);
            }
            tokio::select! {
                result = changed.changed() => result.ok()?,
                () = cancel.cancelled() => return None,
            }
        }
    }
}

async fn session_workspace_location(
    store: &SessionStore,
    session_id: &str,
    root: &crate::tools::WorkspaceLocation,
) -> anyhow::Result<(crate::tools::WorkspaceLocation, usize)> {
    let mut chain = Vec::new();
    let mut current = session_id.to_string();
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(current.clone()) {
            return Err(SessionAncestryError(format!("session parent cycle at {current}")).into());
        }
        let session = store.load(&current).map_err(|error| {
            SessionAncestryError(format!("loading session {current:?}: {error}"))
        })?;
        let meta = session
            .meta()
            .cloned()
            .ok_or_else(|| SessionAncestryError(format!("session {current} has no metadata")))?;
        let parent = meta.parent_id.clone();
        chain.push(meta);
        let Some(parent) = parent else {
            break;
        };
        current = parent;
    }

    let mut location = root.clone();
    for meta in chain.iter().rev() {
        let Some(persisted) = &meta.workspace else {
            continue;
        };
        if persisted == &location {
            continue;
        }
        if persisted.id() == location.id() {
            anyhow::bail!(
                "session {:?} workspace does not match its parent's cwd and isolation",
                meta.session_id
            );
        }
        let restored = crate::tools::WorkspaceLocation::revalidate(&location, persisted).await?;
        if restored != *persisted {
            anyhow::bail!(
                "session {:?} workspace metadata does not match its canonical location",
                meta.session_id
            );
        }
        location = restored;
    }
    Ok((location, chain.len().saturating_sub(1)))
}

async fn revalidate_after_lease_for_session(
    store: &SessionStore,
    session_id: &str,
    root: &crate::tools::WorkspaceLocation,
) -> anyhow::Result<(crate::tools::WorkspaceLocation, usize)> {
    session_workspace_location(store, session_id, root).await
}

fn workspace_route_failure(
    meta: &SessionMeta,
    notification: Notification,
    error: anyhow::Error,
) -> anyhow::Result<RouteOutcome> {
    if error.downcast_ref::<SessionAncestryError>().is_some() {
        return Ok(RouteOutcome::Requeue(notification));
    }
    let Some(grandparent_id) = &meta.parent_id else {
        return Err(anyhow::anyhow!(
            "notification workspace routing failed: {error:#}"
        ));
    };
    Ok(RouteOutcome::Propagate(Notification {
        parent_session_id: grandparent_id.clone(),
        description: notification.description,
        text: format!(
            "<task-notification>\nNested task failed because its workspace could not be restored.\n<result>\n{error:#}\n</result>\n</task-notification>"
        ),
        is_error: true,
    }))
}

fn context_route_failure(
    meta: &SessionMeta,
    notification: Notification,
    error: anyhow::Error,
) -> anyhow::Result<RouteOutcome> {
    let Some(grandparent_id) = &meta.parent_id else {
        return Err(error).context("loading routed subagent context");
    };
    Ok(RouteOutcome::Propagate(Notification {
        parent_session_id: grandparent_id.clone(),
        description: notification.description,
        text: format!(
            "<task-notification>\nNested task failed while loading its context.\n<result>\n{error:#}\n</result>\n</task-notification>"
        ),
        is_error: true,
    }))
}

#[derive(Debug, thiserror::Error)]
#[error("invalid session ancestry: {0}")]
struct SessionAncestryError(String);

async fn revalidate_after_lease(
    parent: &crate::tools::WorkspaceLocation,
    location: &crate::tools::WorkspaceLocation,
) -> anyhow::Result<crate::tools::WorkspaceLocation> {
    if location == parent {
        return Ok(location.clone());
    }
    match location.isolation() {
        crate::tools::WorkspaceIsolation::Shared => Ok(location.clone()),
        crate::tools::WorkspaceIsolation::GitWorktree { .. } => {
            crate::tools::WorkspaceLocation::revalidate(parent, location).await
        }
    }
}

/// A child's reasoning variant together with where it came from. A
/// rejected variant has to name its source: reporting an explicit
/// `reasoning` input as "inherited" sends the model looking for a
/// setting it just passed in, and reporting an inherited one as an input
/// sends it looking for one it never wrote.
struct TaskVariant {
    id: String,
    source: TaskVariantSource,
}

enum TaskVariantSource {
    /// The task call's own `reasoning` field.
    Input,
    /// The parent session's current variant, carried into the child.
    Parent,
}

impl TaskVariant {
    fn from_input(id: String) -> Self {
        Self {
            id,
            source: TaskVariantSource::Input,
        }
    }

    fn from_parent(id: String) -> Self {
        Self {
            id,
            source: TaskVariantSource::Parent,
        }
    }

    /// Why this variant cannot be used, naming both the source the model
    /// can act on and the variants the model does take — a rejection
    /// without the list is one it can only fix by guessing.
    fn rejection(&self, model: &str) -> String {
        let source = match self.source {
            TaskVariantSource::Input => "from the task's reasoning input",
            TaskVariantSource::Parent => "inherited from parent",
        };
        let options = match crate::model::find(model) {
            Some(info) if !info.variants().is_empty() => format!(
                "this model's variants: {}",
                info.variants()
                    .iter()
                    .map(|variant| variant.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Some(_) => "this model takes no reasoning variants; omit reasoning".to_string(),
            None => "this model is not in the catalog; omit reasoning".to_string(),
        };
        format!(
            "unsupported reasoning {:?} for {model} ({source}) — {options}",
            self.id
        )
    }
}

/// A task's workspace lease, or why the task never started.
enum LeaseOutcome {
    Acquired(Arc<crate::tools::WorkspaceLease>),
    Cancelled,
    Failed(LeaseFailure),
}

enum LeaseFailure {
    /// The target workspace is held by another task right now.
    Busy,
    /// What the lease covers is no longer the workspace that was
    /// validated; `Some` when deciding that failed outright.
    Changed(Option<anyhow::Error>),
}

impl LeaseFailure {
    /// Why the task could not start, in one wording for every caller.
    fn message(&self) -> String {
        match self {
            Self::Busy => "target workspace is busy; retry after the current task finishes".into(),
            Self::Changed(None) => "workspace changed while waiting for its lease".into(),
            Self::Changed(Some(error)) => {
                format!("workspace changed while waiting for its lease: {error:#}")
            }
        }
    }
}

/// Take the lease a child will run under, then confirm its workspace
/// survived the wait. An inherited lease already covers the child; a
/// nested task reaching into another workspace only tries for it, since
/// blocking there is how two tasks deadlock on each other's checkouts.
///
/// Both task paths funnel through here so the checks stay in step; they
/// differ only in how they phrase the outcome. `cancel` ends the wait
/// for the lease; a caller that also wants the revalidation abandoned
/// races this whole call against its own token.
async fn acquire_task_lease(
    workspace: &crate::tools::WorkspaceScheduler,
    access: WorkspaceAccess,
    inherited: Option<Arc<crate::tools::WorkspaceLease>>,
    cross_workspace_nested: bool,
    parent_location: &crate::tools::WorkspaceLocation,
    location: &crate::tools::WorkspaceLocation,
    cancel: &tokio_util::sync::CancellationToken,
) -> LeaseOutcome {
    let lease = match inherited {
        Some(lease) => lease,
        None if cross_workspace_nested => match workspace.try_acquire_lease(access) {
            Some(lease) => lease,
            None => return LeaseOutcome::Failed(LeaseFailure::Busy),
        },
        None => tokio::select! {
            lease = workspace.acquire_lease(access) => lease,
            () = cancel.cancelled() => return LeaseOutcome::Cancelled,
        },
    };
    match revalidate_after_lease(parent_location, location).await {
        Ok(revalidated) if &revalidated == location => LeaseOutcome::Acquired(lease),
        Ok(_) => LeaseOutcome::Failed(LeaseFailure::Changed(None)),
        Err(error) => LeaseOutcome::Failed(LeaseFailure::Changed(Some(error))),
    }
}

/// How a child's turn ended, in the terms both task paths share. The
/// foreground path phrases it as a tool result and the background one as
/// a notification; neither re-derives it.
enum TaskOutcome {
    Completed,
    Aborted,
    MaxIterations,
    Failed(anyhow::Error),
    /// Background only: a cancellation token stopped the run.
    Cancelled,
    /// Background only: the stall watchdog fired.
    Stalled,
}

impl TaskOutcome {
    fn from_turn(result: anyhow::Result<TurnOutcome>) -> Self {
        match result {
            Ok(TurnOutcome::Completed) => Self::Completed,
            Ok(TurnOutcome::Aborted) => Self::Aborted,
            Ok(TurnOutcome::MaxIterations) => Self::MaxIterations,
            Err(error) => Self::Failed(error),
        }
    }

    /// What the terminal activity event carries: anything that is not a
    /// clean finish or an iteration limit reads as an abort.
    fn activity(&self) -> TurnOutcome {
        match self {
            Self::Completed => TurnOutcome::Completed,
            Self::MaxIterations => TurnOutcome::MaxIterations,
            _ => TurnOutcome::Aborted,
        }
    }
}

/// Fans a child's loop events out to whoever is watching this
/// delegation, tagged with the call that started it.
#[derive(Clone)]
struct ActivityPublisher {
    tx: tokio::sync::broadcast::Sender<SubagentActivity>,
    parent_session_id: String,
    parent_call_id: String,
    child_session_id: String,
}

impl ActivityPublisher {
    fn publish(&self, event: LoopEvent) {
        let _ = self.tx.send(SubagentActivity {
            parent_session_id: self.parent_session_id.clone(),
            parent_call_id: self.parent_call_id.clone(),
            child_session_id: self.child_session_id.clone(),
            event,
        });
    }

    fn turn_done(&self, outcome: TurnOutcome) {
        self.publish(LoopEvent::TurnDone { outcome });
    }
}

/// A background task's word to its parent, in the envelope the parent
/// loop unwraps.
fn task_notification(
    parent_session_id: &str,
    description: &str,
    body: &str,
    is_error: bool,
) -> Notification {
    Notification {
        parent_session_id: parent_session_id.to_string(),
        description: description.to_string(),
        text: format!("<task-notification>\n{body}\n</task-notification>"),
        is_error,
    }
}

/// The one way a background task reports that it was stopped — it is
/// reachable from the lease wait, the revalidation and the run itself.
fn cancelled_task_notification(parent_session_id: &str, description: &str) -> Notification {
    task_notification(
        parent_session_id,
        description,
        &format!("Task \"{description}\" was cancelled."),
        true,
    )
}

/// Undo a session that was created moments ago but could not be
/// initialized. The id is about to be forgotten, so nothing of it may
/// stay on disk — its log, its lock and its replay index all go, which
/// is exactly what `delete` does under the lease it takes itself. The
/// caller must have dropped the session first.
fn rollback_created_session(store: &SessionStore, id: &str) {
    let _ = store.delete(id);
}

fn publish_reserved_notification(
    permit: &mut Option<tokio::sync::mpsc::OwnedPermit<Notification>>,
    notification: Notification,
) {
    if let Some(permit) = permit.take() {
        let _ = permit.send(notification);
    }
}

fn final_assistant_text(store: &SessionStore, session_id: &str) -> Option<String> {
    store.load(session_id).ok().and_then(|session| {
        let boundary = session
            .events()
            .iter()
            .rposition(|event| matches!(event, crate::session::SessionEvent::UserMessage { .. }))?;
        session
            .events()
            .iter()
            .skip(boundary + 1)
            .rev()
            .find_map(|event| match event {
                crate::session::SessionEvent::AssistantMessage { content, .. } => {
                    let text = content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                        .trim()
                        .to_string();
                    (!text.is_empty()).then_some(text)
                }
                _ => None,
            })
    })
}

fn discarded_event_sender() -> LoopEventSender {
    let (sender, receiver) = loop_event_channel(LOOP_EVENT_CAPACITY);
    drop(receiver);
    sender
}

struct ActiveSessionGuard {
    session_id: String,
    active: Arc<Mutex<std::collections::HashSet<String>>>,
    changed: tokio::sync::watch::Sender<u64>,
}

impl Drop for ActiveSessionGuard {
    fn drop(&mut self) {
        self.active.lock().unwrap().remove(&self.session_id);
        self.changed.send_modify(|version| {
            *version = version.wrapping_add(1);
        });
    }
}

struct BackgroundTaskGuard {
    id: String,
    registry: Arc<Mutex<BackgroundRegistry>>,
}

impl Drop for BackgroundTaskGuard {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap()
            .tasks
            .retain(|task| task.id != self.id);
    }
}

struct SlotGuard(Arc<AtomicUsize>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskInput {
    pub description: String,
    pub prompt: String,
    pub subagent_type: String,
    #[serde(default, deserialize_with = "deserialize_optional_text")]
    pub task_id: Option<String>,
    /// Run detached; completion arrives as a notification.
    #[serde(default)]
    pub background: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_workspace")]
    pub workspace: Option<TaskWorkspaceInput>,
    /// Model override for this task; omit to use the agent definition's
    /// model or inherit the parent's.
    #[serde(default, deserialize_with = "deserialize_optional_text")]
    pub model: Option<String>,
    /// Reasoning variant for the chosen model; omit for its default.
    #[serde(default, deserialize_with = "deserialize_optional_text")]
    pub reasoning: Option<String>,
}

/// Whether an optional field was left unfilled rather than answered.
///
/// GLM-5.3 writes the *string* "null" into optional task fields it means
/// to skip — `"task_id": "null"`, `"model": "null"`, `"reasoning":
/// "null"` — and taking that literally cost three failed round trips in
/// one session: resuming task "null", validating variant "null". Blank
/// strings arrive the same way. Both mean "not set", so every optional
/// field of `TaskInput` goes through here. Required fields deliberately
/// do not: a prompt of "null" is a prompt.
fn is_unfilled(text: &str) -> bool {
    let text = text.trim();
    text.is_empty() || text == "null"
}

fn deserialize_optional_text<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.filter(|text| !is_unfilled(text)))
}

/// The same quirk for the one optional field that is an object: a string
/// where a workspace belongs is the model writing "null". Anything else
/// is decoded as a workspace, so a genuinely malformed one still gets
/// its own error rather than a mismatched-variant one.
fn deserialize_optional_workspace<'de, D>(
    deserializer: D,
) -> Result<Option<TaskWorkspaceInput>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    if let Some(text) = value.as_str()
        && is_unfilled(text)
    {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWorkspaceInput {
    pub cwd: std::path::PathBuf,
    pub isolation: TaskWorkspaceIsolation,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskWorkspaceIsolation {
    GitWorktree,
}

/// The task tool is concurrency-safe within a provider step and manages a
/// child-lifetime workspace claim itself.
pub struct TaskTool {
    spawner: Arc<SubagentSpawner>,
}

impl TaskTool {
    pub fn new(spawner: Arc<SubagentSpawner>) -> Self {
        Self { spawner }
    }
}

impl Tool for TaskTool {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        "Delegate one clearly bounded unit of work. Delegation transfers ownership: do not perform the delegated scope yourself. Independent reviews must be explicitly delegated as separate bounded review tasks. Prefer an agent marked read-only for repository inspection and review so sibling tasks can run concurrently; use a mutable agent only when edits or mutating tools are required. Omit background when the result is needed for the current answer; foreground sibling tasks can be called together for parallel work. Use background only for intentionally deferred work that should trigger a separate follow-up turn."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::Mutating
    }

    fn manages_workspace_access(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        let agents = self
            .spawner
            .agents()
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>();
        let agent_guidance = self
            .spawner
            .agents()
            .iter()
            .map(|agent| {
                let mode = match agent.workspace_mode {
                    AgentWorkspaceMode::Mutable => "mutable",
                    AgentWorkspaceMode::ReadOnly => "read-only",
                };
                match &agent.tools {
                    Some(tools) => format!(
                        "{} ({mode}, tools: {}): {}",
                        agent.name,
                        tools.join("/"),
                        agent.description
                    ),
                    None => format!("{} ({mode}): {}", agent.name, agent.description),
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {"type": "string", "description": "Short task description (3-5 words)"},
                "prompt": {"type": "string", "description": "Full instructions for one bounded scope. The parent should continue only clearly disjoint work."},
                "subagent_type": {
                    "type": "string",
                    "enum": agents,
                    "description": format!("Configured agent to run. Available agents: {agent_guidance}. Prefer an agent marked read-only for repository review and parallel inspection; use a mutable agent only when edits or mutating tools are required.")
                },
                "task_id": {
                    "type": ["string", "null"],
                    "description": "Existing task session UUID to resume, replaying that task's full context — prefer it over a fresh task for follow-up questions on the same scope. Use an id reported by a task result, a task-notification, or the tasks tool; set null or omit to start a new task, and never invent a value."
                },
                "model": {
                    "type": ["string", "null"],
                    "description": "Model override for this task (provider/model-id). Omit to inherit. Call the models tool to see options with pricing — prefer a cheap/fast model for mechanical work."
                },
                "reasoning": {
                    "type": ["string", "null"],
                    "description": "Reasoning variant for the chosen model (see the models tool). Omit for the model's default."
                },
                "background": {"type": "boolean", "description": "Run detached only for intentionally deferred work whose completion should trigger a separate follow-up turn. Do not use when the result is needed for the current answer; call foreground sibling tasks together for parallel current-answer work. Do not poll."}
                ,"workspace": {
                    "type": ["object", "null"],
                    "description": "Validated sibling Git worktree for isolation. Set null or omit to use the current workspace. This is a cooperative scheduling domain, not a sandbox.",
                    "properties": {
                        "cwd": {"type": "string"},
                        "isolation": {"type": "string", "enum": ["git_worktree"]}
                    },
                    "required": ["cwd", "isolation"],
                    "additionalProperties": false
                }
            },
            "required": ["description", "prompt", "subagent_type"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            let input: TaskInput = match serde_json::from_value(input) {
                Ok(v) => v,
                Err(e) => return ToolOutput::error(format!("invalid input for task: {e}")),
            };
            spawner.run_task(input, &ctx).await
        })
    }

    fn run_observed(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
        on_start: ToolStartObserver,
    ) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            let input: TaskInput = match serde_json::from_value(input) {
                Ok(v) => v,
                Err(e) => return ToolOutput::error(format!("invalid input for task: {e}")),
            };
            spawner.run_task_observed(input, &ctx, Some(on_start)).await
        })
    }
}

/// How many tasks the listing reports, newest first. A long session
/// can accumulate dozens; the recent ones are the resumable ones.
const TASK_LISTING_LIMIT: usize = 20;
/// Display width of a task's last reply in the listing.
const TASK_SNIPPET_CHARS: usize = 200;

fn snippet(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > limit {
        let mut short: String = collapsed.chars().take(limit).collect();
        short.push('…');
        short
    } else {
        collapsed
    }
}

fn age_label(modified: std::time::SystemTime) -> String {
    let Ok(elapsed) = modified.elapsed() else {
        return "just now".to_string();
    };
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// tasks: read-only listing of the subagent tasks this session has
/// spawned, so the model can see what it delegated and resume one with
/// the task tool instead of re-explaining a scope to a fresh agent.
pub struct TasksTool {
    spawner: Arc<SubagentSpawner>,
}

impl TasksTool {
    pub fn new(spawner: Arc<SubagentSpawner>) -> Self {
        Self { spawner }
    }
}

impl Tool for TasksTool {
    fn name(&self) -> &'static str {
        "tasks"
    }

    fn description(&self) -> &'static str {
        "List the subagent tasks this session has spawned: id, agent, \
         model, whether one is still running, and a snippet of what it \
         last said. Pass an id back as the task tool's task_id to ask a \
         finished task a follow-up question with its context intact."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn run(&self, _input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            let children = spawner.store.children_of(&ctx.session_id);
            if children.is_empty() {
                return ToolOutput::text("no tasks spawned from this session yet");
            }
            let total = children.len();
            let mut lines = children
                .into_iter()
                .take(TASK_LISTING_LIMIT)
                .map(|child| {
                    let running = spawner.session_is_active(&child.id);
                    let status = if running { "running" } else { "finished" };
                    let prompt = child.title.as_deref().unwrap_or("(no prompt)");
                    let last = if running {
                        // Its final text is not final yet.
                        String::new()
                    } else {
                        match final_assistant_text(&spawner.store, &child.id) {
                            Some(text) => {
                                format!("\n  last: {}", snippet(&text, TASK_SNIPPET_CHARS))
                            }
                            None => String::new(),
                        }
                    };
                    format!(
                        "{} · {} · {} · {status} · {}\n  task: {}{last}",
                        child.id,
                        child.agent,
                        child.model,
                        age_label(child.modified),
                        snippet(prompt, TASK_SNIPPET_CHARS),
                    )
                })
                .collect::<Vec<_>>();
            if total > TASK_LISTING_LIMIT {
                lines.push(format!(
                    "({} older tasks not shown)",
                    total - TASK_LISTING_LIMIT
                ));
            }
            ToolOutput::text(lines.join("\n"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(description: &str) -> Notification {
        Notification {
            parent_session_id: "parent".into(),
            description: description.into(),
            text: description.into(),
            is_error: false,
        }
    }

    #[tokio::test]
    async fn notification_capacity_is_reserved_before_background_admission() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let permit = sender.clone().try_reserve_owned().unwrap();
        assert!(matches!(
            sender.clone().try_reserve_owned(),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));

        let mut permit = Some(permit);
        publish_reserved_notification(&mut permit, notification("first"));
        assert_eq!(receiver.recv().await.unwrap().description, "first");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn rolling_back_a_created_session_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let id = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: id.clone(),
                    parent_id: Some("parent".into()),
                    agent: "explore".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                })
                .unwrap(),
        );

        rollback_created_session(&store, &id);

        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "rollback left {leftovers:?}");
    }

    #[test]
    fn nested_context_failure_propagates_to_the_grandparent() {
        let meta = SessionMeta {
            session_id: "parent".into(),
            parent_id: Some("grandparent".into()),
            agent: "build".into(),
            model: "zai/glm-4.7".into(),
            workspace: None,
        };

        let outcome = context_route_failure(
            &meta,
            notification("nested"),
            anyhow::anyhow!("bad AGENTS.md"),
        )
        .unwrap();

        let RouteOutcome::Propagate(notification) = outcome else {
            panic!("expected propagated failure");
        };
        assert_eq!(notification.parent_session_id, "grandparent");
        assert!(notification.is_error);
        assert!(notification.text.contains("bad AGENTS.md"));
    }
}
