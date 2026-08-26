//! Task tool + subagent spawner — see meta/issues/task-tool-subagents.md.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::agent::{
    LOOP_EVENT_CAPACITY, LoopConfig, LoopEvent, LoopEventSender, TurnOutcome, loop_event_channel,
    run_turn,
};
use crate::config::{AgentDefinition, AgentWorkspaceMode, ProjectInstructions, system_prompt_for};
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
/// The one wording for "make a worktree and name it here". Both refusals
/// that send the model there quote it, and so does the schema: a
/// corrective that drifts between sites is one the model has to learn
/// again at every site.
const WORKTREE_CORRECTION: &str = "`git worktree add ../ilar-task-<name> -b task/<name>`, then \
     pass \"workspace\": {\"cwd\": \"../ilar-task-<name>\", \"isolation\": \"git_worktree\"}";
/// Why a task that would have been detached ran in the turn instead.
/// Both cases are refusals for an explicit `background: true`, but a
/// default is ilar's choice rather than the caller's: demoting it keeps
/// the work happening, and the note keeps the result honest about which
/// path it took.
const BACKGROUND_DEMOTED_BY_CAPACITY: &str = "Ran in the foreground: read-only tasks default to \
     background, but background capacity was full, so this one ran here instead of failing.";
const BACKGROUND_DEMOTED_BY_LEASE: &str = "Ran in the foreground: read-only tasks default to \
     background, but a background task cannot outlive the workspace lease you hold, so this one \
     ran here instead of failing.";

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
    /// Whether the workspace's own context file is trusted for this
    /// launch; inherited from the session that owns the spawner.
    project_instructions: ProjectInstructions,
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
    /// What the parent has said to its children, keyed by child session.
    child_steers: ChildSteers,
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

/// What the parent has said to its children: the live steer channel of
/// every child turn that is running, and the messages sent but not yet
/// taken. Shared with every derived spawner, exactly like the running
/// registry, so a nested task is reachable from the spawner its own
/// parent holds.
#[derive(Clone, Default)]
struct ChildSteers(Arc<Mutex<std::collections::HashMap<String, ChildSteer>>>);

#[derive(Default)]
struct ChildSteer {
    /// The running turn's steer channel; `None` once that turn ended.
    sender: Option<crate::agent::SteerSender>,
    /// Messages the child has not been seen to take. While its turn runs
    /// they are in flight; once it ends they wait for its next resume —
    /// the root rule, where an undelivered steer moves to the queue
    /// instead of vanishing with the channel.
    pending: Vec<String>,
}

impl ChildSteers {
    /// The receiver a child's turn runs with, and the run's claim on
    /// everything that was waiting for it.
    fn open(&self, session_id: &str) -> (crate::agent::SteerReceiver, ChildTurnSteer) {
        let (sender, receiver) = crate::agent::steer_channel();
        (receiver, self.begin(session_id, Some(sender)))
    }

    /// The same claim for a turn that cannot be steered — a routed
    /// notification, which is nonetheless a resume of that session and
    /// so carries what the session never read.
    fn adopt(&self, session_id: &str) -> ChildTurnSteer {
        self.begin(session_id, None)
    }

    /// Install the turn's channel and take its queue in one step: with
    /// both under the same lock, a message either goes to the turn that
    /// is starting or into the prompt that turn starts from, and never
    /// falls between the two.
    fn begin(&self, session_id: &str, sender: Option<crate::agent::SteerSender>) -> ChildTurnSteer {
        let mut steers = self.0.lock().unwrap();
        let entry = steers.entry(session_id.to_string()).or_default();
        entry.sender = sender;
        let queued = std::mem::take(&mut entry.pending);
        drop(steers);
        ChildTurnSteer {
            session_id: session_id.to_string(),
            steers: self.clone(),
            queued,
        }
    }

    /// Hand a message to a running child's turn. False when no live
    /// channel took it: the caller then decides between resuming the
    /// task and holding the message for its next resume.
    fn steer(&self, session_id: &str, text: String) -> bool {
        let mut steers = self.0.lock().unwrap();
        let Some(entry) = steers.get_mut(session_id) else {
            return false;
        };
        let Some(sender) = entry.sender.as_ref() else {
            return false;
        };
        if sender.send(text.clone()).is_err() {
            return false;
        }
        entry.pending.push(text);
        true
    }

