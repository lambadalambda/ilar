//! Task tool + subagent spawner — see meta/issues/task-tool-subagents.md.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::agent::{LoopConfig, run_turn};
use crate::config::AgentDefinition;
use crate::config::system_prompt_for;
use crate::provider::ProviderResolver;
use crate::session::{ContentBlock, SessionMeta, SessionStore, new_id};
use crate::tools::{
    Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, ToolRegistry, WorkspaceAccess,
};
use serde::Deserialize;

/// A completed background task's notification — the synthetic user
/// message that re-invokes the parent loop.
#[derive(Debug, Clone)]
pub struct Notification {
    pub parent_session_id: String,
    pub description: String,
    pub text: String,
    pub is_error: bool,
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
    cwd: std::path::PathBuf,
    depth: usize,
    max_concurrent: usize,
    max_depth: usize,
    running: Arc<AtomicUsize>,
    /// Background completions land here; the session owner consumes.
    notify_tx: tokio::sync::mpsc::UnboundedSender<Notification>,
    /// The single notification receiver, handed out by `subscribe`.
    notify_rx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Notification>>>>,
    stall_timeout: std::time::Duration,
    /// Abort handles for detached background tasks.
    background_tasks: Arc<Mutex<BackgroundRegistry>>,
    workspace: crate::tools::WorkspaceScheduler,
    background_tool_timeout: std::time::Duration,
}

