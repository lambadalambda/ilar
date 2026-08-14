//! Built-in tools — see meta/issues/core-tools.md.

pub mod bash;
pub mod edit;
pub mod executor;
pub mod glob;
pub mod grep;
pub mod read;
pub mod write;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::provider::ToolDefinition;

/// Scheduling class used by the executor's concurrency barrier:
/// read-only tools may run alongside each other; mutating tools form a
/// barrier (the Claude Code `isConcurrencySafe` model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    ReadOnly,
    Mutating,
}

/// Per-invocation context. No permission checks — the sandbox is the
/// permission system.
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: std::path::PathBuf,
    /// Session the tool call belongs to (parent link for subagents).
    pub session_id: String,
    /// Subagent nesting depth (0 = root session).
    pub depth: usize,
    /// Subagent spawner, when the task tool is available.
    pub subagent: Option<std::sync::Arc<crate::subagent::SubagentSpawner>>,
}

impl ToolContext {
    /// Context for a root (non-subagent) session.
    pub fn root(cwd: std::path::PathBuf) -> Self {
        Self {
            cwd,
            session_id: String::new(),
            depth: 0,
            subagent: None,
        }
    }

    /// Context with a subagent spawner attached.
    pub fn with_subagents(
        mut self,
        spawner: std::sync::Arc<crate::subagent::SubagentSpawner>,
    ) -> Self {
        self.subagent = Some(spawner);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

pub type ToolFuture = Pin<Box<dyn Future<Output = ToolOutput> + Send>>;

/// A built-in or custom tool. `run` is boxed (not `async fn`) so the
/// registry can hold `Arc<dyn Tool>`.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn kind(&self) -> ToolKind;
    fn input_schema(&self) -> serde_json::Value;
    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture;
}

/// Named tool lookup + provider-facing definitions.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn builtin() -> Self {
        Self {
            tools: vec![
                Arc::new(read::ReadTool),
                Arc::new(write::WriteTool),
                Arc::new(edit::EditTool),
                Arc::new(bash::BashTool),
                Arc::new(glob::GlobTool),
                Arc::new(grep::GrepTool),
            ],
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    /// Registry with an extra tool (tests, future custom tools).
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Registry with the todo tool attached (shared list for TUI display).
    pub fn with_todos(self, list: std::sync::Arc<std::sync::Mutex<crate::todo::TodoList>>) -> Self {
        self.with_tool(std::sync::Arc::new(crate::todo::TodoTool::new(list)))
    }

    /// Registry with the task (subagent) tool attached.
    pub fn with_subagents(self, spawner: Arc<crate::subagent::SubagentSpawner>) -> Self {
        self.with_tool(Arc::new(crate::subagent::TaskTool::new(spawner)))
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|t| ToolDefinition {
                name: t.name().into(),
                description: t.description().into(),
                input_schema: t.input_schema(),
            })
            .collect()
    }
}

/// Parse tool input; on failure return a ToolOutput error instead of
/// panicking (malformed model output must not crash the loop).
pub fn parse_input<T: serde::de::DeserializeOwned>(
    input: serde_json::Value,
    tool_name: &str,
) -> Result<T, ToolOutput> {
    serde_json::from_value(input)
        .map_err(|e| ToolOutput::error(format!("invalid input for {tool_name}: {e}")))
}