    /// Hold a message for a child that cannot be steered right now.
    fn queue(&self, session_id: &str, text: String) {
        self.0
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_default()
            .pending
            .push(text);
    }

    /// The child took this message at a step boundary, so it is waiting
    /// for nothing — matched by text, the way the root's pending strip
    /// clears itself from the same `Steered` event.
    fn delivered(&self, session_id: &str, text: &str) {
        let mut steers = self.0.lock().unwrap();
        if let Some(entry) = steers.get_mut(session_id)
            && let Some(index) = entry.pending.iter().position(|held| held == text)
        {
            entry.pending.remove(index);
        }
        Self::prune(&mut steers, session_id);
    }

    /// How many messages this task has not read yet.
    fn pending(&self, session_id: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .get(session_id)
            .map_or(0, |entry| entry.pending.len())
    }

    /// The turn is over: its channel is gone, and anything it took but
    /// never started goes back to the head of the queue, ahead of
    /// whatever was said while it was running.
    fn end(&self, session_id: &str, restored: Vec<String>) {
        let mut steers = self.0.lock().unwrap();
        if let Some(entry) = steers.get_mut(session_id) {
            entry.sender = None;
            entry.pending.splice(0..0, restored);
        }
        Self::prune(&mut steers, session_id);
    }

    /// A child with no channel and nothing waiting is not a child this
    /// map has anything to say about.
    fn prune(steers: &mut std::collections::HashMap<String, ChildSteer>, session_id: &str) {
        if steers
            .get(session_id)
            .is_some_and(|entry| entry.sender.is_none() && entry.pending.is_empty())
        {
            steers.remove(session_id);
        }
    }
}

/// One child turn's hold on its task's messages: the queue it starts
/// from while it is starting, and the channel it reads while it runs.
/// However it ends, the channel goes; a run that never got as far as its
/// turn puts the queue back, because a lease it could not take must not
/// swallow what the parent said.
struct ChildTurnSteer {
    session_id: String,
    steers: ChildSteers,
    queued: Vec<String>,
}

impl ChildTurnSteer {
    /// The prompt this run actually starts from: what the task never
    /// read, then what the parent is asking now.
    fn prompt(&self, prompt: &str) -> String {
        if self.queued.is_empty() {
            return prompt.to_string();
        }
        self.queued
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(prompt))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The turn is going ahead with that prompt: the messages in it are
    /// delivered, not waiting.
    fn started(&mut self) {
        self.queued.clear();
    }
}

