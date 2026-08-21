//! Built-in tools — see meta/issues/core-tools.md.

pub mod bash;
pub mod edit;
pub mod executor;
pub mod glob;
pub mod grep;
pub mod models;
pub mod read;
pub mod service;
pub mod web;
pub mod write;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::{collections::HashMap, path::PathBuf};

use anyhow::Context as _;

use crate::provider::ToolDefinition;

/// Scheduling behavior within one provider step. This is independent of
/// whether a tool accesses or mutates the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolConcurrency {
    Concurrent,
    Barrier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAccess {
    None,
    ReadOnly,
    Mutating,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceId(PathBuf);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceIsolation {
    Shared,
    GitWorktree { common_dir: PathBuf },
}

/// Canonical checkout identity and cwd used for cooperative scheduling. A
/// validated worktree is not a filesystem sandbox; tools can still escape it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceLocation {
    cwd: PathBuf,
    root: PathBuf,
    id: WorkspaceId,
    isolation: WorkspaceIsolation,
}

impl WorkspaceLocation {
    pub fn shared(cwd: PathBuf) -> Self {
        Self::try_shared(cwd).unwrap_or_else(|error| panic!("{error:#}"))
    }

    pub fn try_shared(cwd: PathBuf) -> anyhow::Result<Self> {
        let cwd = std::fs::canonicalize(&cwd).map_err(|error| {
            anyhow::anyhow!("workspace cwd {cwd:?} cannot be resolved: {error}")
        })?;
        let root = checkout_root(&cwd).unwrap_or_else(|| cwd.clone());
        Ok(Self {
            cwd,
            id: WorkspaceId(root.clone()),
            root,
            isolation: WorkspaceIsolation::Shared,
        })
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub fn id(&self) -> &WorkspaceId {
        &self.id
    }

    pub fn isolation(&self) -> &WorkspaceIsolation {
        &self.isolation
    }

    pub async fn validated_git_worktree(
        parent: &WorkspaceLocation,
        requested_cwd: PathBuf,
    ) -> anyhow::Result<Self> {
        let requested_cwd = std::fs::canonicalize(&requested_cwd)
            .map_err(|error| anyhow::anyhow!("workspace cwd {:?}: {error}", requested_cwd))?;
        let (parent_root, parent_common) = git_paths(parent.cwd()).await?;
        let (root, common_dir) = git_paths(&requested_cwd).await?;
        if root == parent_root {
            anyhow::bail!("isolated workspace must use a different Git worktree");
        }
        if common_dir != parent_common {
            anyhow::bail!("isolated workspace must belong to the parent Git repository");
        }
        if !requested_cwd.starts_with(&root) {
            anyhow::bail!("workspace cwd is outside its Git worktree root");
        }

        let output = git_output(parent.root(), &["worktree", "list", "--porcelain", "-z"]).await?;
        let listed = output.split(|byte| *byte == 0).any(|field| {
            field
                .strip_prefix(b"worktree ")
                .and_then(|path| std::str::from_utf8(path).ok())
                .and_then(|path| std::fs::canonicalize(path).ok())
                .is_some_and(|path| path == root)
        });
        if !listed {
            anyhow::bail!("workspace is not a registered Git worktree");
        }

        Ok(Self {
            cwd: requested_cwd,
            id: WorkspaceId(root.clone()),
            root,
            isolation: WorkspaceIsolation::GitWorktree { common_dir },
        })
    }

    pub async fn revalidate(
        parent: &WorkspaceLocation,
        persisted: &WorkspaceLocation,
    ) -> anyhow::Result<Self> {
        match persisted.isolation() {
            WorkspaceIsolation::Shared => {
                let restored = WorkspaceLocation::try_shared(persisted.cwd.clone())?;
                if restored.id != persisted.id || restored.id != parent.id {
                    anyhow::bail!(
                        "persisted shared workspace no longer matches its parent checkout"
                    );
                }
                Ok(restored)
            }
            WorkspaceIsolation::GitWorktree { .. } => {
                WorkspaceLocation::validated_git_worktree(parent, persisted.cwd.clone()).await
            }
        }
    }
}

fn checkout_root(cwd: &std::path::Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|path| path.join(".git").exists())
        .and_then(|path| std::fs::canonicalize(path).ok())
}