struct BackgroundTask {
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
        let (notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            notify_rx: Arc::new(Mutex::new(Some(notify_rx))),
            resolver,
            store,
            agents,
            cwd,
            depth,
            max_concurrent,
            max_depth,
            running: Arc::new(AtomicUsize::new(0)),
            notify_tx,
            stall_timeout: std::time::Duration::from_secs(600),
            background_tasks: Arc::new(Mutex::new(BackgroundRegistry::default())),
            workspace: crate::tools::WorkspaceScheduler::new(),
            background_tool_timeout: std::time::Duration::from_secs(600),
        }
    }

    /// Override the background stall watchdog timeout (tests).
    pub fn with_stall_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.stall_timeout = timeout;
        self
    }

    pub fn with_background_tool_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.background_tool_timeout = timeout;
        self
    }

    pub fn background_tool_timeout(&self) -> std::time::Duration {
        self.background_tool_timeout
    }

    pub fn workspace(&self) -> crate::tools::WorkspaceScheduler {
        self.workspace.clone()
    }

    /// Receiver for background-task notifications (single consumer;
    /// second call returns an already-closed receiver).
    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<Notification> {
        self.notify_rx
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| tokio::sync::mpsc::unbounded_channel::<Notification>().1)
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
    fn child_spawner(self: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self {
            resolver: self.resolver.clone(),
            store: self.store.clone(),
            agents: self.agents.clone(),
            cwd: self.cwd.clone(),
            depth: self.depth + 1,
            max_concurrent: self.max_concurrent,
            max_depth: self.max_depth,
            running: self.running.clone(),
            notify_tx: self.notify_tx.clone(),
            notify_rx: self.notify_rx.clone(),
            stall_timeout: self.stall_timeout,
            background_tasks: self.background_tasks.clone(),
            workspace: self.workspace.clone(),
            background_tool_timeout: self.background_tool_timeout,
        })
    }

    /// Run one subagent task; returns its final text as the tool output.
    pub async fn run_task(self: &Arc<Self>, input: TaskInput, ctx: &ToolContext) -> ToolOutput {
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
                    if session
                        .meta()
                        .is_none_or(|meta| meta.agent != input.subagent_type)
                    {
                        return ToolOutput::error(format!(
                            "resuming task session {id:?}: persisted agent does not match {:?}",
                            input.subagent_type
                        ));
                    }
                    id.clone()
                }
                Err(error) => {
                    return ToolOutput::error(format!("resuming task session {id:?}: {error}"));
                }
            },
            None => {
                let id = new_id();
                let model = match &agent.model {
                    Some(model) => model.clone(),
                    None => match self.store.load(&ctx.session_id) {
                        Ok(parent) => parent.effective_model(),
                        Err(error) => {
                            return ToolOutput::error(format!(
                                "loading parent session {:?}: {error}",
                                ctx.session_id
                            ));
                        }
                    },
                };
                let created = self.store.create(SessionMeta {
                    session_id: id.clone(),
                    parent_id: Some(ctx.session_id.clone()),
                    agent: input.subagent_type.clone(),
                    model,
                });
                match created {
                    Ok(session) => drop(session),
                    Err(error) => {
                        return ToolOutput::error(format!("creating subagent session: {error}"));
                    }
                };
                id
            }
        };

        let mut system_prompt = system_prompt_for(&self.cwd);
        if !agent.prompt.is_empty() {
            system_prompt = format!(
                "{system_prompt}\n\n# Agent: {}\n\n{}",
                agent.name, agent.prompt
            );
        }

        // Child registry: builtins + task tool with the deeper spawner.
        let child_spawner = self.child_spawner();
        let registry = match ToolRegistry::builtin().with_subagents(child_spawner.clone()) {
            Ok(registry) => registry,
            Err(error) => {
                return ToolOutput::error(format!("building child tool registry: {error}"));
            }
        };
        let child_ctx = ToolContext {
            cwd: self.cwd.clone(),
            session_id: session_id.clone(),
            depth: self.depth + 1,
            subagent: Some(child_spawner),
            workspace: ctx.workspace.clone(),
            cancel: ctx.cancel.clone(),
        };

        if input.background == Some(true) {
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
            let mut background_registry = self.background_tasks.lock().unwrap();
            if background_registry.closed {
                return ToolOutput::error("background runtime is shutting down");
            }
            background_registry
                .tasks
                .retain(|task| !task.handle.is_finished());
            let handle = tokio::spawn(async move {
                let _slot = _guard; // hold the concurrency slot for the run
                let cancel = tokio_util::sync::CancellationToken::new();
                let (tx, mut rx_evt) = tokio::sync::mpsc::unbounded_channel();
                // Activity tracker: any child event counts as progress.
                let last_activity = Arc::new(Mutex::new(std::time::Instant::now()));
                let watcher_last = last_activity.clone();
                let watcher = tokio::spawn(async move {
                    while rx_evt.recv().await.is_some() {
                        *watcher_last.lock().unwrap() = std::time::Instant::now();
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
                    LoopConfig::default(),
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
                watcher.abort();
                let _ = watcher.await;

                let notification = match outcome {
                    Some(Ok(_)) => {
                        let text = spawner
                            .store
                            .load(&session_id)
                            .ok()
                            .and_then(|s| {
                                s.transcript().iter().rev().find_map(|m| {
                                    m.content.iter().find_map(|b| match b {
                                        ContentBlock::Text { text } => Some(text.clone()),
                                        _ => None,
                                    })
                                })
                            })
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
                let _ = spawner.notify_tx.send(notification);
            });
            background_registry.tasks.push(BackgroundTask {
                handle,
                cancel: background_cancel,
            });
            return ToolOutput::text(
                "Task started in the background. You will be notified when it completes. \
DO NOT sleep, poll, or check on it — work on something else or end your response."
                    .to_string(),
            );
        }

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = run_turn(
            self.resolver.as_ref(),
            &registry,
            &self.store,
            &session_id,
            &input.prompt,
            Some(&system_prompt),
            LoopConfig::default(),
            tx,
            ctx.cancel.clone(),
            child_ctx,
        )
        .await;

        match outcome {
            Ok(_) => {
                // Final text = last assistant text block of the child session.
                let text = self
                    .store
                    .load(&session_id)
                    .ok()
                    .and_then(|s| {
                        s.transcript().iter().rev().find_map(|m| {
                            m.content.iter().find_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                        })
                    })
                    .unwrap_or_else(|| "(subagent finished with no text)".into());
                ToolOutput::text(text)
            }
            Err(e) => ToolOutput::error(format!("subagent failed: {e:#}")),
        }
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
        let job_id = new_id();
        let notification_id = job_id.clone();
        let spawner = self.clone();
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
            let handle = tokio::spawn(async move {
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
                let _ = spawner.notify_tx.send(Notification {
                    parent_session_id,
                    description,
                    text,
                    is_error,
                });
            });
            background_registry.tasks.push(BackgroundTask {
                handle,
                cancel: background_cancel,
            });
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
        let parent = self.store.load(&notification.parent_session_id)?;
        let meta = parent
            .meta()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("notification parent has no metadata"))?;
        let agent = self
            .agents
            .iter()
            .find(|agent| agent.name == meta.agent)
            .ok_or_else(|| anyhow::anyhow!("unknown persisted agent {:?}", meta.agent))?;
        let depth = session_depth(&self.store, &notification.parent_session_id)?;
        let runtime = Arc::new(Self {
            resolver: self.resolver.clone(),
            store: self.store.clone(),
            agents: self.agents.clone(),
            cwd: self.cwd.clone(),
            depth,
            max_concurrent: self.max_concurrent,
            max_depth: self.max_depth,
            running: self.running.clone(),
            notify_tx: self.notify_tx.clone(),
            notify_rx: self.notify_rx.clone(),
            stall_timeout: self.stall_timeout,
            background_tasks: self.background_tasks.clone(),
            workspace: self.workspace.clone(),
            background_tool_timeout: self.background_tool_timeout,
        });
        let registry = ToolRegistry::builtin().with_subagents(runtime.clone())?;
        let mut system_prompt = system_prompt_for(&self.cwd);
        if !agent.prompt.is_empty() {
            system_prompt = format!(
                "{system_prompt}\n\n# Agent: {}\n\n{}",
                agent.name, agent.prompt
            );
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
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
                LoopConfig::default(),
                tx.clone(),
                cancel.clone(),
                ToolContext {
                    cwd: self.cwd.clone(),
                    session_id: notification.parent_session_id.clone(),
                    depth,
                    subagent: Some(runtime.clone()),
                    workspace: self.workspace.clone(),
                    cancel: cancel.clone(),
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
            return outcome.map(|_| RouteOutcome::Complete);
        };
        let (text, is_error) = match outcome {
            Ok(crate::agent::TurnOutcome::Aborted) => {
                ("Nested parent turn was cancelled.".to_string(), true)
            }
            Ok(_) => (
                final_text_after_last_user(&self.store, &notification.parent_session_id)
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
}

fn final_text_after_last_user(store: &SessionStore, session_id: &str) -> Option<String> {
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
                    content.iter().find_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                }
                _ => None,
            })
    })
}

fn session_depth(store: &SessionStore, session_id: &str) -> anyhow::Result<usize> {
    let mut depth = 0;
    let mut current = session_id.to_string();
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(current.clone()) {
            anyhow::bail!("session parent cycle at {current}");
        }
        let session = store.load(&current)?;
        let Some(parent) = session.meta().and_then(|meta| meta.parent_id.clone()) else {
            return Ok(depth);
        };
        depth += 1;
        current = parent;
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
    #[serde(default)]
    pub task_id: Option<String>,
    /// Run detached; completion arrives as a notification.
    #[serde(default)]
    pub background: Option<bool>,
}

/// The task tool: spawns subagents. Read-only for scheduling so sibling
/// tasks run concurrently (Claude Code semantics).
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
        // Agent list is dynamic; description built in `dynamic_description`.
        "Launch a subagent to do a unit of work. Returns its final answer."
    }

    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
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
            "properties": {
                "description": {"type": "string", "description": "Short task description (3-5 words)"},
                "prompt": {"type": "string", "description": "Full instructions for the subagent"},
                "subagent_type": {"type": "string", "description": "Agent name"},
                "task_id": {"type": "string", "description": "Resume a previous task's session"},
                "background": {"type": "boolean", "description": "Run detached; you will be notified on completion. DO NOT poll."}
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
}
