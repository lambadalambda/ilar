//! Task tool + subagent spawner — see meta/issues/task-tool-subagents.md.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::agent::{LoopConfig, run_turn};
use crate::config::AgentDefinition;
use crate::config::system_prompt_for;
use crate::provider::Provider;
use crate::session::{ContentBlock, SessionMeta, SessionStore, new_id};
use crate::tools::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, ToolRegistry};
use serde::Deserialize;

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
        Self {
            provider,
            store,
            agents,
            cwd,
            depth,
            max_concurrent,
            max_depth,
            running: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }

    pub fn agents(&self) -> &[AgentDefinition] {
        &self.agents
    }

    /// Spawner for children of this spawner: one level deeper, shared slot
    /// counter.
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
                "task_id": {"type": "string", "description": "Resume a previous task's session"}
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