async fn git_paths(cwd: &std::path::Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let root = git_path(cwd, "--show-toplevel").await?;
    let common = git_path(cwd, "--git-common-dir").await?;
    Ok((root, common))
}

async fn git_path(cwd: &std::path::Path, selector: &str) -> anyhow::Result<PathBuf> {
    let output = git_output(cwd, &["rev-parse", "--path-format=absolute", selector]).await?;
    let output = output.strip_suffix(b"\n").unwrap_or(&output);
    let path = std::str::from_utf8(output).context("Git returned a non-UTF-8 path")?;
    if path.is_empty() {
        anyhow::bail!("Git did not return a path for {selector}");
    }
    Ok(std::fs::canonicalize(path)?)
}

fn is_git_environment_variable(key: &std::ffi::OsStr) -> bool {
    key.to_string_lossy().starts_with("GIT_")
}

async fn git_output(cwd: &std::path::Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let mut command = tokio::process::Command::new("git");
    command.arg("-C").arg(cwd).args(args).kill_on_drop(true);
    for (key, _) in std::env::vars_os().filter(|(key, _)| is_git_environment_variable(key)) {
        command.env_remove(key);
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
        .await
        .map_err(|_| anyhow::anyhow!("Git workspace validation timed out"))??;
    if !output.status.success() {
        anyhow::bail!(
            "Git workspace validation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

#[derive(Clone)]
pub struct WorkspaceScheduler {
    locks: Arc<std::sync::Mutex<HashMap<WorkspaceId, Arc<tokio::sync::RwLock<()>>>>>,
    id: WorkspaceId,
}

pub enum WorkspacePermit {
    None,
    ReadOnly {
        _guard: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    Mutating {
        _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    },
}

pub struct WorkspaceLease {
    scheduler: Arc<std::sync::Mutex<HashMap<WorkspaceId, Arc<tokio::sync::RwLock<()>>>>>,
    id: WorkspaceId,
    access: WorkspaceAccess,
    _permit: WorkspacePermit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceCoverage {
    Absent,
    Covered,
    Incompatible,
}

impl WorkspaceScheduler {
    pub fn new() -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            id: WorkspaceId(PathBuf::from(format!("<ephemeral-{id}>"))),
        }
    }

    pub fn for_location(location: &WorkspaceLocation) -> Self {
        Self {
            locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            id: location.id.clone(),
        }
    }

    pub fn scoped(&self, location: &WorkspaceLocation) -> Self {
        Self {
            locks: self.locks.clone(),
            id: location.id.clone(),
        }
    }

    fn lock(&self) -> Arc<tokio::sync::RwLock<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(self.id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
            .clone()
    }

    pub async fn acquire(&self, access: WorkspaceAccess) -> WorkspacePermit {
        let lock = self.lock();
        match access {
            WorkspaceAccess::None => WorkspacePermit::None,
            WorkspaceAccess::ReadOnly => WorkspacePermit::ReadOnly {
                _guard: lock.read_owned().await,
            },
            WorkspaceAccess::Mutating => WorkspacePermit::Mutating {
                _guard: lock.write_owned().await,
            },
        }
    }

    pub async fn acquire_lease(&self, access: WorkspaceAccess) -> Arc<WorkspaceLease> {
        Arc::new(WorkspaceLease {
            scheduler: self.locks.clone(),
            id: self.id.clone(),
            access,
            _permit: self.acquire(access).await,
        })
    }

    pub fn try_acquire_lease(&self, access: WorkspaceAccess) -> Option<Arc<WorkspaceLease>> {
        let lock = self.lock();
        let permit = match access {
            WorkspaceAccess::None => WorkspacePermit::None,
            WorkspaceAccess::ReadOnly => WorkspacePermit::ReadOnly {
                _guard: lock.try_read_owned().ok()?,
            },
            WorkspaceAccess::Mutating => WorkspacePermit::Mutating {
                _guard: lock.try_write_owned().ok()?,
            },
        };
        Some(Arc::new(WorkspaceLease {
            scheduler: self.locks.clone(),
            id: self.id.clone(),
            access,
            _permit: permit,
        }))
    }
}

impl Default for WorkspaceScheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// Lossy sink for live tool-output tails: the latest value per call id
/// wins, drained by the loop-event receiver alongside input progress.
#[derive(Clone)]
pub struct OutputTailSink {
    tails: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
    wake: tokio::sync::mpsc::Sender<()>,
}

impl OutputTailSink {
    pub fn new(
        tails: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
        wake: tokio::sync::mpsc::Sender<()>,
    ) -> Self {
        Self { tails, wake }
    }

    pub fn report(&self, call_id: &str, tail: String) {
        self.tails.lock().unwrap().insert(call_id.to_string(), tail);
        let _ = self.wake.try_send(());
    }
}

/// Per-invocation context. No permission checks — the sandbox is the
/// permission system.
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: std::path::PathBuf,
    pub location: WorkspaceLocation,
    /// Session the tool call belongs to (parent link for subagents).
    pub session_id: String,
    /// Current provider tool-call id while a tool is executing.
    pub call_id: Option<String>,
    /// Subagent nesting depth (0 = root session).
    pub depth: usize,
    /// Subagent spawner, when the task tool is available.
    pub subagent: Option<std::sync::Arc<crate::subagent::SubagentSpawner>>,
    pub workspace: WorkspaceScheduler,
    pub workspace_lease: Option<Arc<WorkspaceLease>>,
    /// Workspace IDs whose leases are held by this child call stack.
    pub workspace_ancestry: Vec<WorkspaceId>,
    pub cancel: tokio_util::sync::CancellationToken,
    /// Live-output reporter for long-running tools, when a UI is listening.
    pub output_tail: Option<OutputTailSink>,
}

impl ToolContext {
    /// Context for a root (non-subagent) session.
    pub fn root(cwd: std::path::PathBuf) -> Self {
        let location = WorkspaceLocation::shared(cwd);
        Self {
            cwd: location.cwd.clone(),
            session_id: String::new(),
            call_id: None,
            depth: 0,
            subagent: None,
            workspace: WorkspaceScheduler::for_location(&location),
            location,
            workspace_lease: None,
            workspace_ancestry: Vec::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            output_tail: None,
        }
    }

    /// Context with a subagent spawner attached.
    pub fn with_subagents(
        mut self,
        spawner: std::sync::Arc<crate::subagent::SubagentSpawner>,
    ) -> Self {
        self.workspace = spawner.workspace();
        self.location = spawner.workspace_location();
        self.cwd = self.location.cwd.clone();
        self.subagent = Some(spawner);
        self
    }

    pub fn workspace_coverage(&self, requested: WorkspaceAccess) -> WorkspaceCoverage {
        let Some(lease) = &self.workspace_lease else {
            return WorkspaceCoverage::Absent;
        };
        if !Arc::ptr_eq(&lease.scheduler, &self.workspace.locks) || lease.id != self.workspace.id {
            return WorkspaceCoverage::Incompatible;
        }
        match (lease.access, requested) {
            (_, WorkspaceAccess::None)
            | (WorkspaceAccess::Mutating, _)
            | (WorkspaceAccess::ReadOnly, WorkspaceAccess::ReadOnly) => WorkspaceCoverage::Covered,
            (WorkspaceAccess::ReadOnly, WorkspaceAccess::Mutating)
            | (WorkspaceAccess::None, WorkspaceAccess::ReadOnly | WorkspaceAccess::Mutating) => {
                WorkspaceCoverage::Incompatible
            }
        }
    }

    pub fn has_workspace_lease(&self) -> bool {
        self.workspace_lease.is_some()
    }
}

#[derive(Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    child_session_id: Option<String>,
    state: Option<crate::session::SessionState>,
    pending_state_commit: Option<PendingStateCommit>,
}

#[derive(Clone)]
struct PendingStateCommit {
    target: std::sync::Arc<std::sync::Mutex<crate::todo::TodoList>>,
    list: crate::todo::TodoList,
}

impl std::fmt::Debug for ToolOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolOutput")
            .field("content", &self.content)
            .field("is_error", &self.is_error)
            .field("child_session_id", &self.child_session_id)
            .field("state", &self.state)
            .finish()
    }
}

