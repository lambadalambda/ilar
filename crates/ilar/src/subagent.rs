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
            notify_tx,
            activity_tx,
            stall_timeout: std::time::Duration::from_secs(600),
            background_tasks: Arc::new(Mutex::new(BackgroundRegistry::default())),
            workspace,
            background_tool_timeout: std::time::Duration::from_secs(600),
            loop_config: LoopConfig::default(),
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

    /// Abort every detached background task (Ctrl-C path).
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
        for task in tasks {
            let _ = task.await;
        }
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

    /// Spawner for children of this spawner: one level deeper, shared slot
    /// counter and notification channel.
    fn child_spawner(
        self: &Arc<Self>,
        workspace_location: crate::tools::WorkspaceLocation,
        workspace: crate::tools::WorkspaceScheduler,
    ) -> Arc<Self> {
        Arc::new(Self {
            resolver: self.resolver.clone(),
            store: self.store.clone(),
            agents: self.agents.clone(),
            user_config_dir: self.user_config_dir.clone(),
            workspace_location,
            depth: self.depth + 1,
            max_concurrent: self.max_concurrent,
            max_depth: self.max_depth,
            running: self.running.clone(),
            active_sessions: self.active_sessions.clone(),
            active_sessions_changed: self.active_sessions_changed.clone(),
            notify_tx: self.notify_tx.clone(),
            notify_rx: self.notify_rx.clone(),
            activity_tx: self.activity_tx.clone(),
            stall_timeout: self.stall_timeout,
            background_tasks: self.background_tasks.clone(),
            workspace,
            background_tool_timeout: self.background_tool_timeout,
            loop_config: self.loop_config.clone(),
        })
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
        let mut system_prompt = match system_prompt_for(&self.user_config_dir, child_location.cwd())
        {
            Ok(prompt) => prompt,
            Err(error) => {
                return ToolOutput::error(format!("loading subagent context: {error:#}"));
            }
        };
        if !agent.prompt.is_empty() {
            system_prompt = format!(
                "{system_prompt}\n\n# Agent: {}\n\n{}",
                agent.name, agent.prompt
            );
        }

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
                let (model, inherited_variant) = match &agent.model {
                    Some(model) => (model.clone(), None),
                    None => match self.store.load(&ctx.session_id) {
                        Ok(parent) => (parent.effective_model(), parent.effective_variant()),
                        Err(error) => {
                            return ToolOutput::error(format!(
                                "loading parent session {:?}: {error}",
                                ctx.session_id
                            ));
                        }
                    },
                };
                if let Err(error) =
                    crate::model::variant_options(&model, inherited_variant.as_deref())
                {
                    return ToolOutput::error(format!(
                        "validating inherited subagent reasoning variant: {error}"
                    ));
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
                        let inherited_model = session.effective_model();
                        if let Some(variant) = inherited_variant
                            && let Err(error) =
                                session.append(crate::session::SessionEvent::ModelChange {
                                    id: new_id(),
                                    model: inherited_model,
                                    variant: Some(variant),
                                    ts: chrono::Utc::now(),
                                })
                        {
                            drop(session);
                            if let Ok(path) = self.store.session_path(&id) {
                                let _ = std::fs::remove_file(path);
                            }
                            return ToolOutput::error(format!(
                                "persisting inherited subagent reasoning variant: {error}"
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
        let parent_call_id = ctx.call_id.clone().unwrap_or_default();

        let child_spawner = self.child_spawner(child_location.clone(), child_workspace.clone());
        let registry = match agent.workspace_mode {
            AgentWorkspaceMode::ReadOnly => ToolRegistry::read_only(),
            AgentWorkspaceMode::Mutable => {
                match ToolRegistry::builtin().with_subagents(child_spawner.clone()) {
                    Ok(registry) => registry,
                    Err(error) => {
                        return ToolOutput::error(format!("building child tool registry: {error}"));
                    }
                }
            }
        };
        let registry = match &agent.tools {
            Some(tools) => registry.restricted_to(tools),
            None => registry,
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
            let background_cancel = tokio_util::sync::CancellationToken::new();
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
            let activity_tx = self.activity_tx.clone();
            let activity_parent_session_id = parent_session_id.clone();
            let activity_call_id = parent_call_id.clone();
            let activity_session_id = session_id.clone();
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
                let _slot = _guard; // hold the concurrency slot for the run
                let lease = match inherited_lease {
                    Some(lease) => lease,
                    None if cross_workspace_nested => {
                        let Some(lease) = workspace.try_acquire_lease(workspace_access) else {
                            publish_reserved_notification(
                                &mut notification_permit,
                                Notification {
                                    parent_session_id,
                                    description: description.clone(),
                                    text: format!(
                                        "<task-notification>\nTask \"{description}\" failed: target workspace is busy; retry after the current task finishes\n</task-notification>"
                                    ),
                                    is_error: true,
                                },
                            );
                            return;
                        };
                        lease
                    }
                    None => {
                        tokio::select! {
                            lease = workspace.acquire_lease(workspace_access) => lease,
                            () = task_cancel.cancelled() => {
                                publish_reserved_notification(&mut notification_permit, Notification {
                                    parent_session_id,
                                    description: description.clone(),
                                    text: format!("<task-notification>\nTask \"{description}\" was cancelled.\n</task-notification>"),
                                    is_error: true,
                                });
                                return;
                            }
                            () = root_cancel.cancelled() => {
                                publish_reserved_notification(&mut notification_permit, Notification {
                                    parent_session_id,
                                    description: description.clone(),
                                    text: format!("<task-notification>\nTask \"{description}\" was cancelled.\n</task-notification>"),
                                    is_error: true,
                                });
                                return;
                            }
                        }
                    }
                };
                let revalidated = tokio::select! {
                    result = revalidate_after_lease(&parent_location, &leased_location) => result,
                    () = task_cancel.cancelled() => {
                        publish_reserved_notification(&mut notification_permit, Notification {
                            parent_session_id,
                            description: description.clone(),
                            text: format!("<task-notification>\nTask \"{description}\" was cancelled.\n</task-notification>"),
                            is_error: true,
                        });
                        return;
                    }
                    () = root_cancel.cancelled() => {
                        publish_reserved_notification(&mut notification_permit, Notification {
                            parent_session_id,
                            description: description.clone(),
                            text: format!("<task-notification>\nTask \"{description}\" was cancelled.\n</task-notification>"),
                            is_error: true,
                        });
                        return;
                    }
                };
                if let Err(error) = revalidated.as_ref() {
                    publish_reserved_notification(
                        &mut notification_permit,
                        Notification {
                            parent_session_id,
                            description: description.clone(),
                            text: format!(
                                "<task-notification>\nTask \"{description}\" failed: workspace changed while waiting for its lease: {error:#}\n</task-notification>"
                            ),
                            is_error: true,
                        },
                    );
                    return;
                }
                if revalidated
                    .as_ref()
                    .is_ok_and(|location| location != &leased_location)
                {
                    publish_reserved_notification(
                        &mut notification_permit,
                        Notification {
                            parent_session_id,
                            description: description.clone(),
                            text: format!(
                                "<task-notification>\nTask \"{description}\" failed: workspace changed while waiting for its lease\n</task-notification>"
                            ),
                            is_error: true,
                        },
                    );
                    return;
                }
                child_ctx.workspace_lease = Some(lease);
                let cancel = root_cancel.child_token();
                let (tx, mut rx_evt) = loop_event_channel(LOOP_EVENT_CAPACITY);
                // Activity tracker: any child event counts as progress.
                let last_activity = Arc::new(Mutex::new(std::time::Instant::now()));
                let watcher_last = last_activity.clone();
                let watcher = tokio::spawn(async move {
                    while let Some(event) = rx_evt.recv().await {
                        *watcher_last.lock().unwrap() = std::time::Instant::now();
                        let _ = activity_tx.send(SubagentActivity {
                            parent_session_id: activity_parent_session_id.clone(),
                            parent_call_id: activity_call_id.clone(),
                            child_session_id: activity_session_id.clone(),
                            event,
                        });
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
                let turn = run_turn(
                    spawner.resolver.as_ref(),
                    &registry,
                    &spawner.store,
                    &session_id,
                    &prompt,
                    Some(&system_prompt),
                    spawner.loop_config.clone(),
                    tx,
                    cancel.clone(),
                    child_ctx,
                );
                let (outcome, was_cancelled) = tokio::select! {
                    outcome = turn => (Some(outcome), false),
                    () = stall_watch => {
                        cancel.cancel();
                        (None, false)
                    }
                    () = task_cancel.cancelled() => {
                        cancel.cancel();
                        (None, true)
                    }
                    () = root_cancel.cancelled() => {
                        cancel.cancel();
                        (None, true)
                    }
                };
                let _ = watcher.await;
                let activity_outcome = match &outcome {
                    Some(Ok(outcome)) => *outcome,
                    _ => TurnOutcome::Aborted,
                };
                let _ = spawner.activity_tx.send(SubagentActivity {
                    parent_session_id: parent_session_id.clone(),
                    parent_call_id: parent_call_id.clone(),
                    child_session_id: session_id.clone(),
                    event: LoopEvent::TurnDone {
                        outcome: activity_outcome,
                    },
                });

                let notification = match outcome {
                    Some(Ok(TurnOutcome::Completed)) => {
                        let text = final_assistant_text(&spawner.store, &session_id)
                            .unwrap_or_else(|| "(finished with no text)".into());
                        Notification {
                            parent_session_id,
                            description: description.clone(),
                            text: format!(
                                "<task-notification>\nTask \"{description}\" completed.\n<result>\n{text}\n</result>\n</task-notification>"
                            ),
                            is_error: false,
                        }
                    }
                    Some(Ok(TurnOutcome::Aborted)) => Notification {
                        parent_session_id,
                        description: description.clone(),
                        text: format!(
                            "<task-notification>\nTask \"{description}\" was aborted.\n</task-notification>"
                        ),
                        is_error: true,
                    },
                    Some(Ok(TurnOutcome::MaxIterations)) => Notification {
                        parent_session_id,
                        description: description.clone(),
                        text: format!(
                            "<task-notification>\nTask \"{description}\" failed: subagent reached its iteration limit.\n</task-notification>"
                        ),
                        is_error: true,
                    },
                    Some(Err(e)) => Notification {
                        parent_session_id,
                        description: description.clone(),
                        text: format!(
                            "<task-notification>\nTask \"{description}\" failed: {e:#}\n</task-notification>"
                        ),
                        is_error: true,
                    },
                    None if was_cancelled => Notification {
                        parent_session_id,
                        description: description.clone(),
                        text: format!(
                            "<task-notification>\nTask \"{description}\" was cancelled.\n</task-notification>"
                        ),
                        is_error: true,
                    },
                    None => Notification {
                        parent_session_id,
                        description: description.clone(),
                        text: format!(
                            "<task-notification>\nTask \"{description}\" stalled: no progress for {}s. It has been stopped.\n</task-notification>",
                            stall_timeout.as_secs()
                        ),
                        is_error: true,
                    },
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
            return ToolOutput::text(
                "Deferred background task started. Completion will trigger a separate follow-up turn. \
Do not sleep, poll, or check on it. Do not perform this task's scope yourself; continue only \
clearly disjoint work."
                    .to_string(),
            )
            .with_child_session(returned_session_id);
        }

        let lease = match inherited_lease {
            Some(lease) => lease,
            None if cross_workspace_nested => {
                let Some(lease) = child_workspace.try_acquire_lease(workspace_access) else {
                    return ToolOutput::error(
                        "target workspace is busy; retry after the current task finishes",
                    );
                };
                lease
            }
            None => {
                tokio::select! {
                    lease = child_workspace.acquire_lease(workspace_access) => lease,
                    () = ctx.cancel.cancelled() => {
                        return ToolOutput::error("subagent cancelled while waiting for workspace");
                    }
                }
            }
        };
        match revalidate_after_lease(&ctx.location, &child_location).await {
            Ok(location) if location == child_location => {}
            Ok(_) => return ToolOutput::error("workspace changed while waiting for its lease"),
            Err(error) => {
                return ToolOutput::error(format!(
                    "workspace changed while waiting for its lease: {error:#}"
                ));
            }
        }
        child_ctx.workspace_lease = Some(lease);
        if let Some(on_start) = on_start.take() {
            on_start();
        }
        let (tx, mut rx_evt) = loop_event_channel(LOOP_EVENT_CAPACITY);
        let activity_tx = self.activity_tx.clone();
        let activity_parent_session_id = ctx.session_id.clone();
        let activity_call_id = parent_call_id;
        let activity_session_id = session_id.clone();
        let turn = run_turn(
            self.resolver.as_ref(),
            &registry,
            &self.store,
            &session_id,
            &input.prompt,
            Some(&system_prompt),
            self.loop_config.clone(),
            tx,
            ctx.cancel.clone(),
            child_ctx,
        );
        tokio::pin!(turn);
        let outcome = loop {
            tokio::select! {
                event = rx_evt.recv() => {
                    if let Some(event) = event {
                        let _ = activity_tx.send(SubagentActivity {
                            parent_session_id: activity_parent_session_id.clone(),
                            parent_call_id: activity_call_id.clone(),
                            child_session_id: activity_session_id.clone(),
                            event,
                        });
                    }
                }
                outcome = &mut turn => break outcome,
            }
        };
        while let Ok(event) = rx_evt.try_recv() {
            let _ = activity_tx.send(SubagentActivity {
                parent_session_id: activity_parent_session_id.clone(),
                parent_call_id: activity_call_id.clone(),
                child_session_id: activity_session_id.clone(),
                event,
            });
        }
        let activity_outcome = match &outcome {
            Ok(outcome) => *outcome,
            Err(_) => TurnOutcome::Aborted,
        };
        let _ = activity_tx.send(SubagentActivity {
            parent_session_id: activity_parent_session_id,
            parent_call_id: activity_call_id,
            child_session_id: activity_session_id,
            event: LoopEvent::TurnDone {
                outcome: activity_outcome,
            },
        });

        let output = match outcome {
            Ok(TurnOutcome::Completed) => {
                let text = final_assistant_text(&self.store, &session_id)
                    .unwrap_or_else(|| "(subagent finished with no text)".into());
                ToolOutput::text(text)
            }
            Ok(TurnOutcome::Aborted) => ToolOutput::error("subagent aborted"),
            Ok(TurnOutcome::MaxIterations) => {
                ToolOutput::error("subagent failed: iteration limit reached")
            }
            Err(e) => ToolOutput::error(format!("subagent failed: {e:#}")),
        };
        output.with_child_session(session_id.clone())
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
        let runtime = Arc::new(Self {
            resolver: self.resolver.clone(),
            store: self.store.clone(),
            agents: self.agents.clone(),
            user_config_dir: self.user_config_dir.clone(),
            workspace_location: workspace_location.clone(),
            depth,
            max_concurrent: self.max_concurrent,
            max_depth: self.max_depth,
            running: self.running.clone(),
            active_sessions: self.active_sessions.clone(),
            active_sessions_changed: self.active_sessions_changed.clone(),
            notify_tx: self.notify_tx.clone(),
            notify_rx: self.notify_rx.clone(),
            activity_tx: self.activity_tx.clone(),
            stall_timeout: self.stall_timeout,
            background_tasks: self.background_tasks.clone(),
            workspace: workspace.clone(),
            background_tool_timeout: self.background_tool_timeout,
            loop_config: self.loop_config.clone(),
        });
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
        let registry = match agent.workspace_mode {
            AgentWorkspaceMode::ReadOnly => ToolRegistry::read_only(),
            AgentWorkspaceMode::Mutable => {
                ToolRegistry::builtin().with_subagents(runtime.clone())?
            }
        };
        let registry = match &agent.tools {
            Some(tools) => registry.restricted_to(tools),
            None => registry,
        };
        let mut system_prompt =
            match system_prompt_for(&self.user_config_dir, workspace_location.cwd()) {
                Ok(prompt) => prompt,
                Err(error) => return context_route_failure(&meta, notification, error),
            };
        if !agent.prompt.is_empty() {
            system_prompt = format!(
                "{system_prompt}\n\n# Agent: {}\n\n{}",
                agent.name, agent.prompt
            );
        }
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
            )
            .await;
            match result {
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
                {
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(RouteOutcome::Requeue(notification)),
                        () = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
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
    #[serde(default, deserialize_with = "deserialize_task_id")]
    pub task_id: Option<String>,
    /// Run detached; completion arrives as a notification.
    #[serde(default)]
    pub background: Option<bool>,
    #[serde(default)]
    pub workspace: Option<TaskWorkspaceInput>,
}

fn deserialize_task_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.filter(|task_id| !task_id.trim().is_empty()))
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
                    "description": "Existing task session UUID to resume. Set null or omit when starting a new task; never invent a value."
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