impl Drop for ChildTurnSteer {
    fn drop(&mut self) {
        self.steers
            .end(&self.session_id, std::mem::take(&mut self.queued));
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
    /// `project_instructions` is stated, never defaulted: a refused
    /// project file must stay refused for the agents a session delegates
    /// to, and a constructor default would let a new call site silently
    /// hand back exactly what the launch declined.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        resolver: Arc<dyn ProviderResolver>,
        store: SessionStore,
        agents: Vec<AgentDefinition>,
        cwd: std::path::PathBuf,
        depth: usize,
        max_concurrent: usize,
        max_depth: usize,
        project_instructions: ProjectInstructions,
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
            project_instructions,
            workspace_location,
            depth,
            max_concurrent,
            max_depth,
            running: Arc::new(AtomicUsize::new(0)),
            active_sessions: Arc::new(Mutex::new(std::collections::HashSet::new())),
            active_sessions_changed,
            running_tasks: Arc::new(Mutex::new(Vec::new())),
            child_steers: ChildSteers::default(),
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

    /// Shrink the notification channel (tests): filling the real one
    /// would mean holding sixty-four children open at once, and what the
    /// capacity path does at its edge is worth testing cheaply.
    pub fn with_notification_capacity(mut self, capacity: usize) -> Self {
        let (notify_tx, notify_rx) = tokio::sync::mpsc::channel(capacity);
        self.notify_tx = notify_tx;
        self.notify_rx = Arc::new(Mutex::new(Some(notify_rx)));
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
            project_instructions: self.project_instructions,
            workspace_location,
            depth,
            max_concurrent: self.max_concurrent,
            max_depth: self.max_depth,
            running: self.running.clone(),
            active_sessions: self.active_sessions.clone(),
            active_sessions_changed: self.active_sessions_changed.clone(),
            running_tasks: self.running_tasks.clone(),
            child_steers: self.child_steers.clone(),
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
            system_prompt_for(&self.user_config_dir, cwd, self.project_instructions)?.prompt,
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
        // The one place `background` stops being a maybe. A read-only
        // task is independent by nature — no edits to merge, no write
        // lease to hand back — so omitting the flag means "detach and
        // tell me when it lands", which leaves the parent free to keep
        // working. A mutable task's edits are usually wanted before the
        // next call reads them, so its default stays in the turn.
        // Everything below sees a bool, so capacity and notification
        // wiring never have to ask what the caller meant.
        let background_explicit = input.background.is_some();
        let mut background = input.background.unwrap_or(match agent.workspace_mode {
            AgentWorkspaceMode::ReadOnly => true,
            AgentWorkspaceMode::Mutable => false,
        });
        let mut background_demoted: Option<&'static str> = None;
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
                        return ToolOutput::error(format!(
                            "invalid task workspace {:?}: {error:#}. workspace.cwd must already be \
                             a registered Git worktree of this repository: {WORKTREE_CORRECTION}. \
                             Outside a Git repository no workspace is valid: omit workspace to run \
                             in the current checkout.",
                            workspace.cwd
                        ));
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
        if background && same_workspace && ctx.has_workspace_lease() {
            if background_explicit {
                return ToolOutput::error(
                    "background tasks cannot outlive a parent workspace lease; use a foreground task or validated worktree",
                );
            }
            background = false;
            background_demoted = Some(BACKGROUND_DEMOTED_BY_LEASE);
        }
        if workspace_access == WorkspaceAccess::Mutating
            && same_workspace
            && ctx.has_workspace_lease()
        {
            return ToolOutput::error(format!(
                "nested mutable tasks cannot reuse their parent checkout; this one is held for the \
                 whole of the task you are running in. Run it in a sibling worktree instead: \
                 {WORKTREE_CORRECTION}. If the task only needs to read, pass a read-only \
                 subagent_type and omit workspace."
            ));
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
        let notification_permit = if background {
            match self.notify_tx.clone().try_reserve_owned() {
                Ok(permit) => Some(permit),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) if background_explicit => {
                    return ToolOutput::error(
                        "background task capacity is full; retry after a notification is handled",
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    background = false;
                    background_demoted = Some(BACKGROUND_DEMOTED_BY_CAPACITY);
                    None
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
                    // Children are never listed, so there is no listing
                    // for a launch directory to group.
                    cwd: None,
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
            background,
            started: std::time::Instant::now(),
        });
        // The channel and the queue in one step, under the claim taken
        // above: from here a message reaches the turn that is starting,
        // and one that arrived before it heads that turn's prompt —
        // anything the parent said while the task's last turn was ending
        // never reached it, and the root rule is that such a steer waits
        // in the queue rather than vanishing. A task's queue is the
        // prompt of its next run, which this is.
        let (steer_rx, mut child_steer) = self.child_steers.open(&session_id);
        let prompt = child_steer.prompt(&input.prompt);
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
            // Not inherited from the parent: the child's turn sets this
            // from the child session's own model, which is the one that
            // will be looking at whatever its tools return.
            vision: false,
            // Empty, not the parent's: the child model has seen nothing
            // of the workspace, so its first edit reads the file itself.
            seen_files: crate::tools::SeenFiles::default(),
            // Inherited: a child's oversized output is worth keeping for
            // the same reason its parent's is.
            spill_dir: ctx.spill_dir.clone(),
        };

        if background {
            let notification_permit = notification_permit.expect("reserved for background task");
            // Detached: run the child on a spawned task with a stall
            // watchdog; completion lands as a notification for the parent
            // loop; the tool call returns immediately.
            let spawner = Arc::clone(self);
            let description = input.description.clone();
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
                steers: self.child_steers.clone(),
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
                // Declared with the other guards so it drops before
                // them: the channel is gone before the session can be
                // claimed again.
                let mut child_steer = child_steer;
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
                // The child's own token, not the root's: everything
                // inside this task stops when the task does. The turn
                // overrides this for the tools it runs, so the two agree
                // rather than one of them naming a token that outlives
                // the task.
                child_ctx.cancel = cancel.clone();
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
                // Nothing after this point declines to start, so the
                // queue folded into the prompt is delivered, not waiting.
                child_steer.started();
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
                    // Not a user: the parent, whose message reaches this
                    // child at the same step boundary a root steer does.
                    Some(steer_rx),
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
        child_steer.started();
        let activity = ActivityPublisher {
            tx: self.activity_tx.clone(),
            steers: self.child_steers.clone(),
            parent_session_id: ctx.session_id.clone(),
            parent_call_id,
            child_session_id: session_id.clone(),
        };
        let turn = run_turn(
            self.resolver.as_ref(),
            &registry,
            &self.store,
            &session_id,
            &prompt,
            &[],
            Some(&system_prompt),
            self.loop_config.clone(),
            tx,
            ctx.cancel.clone(),
            child_ctx,
            // Same channel as the background path: the parent of a
            // foreground task is blocked on it and cannot use it, but
            // the wiring is the child's, not the caller's.
            Some(steer_rx),
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
        // A default that could not be honoured says so: the schema
        // promised this task would be detached, and a silently blocking
        // call is exactly the surprise the promise was meant to remove.
        let output = match background_demoted {
            Some(note) => output.with_appended_text(&format!("\n\n({note})")),
            None => output,
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

    /// The one verb for talking to a task: steer it where it stands if
    /// its turn is running, resume it from its transcript if it has
    /// finished. The caller never has to know which case it is in, so
    /// every answer here says what actually happened to the message.
    pub async fn message_task(
        self: &Arc<Self>,
        input: TaskMessageInput,
        ctx: &ToolContext,
    ) -> ToolOutput {
        self.message_task_observed(input, ctx, None).await
    }

    async fn message_task_observed(
        self: &Arc<Self>,
        input: TaskMessageInput,
        ctx: &ToolContext,
        mut on_start: Option<ToolStartObserver>,
    ) -> ToolOutput {
        let text = input.message.trim().to_string();
        if text.is_empty() {
            return ToolOutput::error("message must not be empty");
        }
        let task_id = input.task_id.trim().to_string();
        // Only this session's own tasks: an id from somewhere else names
        // a conversation this session has no standing in, and the resume
        // path would refuse it a moment later anyway.
        let meta = match self.store.load(&task_id) {
            Ok(session) => match session.meta() {
                Some(meta) if meta.parent_id.as_deref() == Some(ctx.session_id.as_str()) => {
                    meta.clone()
                }
                Some(_) => {
                    return ToolOutput::error(format!(
                        "task {task_id:?} was not spawned by this session; the tasks tool lists the ones that were"
                    ));
                }
                None => {
                    return ToolOutput::error(format!("task {task_id:?} has no metadata"));
                }
            },
            Err(error) => {
                return ToolOutput::error(format!(
                    "unknown task {task_id:?}: {error}. Use an id from a task result, a \
                     task-notification, or the tasks tool, and never invent one."
                ));
            }
        };
        if self
            .running_tasks()
            .iter()
            .any(|task| task.session_id == task_id && !task.background)
        {
            return ToolOutput::error(format!(
                "task {task_id} is a foreground task of the turn you are in: you are blocked on \
                 its result, so nothing said now can reach it before it comes back. Messaging \
                 serves background tasks, which keep working while you do — start one with the \
                 task tool's background flag — and finished tasks, which this call resumes."
            ));
        }
        if self.child_steers.steer(&task_id, text.clone()) {
            if let Some(on_start) = on_start.take() {
                on_start();
            }
            // Not "delivered": the task takes it at its next step, and a
            // task that stops before reaching one leaves it waiting for
            // its resume. Saying more than that would have the model
            // count on a reading that may not happen.
            return ToolOutput::text(format!(
                "Message queued for running task {task_id}; it reaches that task at its next \
                 step, and waits for the task's next resume if the task stops before then. Its \
                 answer comes back the way that task's answers always do — as its result or its \
                 completion notification — so do not wait for a reply here and do not repeat the \
                 message."
            ))
            .with_child_session(task_id);
        }
        if self.session_is_active(&task_id) {
            // Running a turn this spawner did not start — a completion
            // routed to it. Holding the message is the undelivered rule:
            // that turn is already under way with its own prompt, so the
            // resume after it is the one that carries this.
            self.child_steers.queue(&task_id, text);
            if let Some(on_start) = on_start.take() {
                on_start();
            }
            return ToolOutput::text(format!(
                "Task {task_id} is busy with a completion of its own and has no live channel; \
                 your message is held and delivered at its next resume."
            ))
            .with_child_session(task_id);
        }
        // The one thing a resume needs that a message does not name: the
        // worktree the task ran in. It is in the task's own metadata, so
        // ask for it there rather than making the model know which case
        // it is in.
        let workspace = input
            .workspace
            .or_else(|| persisted_worktree(&meta, &ctx.location));
        self.run_task_observed(
            TaskInput {
                description: format!("message: {}", snippet(&text, TASK_MESSAGE_LABEL_CHARS)),
                prompt: text,
                subagent_type: meta.agent,
                task_id: Some(task_id),
                // Stated, not defaulted: this call promises to return
                // the task's answer, so it runs in the turn whatever the
                // agent's own default would have been.
                background: Some(false),
                workspace,
                model: None,
                reasoning: None,
            },
            ctx,
            on_start,
        )
        .await
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
        // Same shape as a background task: a child of the caller's
        // token, so one token stands for "this job should stop" — an
        // interrupted turn takes the job with it, and
        // `abort_all`/`shutdown` can still cancel it alone.
        let background_cancel = root_cancel.child_token();
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
        // The lease may have been waited on for a while: re-derive the
        // workspace and make sure it is still the one that was resolved
        // before the wait.
        let (leased_location, leased_depth) = match session_workspace_location(
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
            // A routed notification is a resume of this session, so it
            // carries what the session never read — the parent's
            // messages go in ahead of the completion that woke it. The
            // hold is per attempt: an attempt that never appended
            // anything puts them back when it drops.
            let mut queued = self.child_steers.adopt(&notification.parent_session_id);
            let text = queued.prompt(&notification.text);
            let result = run_turn(
                self.resolver.as_ref(),
                &registry,
                &self.store,
                &notification.parent_session_id,
                &text,
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
                    // Set by the turn below from the notified session's
                    // own model.
                    vision: false,
                    // A routed notification is a fresh view of the
                    // session: nothing has been read on this context yet.
                    seen_files: crate::tools::SeenFiles::default(),
                    // The spawner is built from configuration, not from a
                    // tool context, so this path has no state directory
                    // to spill into and truncates as it always did.
                    spill_dir: None,
                },
                // No live channel: this turn is not the parent's to
                // steer, so a message that arrives while it runs waits
                // for the resume after it.
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
                result => {
                    queued.started();
                    break result;
                }
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

/// The isolated workspace a task was persisted with, as the task tool
/// takes it — `None` when the task ran in the caller's own checkout and
/// no workspace has to be named at all.
fn persisted_worktree(
    meta: &SessionMeta,
    parent: &crate::tools::WorkspaceLocation,
) -> Option<TaskWorkspaceInput> {
    let persisted = meta.workspace.as_ref()?;
    if persisted == parent {
        return None;
    }
    match persisted.isolation() {
        crate::tools::WorkspaceIsolation::Shared => None,
        crate::tools::WorkspaceIsolation::GitWorktree { .. } => Some(TaskWorkspaceInput {
            cwd: persisted.cwd().to_path_buf(),
            isolation: TaskWorkspaceIsolation::GitWorktree,
        }),
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
    /// The same events say when the child took a message, so this is
    /// where a delivered one stops counting as pending.
    steers: ChildSteers,
    parent_session_id: String,
    parent_call_id: String,
    child_session_id: String,
}

impl ActivityPublisher {
    fn publish(&self, event: LoopEvent) {
        if let LoopEvent::Steered { text } = &event {
            self.steers.delivered(&self.child_session_id, text);
        }
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

/// One message for one task, whatever that task is currently doing.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskMessageInput {
    pub task_id: String,
    pub message: String,
    /// The task's own worktree, when it has one: a resume has to name it
    /// again, exactly as the task tool's `task_id` does.
    #[serde(default, deserialize_with = "deserialize_optional_workspace")]
    pub workspace: Option<TaskWorkspaceInput>,
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
        "Delegate one clearly bounded unit of work to a configured agent. Delegation transfers ownership: do not perform the delegated scope yourself. Independent reviews must be explicitly delegated as separate bounded review tasks. subagent_type names the agent, and the agent — not this call — fixes its tools and whether it may write: prefer an agent marked read-only for repository inspection and review so sibling tasks can run concurrently, and use a mutable agent only when edits or mutating tools are required.\n\nOmit workspace by default and the task runs in your own checkout. Always omit it for a read-only agent: read-only tasks share the checkout, so they run alongside each other. Omit it for a mutable agent too on an ordinary foreground turn: mutable tasks sharing one checkout serialize behind its write lease, so each one sees the previous one's edits and nothing has to be merged — that is what you want for dependent, sequential work. Pass workspace when independent mutable tasks should run in parallel: write leases are per workspace, so tasks in separate worktrees run at the same time, and you merge their divergent results yourself once each has reported. Pass it also when you are yourself running as a subagent and delegate a mutable task, since your own checkout is already held for the whole of your run, and for a mutable background task, which would otherwise hold your checkout — and block your own edits — until it finishes. A workspace must be an existing Git worktree of this repository that you create first (`git worktree add ../ilar-task-<name> -b task/<name>`) and then name as {\"cwd\": \"../ilar-task-<name>\", \"isolation\": \"git_worktree\"} — ilar validates it and never creates one, so outside a Git repository there is no isolated workspace and parallel mutable tasks are unavailable.\n\nBackground follows the agent: a read-only agent's task you delegate runs in the background and reports back as a notification while you keep working, and a mutable agent's task runs in the foreground. Pass background false when you need the result to continue this turn's work — foreground sibling tasks can be called together for parallel current-answer work. Pass background true for a mutable task whose deferred completion should trigger a separate follow-up turn, with a workspace of its own. Never poll a detached task: task_message steers it mid-flight, and its completion comes to you."
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
        // Built from the configured agents, so the shape the model copies
        // names an agent that exists here rather than one from another
        // install; the scope is inspection, which every agent may do,
        // because which agents are mutable is per install.
        let example = self.spawner.agents().first().map(|agent| {
            serde_json::json!({
                "description": "trace the retry path",
                "prompt": "Find where the HTTP client retries in src/, and report the call sites and the backoff policy.",
                "subagent_type": agent.name,
            })
        });
        let overview = match example {
            Some(example) => format!(
                "One agent, one bounded scope. Example: {example}. Add a workspace only in the cases described below."
            ),
            None => {
                "One agent, one bounded scope. Add a workspace only in the cases described below."
                    .to_string()
            }
        };
        serde_json::json!({
            "type": "object",
            "description": overview,
            "properties": {
                "description": {"type": "string", "description": "Short task description (3-5 words)"},
                "prompt": {"type": "string", "description": "Full instructions for one bounded scope. The parent should continue only clearly disjoint work."},
                "subagent_type": {
                    "type": "string",
                    "enum": agents,
                    "description": format!("Configured agent to run; it fixes the tools the task gets and whether the task may write. Available agents: {agent_guidance}. Prefer an agent marked read-only for repository review and parallel inspection; use a mutable agent only when edits or mutating tools are required.")
                },
                "task_id": {
                    "type": ["string", "null"],
                    "description": "Existing task session UUID to resume, replaying that task's full context — prefer it over a fresh task for follow-up questions on the same scope. Use an id reported by a task result, a task-notification, or the tasks tool; set null or omit to start a new task, and never invent a value. Resuming a task that ran in its own worktree requires that same workspace passed again."
                },
                "model": {
                    "type": ["string", "null"],
                    "description": "Model override for this task (provider/model-id). Omit to inherit. Call the models tool to see options with pricing — prefer a cheap/fast model for mechanical work."
                },
                "reasoning": {
                    "type": ["string", "null"],
                    "description": "Reasoning variant for the chosen model (see the models tool). Omit for the model's default."
                },
                "background": {"type": "boolean", "description": "Whether the task runs detached. Omit it and the agent decides: a read-only agent's task runs in the background, a mutable agent's in the foreground. Pass false when you need the result to continue this turn's work; read-only tasks otherwise run in the background and report back as notifications, freeing you to keep working. Pass true for a mutable task whose completion should trigger a separate follow-up turn, and give it its own workspace. Do not poll a detached task: its completion finds you as a notification, and task_message corrects its course mid-flight."}
                ,"workspace": {
                    "type": ["object", "null"],
                    "description": format!("Sibling Git worktree to run this task in. Set null or omit to use the current checkout — right for every read-only agent, and for a mutable agent unless you already hold this checkout or want independent mutable tasks to run in parallel. cwd must already be a registered worktree of this repository, because ilar validates the path and never creates one: {WORKTREE_CORRECTION}. This is a cooperative scheduling domain, not a sandbox: tasks in separate worktrees run at the same time and their results are yours to merge afterwards."),
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

/// task_message: the one verb for talking to a task, running or
/// finished. Same concurrency and workspace declarations as the task
/// tool, because its finished branch *is* a task invocation.
pub struct TaskMessageTool {
    spawner: Arc<SubagentSpawner>,
}

impl TaskMessageTool {
    pub fn new(spawner: Arc<SubagentSpawner>) -> Self {
        Self { spawner }
    }
}

impl Tool for TaskMessageTool {
    fn name(&self) -> &'static str {
        "task_message"
    }

    fn description(&self) -> &'static str {
        "Send a message to a task you spawned — one verb whether it is still running or already finished, and you do not need to know which. A task that is still running receives it at its next step, the way a message reaches you mid-turn, and keeps going: its answer arrives as that task's own result or completion notification, never as the output of this call. A task that has finished is resumed from its transcript with your message as its prompt, and this call returns its answer exactly as a task call does. A message a running task ended before reading is not lost: it waits and is delivered ahead of the prompt of that task's next resume, and the tasks tool shows what is still waiting. Use it to correct a background task's course, add a constraint it should have had, or ask a finished task a follow-up question with its context intact. A foreground task of the turn you are in cannot be messaged: you are blocked on its result, so it is over before you can speak again — that is what background tasks are for."
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
        serde_json::json!({
            "type": "object",
            "description": "One message for one task. Running: it lands at that task's next step and the task keeps its own result path. Finished: it resumes the task from its transcript and this call returns what the task says.",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task session UUID to talk to. Use an id reported by a task result, a task-notification, or the tasks tool, and never invent a value; it must be a task this session spawned."
                },
                "message": {
                    "type": "string",
                    "description": "What to tell the task — a correction, a constraint it should have had, or a follow-up question. Write it as if you could interrupt it, because for a running task that is exactly what this is."
                },
                "workspace": {
                    "type": ["object", "null"],
                    "description": format!("The Git worktree a finished task is resumed in. Set null or omit: a task that ran in its own worktree is resumed there from its own metadata, which is why this call needs nothing but an id. Pass it only to name that same worktree explicitly, the way the task tool's resume does — it must be the registered worktree the task actually ran in ({WORKTREE_CORRECTION}), and one that has been removed has to be restored at that path."),
                    "properties": {
                        "cwd": {"type": "string"},
                        "isolation": {"type": "string", "enum": ["git_worktree"]}
                    },
                    "required": ["cwd", "isolation"],
                    "additionalProperties": false
                }
            },
            "required": ["task_id", "message"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            match parse_task_message(input) {
                Ok(input) => spawner.message_task(input, &ctx).await,
                Err(error) => error,
            }
        })
    }

    /// Same deferral as the task tool: the resume branch is a task
    /// start, and it is announced when it starts rather than when the
    /// call is made.
    fn run_observed(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
        on_start: ToolStartObserver,
    ) -> ToolFuture {
        let spawner = self.spawner.clone();
        Box::pin(async move {
            match parse_task_message(input) {
                Ok(input) => {
                    spawner
                        .message_task_observed(input, &ctx, Some(on_start))
                        .await
                }
                Err(error) => error,
            }
        })
    }
}

fn parse_task_message(input: serde_json::Value) -> Result<TaskMessageInput, ToolOutput> {
    serde_json::from_value(input)
        .map_err(|error| ToolOutput::error(format!("invalid input for task_message: {error}")))
}

/// How many tasks the listing reports, newest first. A long session
/// can accumulate dozens; the recent ones are the resumable ones.
const TASK_LISTING_LIMIT: usize = 20;
/// Display width of the message a resume is named by.
const TASK_MESSAGE_LABEL_CHARS: usize = 40;
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
         model, whether one is still running, how many of your messages \
         it has not read yet (pending), and a snippet of what it last \
         said. Pass an id to task_message to talk to one — a running \
         task is steered at its next step, a finished one is resumed \
         with its context intact — or back as the task tool's task_id to \
         give a finished task a fresh scope."
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
                    // What the parent said and the task has not read:
                    // in flight while it runs, waiting for its resume
                    // once it has stopped. Either way it is owed a
                    // reading, so the listing says so.
                    let waiting = match spawner.child_steers.pending(&child.id) {
                        0 => String::new(),
                        1 => " · 1 message pending".to_string(),
                        count => format!(" · {count} messages pending"),
                    };
                    format!(
                        "{} · {} · {} · {status}{waiting} · {}\n  task: {}{last}",
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
                    cwd: None,
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

    /// The undelivered rule at its own level: a run that never started
    /// hands the queue back in order, and a run that did takes it.
    #[test]
    fn an_unstarted_run_hands_its_queue_back_in_order() {
        let steers = ChildSteers::default();
        steers.queue("child", "first".into());
        steers.queue("child", "second".into());

        {
            let (_receiver, run) = steers.open("child");
            assert_eq!(run.prompt("go on"), "first\n\nsecond\n\ngo on");
            assert_eq!(steers.pending("child"), 0, "the run holds them");
        }
        assert_eq!(steers.pending("child"), 2, "an unstarted run kept them");

        let (_receiver, mut run) = steers.open("child");
        run.started();
        drop(run);
        assert_eq!(steers.pending("child"), 0);
    }

    #[test]
    fn a_message_reaches_a_running_turn_and_stops_pending_once_taken() {
        let steers = ChildSteers::default();
        assert!(!steers.steer("child", "before any turn".into()));

        let (mut receiver, mut run) = steers.open("child");
        run.started();
        assert!(steers.steer("child", "mid-turn".into()));
        assert_eq!(receiver.try_recv().unwrap(), "mid-turn");
        assert_eq!(steers.pending("child"), 1, "sent is not yet read");

        steers.delivered("child", "mid-turn");
        assert_eq!(steers.pending("child"), 0);
        drop(run);
        assert!(!steers.steer("child", "after the turn".into()));
    }

    /// A steer the turn ended before reading is the message the next run
    /// opens with — the root rule, one level down.
    #[test]
    fn a_steer_the_turn_never_read_heads_the_next_run() {
        let steers = ChildSteers::default();
        let (receiver, mut run) = steers.open("child");
        run.started();
        assert!(steers.steer("child", "look at the migration".into()));
        drop(receiver);
        drop(run);

        let (_receiver, next) = steers.open("child");
        assert_eq!(next.prompt("continue"), "look at the migration\n\ncontinue");
    }

    #[test]
    fn nested_context_failure_propagates_to_the_grandparent() {
        let meta = SessionMeta {
            session_id: "parent".into(),
            parent_id: Some("grandparent".into()),
            agent: "build".into(),
            model: "zai/glm-4.7".into(),
            workspace: None,
            cwd: None,
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