impl PartialEq for ToolOutput {
    fn eq(&self, other: &Self) -> bool {
        self.content == other.content
            && self.is_error == other.is_error
            && self.child_session_id == other.child_session_id
            && self.state == other.state
    }
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            child_session_id: None,
            state: None,
            pending_state_commit: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            child_session_id: None,
            state: None,
            pending_state_commit: None,
        }
    }

    pub fn session_state(&self) -> Option<&crate::session::SessionState> {
        self.state.as_ref()
    }

    pub fn child_session_id(&self) -> Option<&str> {
        self.child_session_id.as_deref()
    }

    pub(crate) fn with_child_session(mut self, session_id: String) -> Self {
        self.child_session_id = Some(session_id);
        self
    }

    pub(crate) fn with_todo_state(
        mut self,
        target: std::sync::Arc<std::sync::Mutex<crate::todo::TodoList>>,
        list: crate::todo::TodoList,
    ) -> Self {
        self.state = Some(crate::session::SessionState::TodoList { list: list.clone() });
        self.pending_state_commit = Some(PendingStateCommit { target, list });
        self
    }

    pub(crate) fn discard_session_state(&mut self) {
        self.state = None;
        self.pending_state_commit = None;
    }

    pub(crate) fn commit_session_state(&mut self) {
        if let Some(commit) = self.pending_state_commit.take() {
            *commit.target.lock().unwrap() = commit.list;
        }
    }
}

