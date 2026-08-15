//! Built-in tools — see meta/issues/core-tools.md.

pub mod bash;
pub mod edit;
pub mod executor;
pub mod glob;
pub mod grep;
pub mod read;
pub mod web;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAccess {
    ReadOnly,
    Mutating,
}

#[derive(Clone)]
pub struct WorkspaceScheduler {
    lock: Arc<tokio::sync::RwLock<()>>,
}

pub enum WorkspacePermit {
    ReadOnly {
        _guard: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    Mutating {
        _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    },
}

impl WorkspaceScheduler {
    pub fn new() -> Self {
        Self {
            lock: Arc::new(tokio::sync::RwLock::new(())),
        }
    }

    pub async fn acquire(&self, access: WorkspaceAccess) -> WorkspacePermit {
        match access {
            WorkspaceAccess::ReadOnly => WorkspacePermit::ReadOnly {
                _guard: self.lock.clone().read_owned().await,
            },
            WorkspaceAccess::Mutating => WorkspacePermit::Mutating {
                _guard: self.lock.clone().write_owned().await,
            },
        }
    }
}

impl Default for WorkspaceScheduler {
    fn default() -> Self {
        Self::new()
    }
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
    pub workspace: WorkspaceScheduler,
    pub cancel: tokio_util::sync::CancellationToken,
}

impl ToolContext {
    /// Context for a root (non-subagent) session.
    pub fn root(cwd: std::path::PathBuf) -> Self {
        Self {
            cwd,
            session_id: String::new(),
            depth: 0,
            subagent: None,
            workspace: WorkspaceScheduler::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Context with a subagent spawner attached.
    pub fn with_subagents(
        mut self,
        spawner: std::sync::Arc<crate::subagent::SubagentSpawner>,
    ) -> Self {
        self.workspace = spawner.workspace();
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
    fn workspace_access(&self) -> WorkspaceAccess {
        match self.kind() {
            ToolKind::ReadOnly => WorkspaceAccess::ReadOnly,
            ToolKind::Mutating => WorkspaceAccess::Mutating,
        }
    }
    fn supports_background(&self) -> bool {
        false
    }
    fn manages_workspace_access(&self) -> bool {
        false
    }
    fn input_schema(&self) -> serde_json::Value;
    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture;
}

/// Named tool lookup + provider-facing definitions.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("duplicate tool name: {0}")]
pub struct DuplicateToolError(&'static str);

impl DuplicateToolError {
    pub fn tool_name(&self) -> &'static str {
        self.0
    }
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
                Arc::new(web::WebFetchTool::default()),
            ],
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    /// Registry with an extra tool (tests, future custom tools).
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Result<Self, DuplicateToolError> {
        if self
            .tools
            .iter()
            .any(|existing| existing.name() == tool.name())
        {
            return Err(DuplicateToolError(tool.name()));
        }
        self.tools.push(tool);
        Ok(self)
    }

    /// Registry with the skill tool attached.
    pub fn with_skills(
        self,
        store: std::sync::Arc<crate::skill::SkillStore>,
    ) -> Result<Self, DuplicateToolError> {
        self.with_tool(std::sync::Arc::new(crate::skill::SkillTool::new(store)))
    }

    /// Registry with a search backend attached.
    pub fn with_search(
        self,
        backend: Box<dyn web::SearchBackend>,
    ) -> Result<Self, DuplicateToolError> {
        self.with_tool(std::sync::Arc::new(web::WebSearchTool::new(backend)))
    }

    /// Registry with optional Tavily websearch. Webfetch is already builtin.
    pub fn with_web_tools(self) -> Result<Self, DuplicateToolError> {
        match web::TavilyBackend::from_env() {
            Some(backend) => self.with_search(Box::new(backend)),
            None => Ok(self),
        }
    }

    /// Registry with the todo tool attached (shared list for TUI display).
    pub fn with_todos(
        self,
        list: std::sync::Arc<std::sync::Mutex<crate::todo::TodoList>>,
    ) -> Result<Self, DuplicateToolError> {
        self.with_tool(std::sync::Arc::new(crate::todo::TodoTool::new(list)))
    }

    /// Registry with the task (subagent) tool attached.
    pub fn with_subagents(
        self,
        spawner: Arc<crate::subagent::SubagentSpawner>,
    ) -> Result<Self, DuplicateToolError> {
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

struct CancelBlockingScan(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for CancelBlockingScan {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

pub(crate) async fn blocking_scan<T, F>(scan: F) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce(std::sync::Arc<std::sync::atomic::AtomicBool>) -> T + Send + 'static,
{
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_on_drop = CancelBlockingScan(cancelled.clone());
    let result = tokio::task::spawn_blocking(move || scan(cancelled)).await;
    drop(cancel_on_drop);
    result
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn dropping_blocking_scan_signals_worker() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let stopped = std::sync::Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let task = tokio::spawn(super::blocking_scan(move |cancelled| {
            while !cancelled.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            worker_stopped.store(true, Ordering::Release);
        }));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        task.abort();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !stopped.load(Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("blocking worker did not observe cancellation");
    }
}
