//! Task tool + subagent spawner — see meta/issues/task-tool-subagents.md.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::agent::{LoopConfig, run_turn};
use crate::config::AgentDefinition;
use crate::config::system_prompt_for;
use crate::provider::Provider;
use crate::session::{ContentBlock, SessionMeta, SessionStore, new_id};
use crate::tools::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, ToolRegistry};
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

/// Spawns child agent loops with their own sessions. Shared across a
/// session's turns (concurrency slot counter) and cloned into children
/// (depth+1) for nesting up to the depth cap.
pub struct SubagentSpawner {
    provider: Arc<dyn Provider>,
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
    background_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl SubagentSpawner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn Provider>,
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
            provider,
            store,
            agents,
            cwd,
            depth,
            max_concurrent,
            max_depth,
            running: Arc::new(AtomicUsize::new(0)),
            notify_tx,
            stall_timeout: std::time::Duration::from_secs(600),
            background_tasks: Mutex::new(Vec::new()),
        }
    }

    /// Override the background stall watchdog timeout (tests).
    pub fn with_stall_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.stall_timeout = timeout;
        self
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
        let mut tasks = self.background_tasks.lock().unwrap();
        for task in tasks.drain(..) {
            task.abort();
        }
    }

    /// Number of live detached background tasks.
    pub fn running_background(&self) -> usize {
        let mut tasks = self.background_tasks.lock().unwrap();
        tasks.retain(|t| !t.is_finished());
        tasks.len()
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }

    pub fn agents(&self) -> &[AgentDefinition] {
        &self.agents
    }

    /// Spawner for children of this spawner: one level deeper, shared slot
    /// counter and notification channel.
    fn child_spawner(self: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self {
            provider: self.provider.clone(),
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
            background_tasks: Mutex::new(Vec::new()),
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
            Some(id) if self.store.load(id).is_ok() => id.clone(),
            _ => {
                let id = new_id();
                let model = agent.model.clone().unwrap_or_else(|| "zai/glm-4.7".into());
                if let Err(e) = self.store.create(SessionMeta {
                    session_id: id.clone(),
                    parent_id: Some(ctx.session_id.clone()),
                    agent: input.subagent_type.clone(),
                    model,
                }) {
                    return ToolOutput::error(format!("creating subagent session: {e}"));
                }
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
        let registry = ToolRegistry::builtin().with_subagents(child_spawner.clone());
        let child_ctx = ToolContext {
            cwd: self.cwd.clone(),
            session_id: session_id.clone(),
            depth: self.depth + 1,
            subagent: Some(child_spawner),
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
                    spawner.provider.as_ref(),
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
                let outcome = tokio::select! {
                    outcome = turn => Some(outcome),
                    () = stall_watch => {
                        cancel.cancel();
                        None // stalled
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
            self.background_tasks.lock().unwrap().push(handle);
            return ToolOutput::text(
                "Task started in the background. You will be notified when it completes. \
DO NOT sleep, poll, or check on it — work on something else or end your response."
                    .to_string(),
            );
        }

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = run_turn(
            self.provider.as_ref(),
            &registry,
            &self.store,
            &session_id,
            &input.prompt,
            Some(&system_prompt),
            LoopConfig::default(),
            tx,
            tokio_util::sync::CancellationToken::new(),
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