pub type ToolFuture = Pin<Box<dyn Future<Output = ToolOutput> + Send>>;
pub type ToolStartObserver = Box<dyn FnOnce() + Send>;

/// A built-in or custom tool. `run` is boxed (not `async fn`) so the
/// registry can hold `Arc<dyn Tool>`.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn concurrency(&self) -> ToolConcurrency;
    fn workspace_access(&self) -> WorkspaceAccess;
    fn supports_background(&self) -> bool {
        false
    }
    fn manages_workspace_access(&self) -> bool {
        false
    }
    fn accepts_executor_workspace_lease(&self) -> bool {
        false
    }
    fn input_schema(&self) -> serde_json::Value;
    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture;
    fn run_observed(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
        on_start: ToolStartObserver,
    ) -> ToolFuture {
        on_start();
        self.run(input, ctx)
    }
}

/// Named tool lookup + provider-facing definitions.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    questions: Option<crate::question::QuestionSender>,
}

/// Tool names an agent `tools:` allowlist may reference — everything a
/// child registry can contain (builtins plus the task tool).
pub fn child_tool_names() -> Vec<&'static str> {
    let mut names = ToolRegistry::builtin().tool_names();
    names.push("task");
    names.push("service");
    names.push("models");
    names
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
            questions: None,
        }
    }

    /// Enforced read-only child registry. Delegation and shell access are
    /// omitted because prompts alone are not a capability boundary.
    pub fn read_only() -> Self {
        Self {
            tools: vec![
                Arc::new(read::ReadTool),
                Arc::new(glob::GlobTool),
                Arc::new(grep::GrepTool),
                Arc::new(web::WebFetchTool::default()),
            ],
            questions: None,
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    pub fn tool_names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    /// Registry reduced to an agent allowlist (intersection: allowlisted
    /// names absent from this registry are simply not granted).
    pub fn restricted_to(mut self, allowlist: &[String]) -> Self {
        self.tools
            .retain(|tool| allowlist.iter().any(|name| name == tool.name()));
        self
    }

    /// Registry with an extra tool (tests, future custom tools).
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Result<Self, DuplicateToolError> {
        if tool.name() == crate::question::QUESTION_TOOL_NAME
            || self
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

    /// Registry with websearch attached: Tavily when `ILAR_TAVILY_API_KEY`
    /// is set, otherwise the keyless Exa MCP endpoint so search works out
    /// of the box. Webfetch is already builtin.
    pub fn with_web_tools(self) -> Result<Self, DuplicateToolError> {
        match web::TavilyBackend::from_env() {
            Some(backend) => self.with_search(Box::new(backend)),
            None => self.with_search(Box::new(web::ExaBackend::from_env())),
        }
    }

    /// Registry with the models listing tool attached.
    pub fn with_models(
        self,
        models: Vec<&'static crate::model::ModelInfo>,
    ) -> Result<Self, DuplicateToolError> {
        self.with_tool(std::sync::Arc::new(models::ModelsTool::new(models)))
    }

    /// Registry with the service tool attached (shared per-session
    /// manager: services die when it drops).
    pub fn with_services(
        self,
        manager: std::sync::Arc<service::ServiceManager>,
    ) -> Result<Self, DuplicateToolError> {
        self.with_tool(std::sync::Arc::new(service::ServiceTool::new(manager)))
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

    /// Advertise structured questions to the provider for a root agent.
    ///
    /// The question definition is a protocol marker, not an executable tool:
    /// it is intentionally absent from [`Self::get`] and ordinary execution.
    pub fn with_questions(mut self, sender: crate::question::QuestionSender) -> Self {
        self.questions = Some(sender);
        self
    }

    pub(crate) fn question_sender(&self) -> Option<&crate::question::QuestionSender> {
        self.questions.as_ref()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .tools
            .iter()
            .map(|t| ToolDefinition {
                name: t.name().into(),
                description: t.description().into(),
                input_schema: t.input_schema(),
            })
            .collect::<Vec<_>>();
        if self.questions.is_some() {
            definitions.push(crate::question::question_tool_definition());
        }
        definitions
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

/// Run filesystem work on the blocking pool while holding the workspace
/// lease, so a dropped tool future cannot release the lease before the
/// I/O it authorised has actually stopped.
pub(crate) async fn run_blocking_io<T, F>(
    lease: std::sync::Arc<WorkspaceLease>,
    operation: F,
) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        operation()
    })
    .await
    .map_err(|error| std::io::Error::other(format!("blocking io task failed: {error}")))?
}

/// Reject a user-supplied path or pattern that would leave the directory
/// the tool was pointed at. `Path::join` on an absolute path silently
/// replaces the base, so without this a `path` of `/` walks the disk.
///
/// This is a blast-radius guard for accidents, not a security boundary —
/// ilar has no sandbox by design (see the README).
pub(crate) fn ensure_workspace_relative(requested: &str, tool: &str) -> Result<(), ToolOutput> {
    let escapes = std::path::Path::new(requested)
        .components()
        .any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
    if escapes {
        return Err(ToolOutput::error(format!(
            "{tool}: {requested:?} must stay within the workspace (no leading / or ..)"
        )));
    }
    Ok(())
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
    #[test]
    fn identifies_every_git_environment_variable() {
        assert!(super::is_git_environment_variable("GIT_DIR".as_ref()));
        assert!(super::is_git_environment_variable(
            "GIT_CONFIG_COUNT".as_ref()
        ));
        assert!(super::is_git_environment_variable(
            "GIT_CONFIG_KEY_0".as_ref()
        ));
        assert!(!super::is_git_environment_variable("PATH".as_ref()));
    }

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
