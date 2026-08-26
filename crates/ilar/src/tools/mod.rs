//! Built-in tools — see meta/issues/core-tools.md.

pub mod bash;
pub mod binary;
pub mod edit;
pub mod executor;
pub mod glob;
pub mod grep;
pub mod history;
pub mod models;
mod process;
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

/// Files past this size are never tracked in [`SeenFiles`]: `edit`
/// refuses to load them at all (same cap), so hashing them would buy
/// nothing but a second pass over the disk.
pub(crate) const MAX_TRACKED_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// What the model has actually been shown of the workspace: canonical
/// path → SHA-256 of the file's contents at the moment it saw them.
/// Content identity, not wall-clock: a file rewritten to the same bytes
/// is still the file the model read.
///
/// Every clone of a session's [`ToolContext`] shares one map, so a read
/// in one tool call licenses an edit in the next. A subagent starts with
/// an empty one — the child model has seen nothing — and compaction
/// empties it, because the summary truncated the model's memory of what
/// the files said.
#[derive(Clone, Default)]
pub struct SeenFiles {
    inner: Arc<std::sync::Mutex<SeenFilesState>>,
}

#[derive(Default)]
struct SeenFilesState {
    digests: HashMap<PathBuf, [u8; 32]>,
    /// The compaction this map has already reacted to (see
    /// [`SeenFiles::forget_after_compaction`]).
    compaction: Option<String>,
}

impl SeenFiles {
    /// Record contents the caller already holds (write, edit). Contents
    /// past [`MAX_TRACKED_FILE_BYTES`] are dropped, so the map never
    /// claims to have seen a file `edit` would refuse to open anyway.
    pub(crate) fn record(&self, path: &std::path::Path, contents: &[u8]) {
        if contents.len() as u64 > MAX_TRACKED_FILE_BYTES {
            return;
        }
        self.inner
            .lock()
            .unwrap()
            .digests
            .insert(canonical_key(path), digest(contents));
    }

    /// Record whatever is on disk right now (read, which streams a window
    /// and so never holds the whole file). Silently does nothing when the
    /// file cannot be read or is past [`MAX_TRACKED_FILE_BYTES`] — the
    /// stat is what keeps an oversized file from being loaded at all.
    /// Failing to record only means the next edit asks for a re-read.
    pub(crate) fn record_from_disk(&self, path: &std::path::Path) {
        let Ok(metadata) = std::fs::metadata(path) else {
            return;
        };
        if metadata.len() > MAX_TRACKED_FILE_BYTES {
            return;
        }
        if let Ok(contents) = std::fs::read(path) {
            self.record(path, &contents);
        }
    }

    pub(crate) fn digest_of(&self, path: &std::path::Path) -> Option<[u8; 32]> {
        self.inner
            .lock()
            .unwrap()
            .digests
            .get(&canonical_key(path))
            .copied()
    }

    /// Drop everything when the session's latest compaction is not the
    /// one this map last reacted to. An identity, not a count: a loaded
    /// session carries only the events after its replay checkpoint, and
    /// publishing that checkpoint drops every compaction but the last, so
    /// counting them would stop noticing after the first. One comparison
    /// covers every path that can compact a session — the turn's own
    /// threshold check and a manual `/compact` between turns alike.
    pub(crate) fn forget_after_compaction(&self, latest: Option<&str>) {
        let mut state = self.inner.lock().unwrap();
        if state.compaction.as_deref() != latest {
            state.compaction = latest.map(str::to_string);
            state.digests.clear();
        }
    }
}

/// Best-effort canonical path. A path that cannot be resolved (the file
/// is gone, a permission error) keys on itself: two lookups of the same
/// unresolvable path still agree, which is all the map needs.
fn canonical_key(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn digest(contents: &[u8]) -> [u8; 32] {
    <sha2::Sha256 as sha2::Digest>::digest(contents).into()
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
    /// Whether the model about to receive this call's result can see
    /// images. Set per turn from the session's own model, so a tool
    /// producing an image never hands one to a model that would drop it.
    pub vision: bool,
    /// Files this session has shown the model, and what they said at the
    /// time. `edit` refuses to touch anything absent or stale here.
    pub seen_files: SeenFiles,
    /// Directory oversized tool output is written to, so the model can
    /// grep what did not fit in its result. `None` — a context with no
    /// state directory behind it — simply truncates as before.
    pub spill_dir: Option<PathBuf>,
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
            vision: false,
            seen_files: SeenFiles::default(),
            spill_dir: None,
        }
    }

    /// Context that may spill oversized tool output into `dir`.
    pub fn with_spill_dir(mut self, dir: PathBuf) -> Self {
        self.spill_dir = Some(dir);
        self
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

/// Decoded image bytes one tool result may carry. Enforced here rather
/// than in any one tool, so every image-producing tool inherits it: a
/// single result big enough to blow the request budget is a bug the
/// model cannot see coming.
pub const MAX_RESULT_IMAGE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Images the result carries, already within [`MAX_RESULT_IMAGE_BYTES`]:
    /// private so the cap cannot be bypassed by assignment.
    images: Vec<crate::session::ImageContent>,
    child_session_id: Option<String>,
    state: Option<crate::session::SessionState>,
    /// Boxed: one tool in the tree ever sets it, and `ToolOutput` is an
    /// `Err` variant all over the tools — the cold field pays for itself.
    pending_state_commit: Option<Box<PendingStateCommit>>,
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
            // Sizes, not payloads: a base64 screenshot in a panic
            // message helps nobody.
            .field(
                "images",
                &self
                    .images
                    .iter()
                    .map(crate::session::ImageContent::byte_len)
                    .collect::<Vec<_>>(),
            )
            .field("child_session_id", &self.child_session_id)
            .field("state", &self.state)
            .finish()
    }
}

impl PartialEq for ToolOutput {
    fn eq(&self, other: &Self) -> bool {
        self.content == other.content
            && self.is_error == other.is_error
            && self.images == other.images
            && self.child_session_id == other.child_session_id
            && self.state == other.state
    }
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images: Vec::new(),
            child_session_id: None,
            state: None,
            pending_state_commit: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            images: Vec::new(),
            child_session_id: None,
            state: None,
            pending_state_commit: None,
        }
    }

    pub fn images(&self) -> &[crate::session::ImageContent] {
        &self.images
    }

    pub(crate) fn take_images(&mut self) -> Vec<crate::session::ImageContent> {
        std::mem::take(&mut self.images)
    }

    /// Attach images, keeping the result within [`MAX_RESULT_IMAGE_BYTES`].
    /// Images are taken in order until one does not fit; that one and
    /// everything after it are dropped whole — half an image is worth
    /// nothing to a vision model — and a single note naming them is
    /// appended to the text, because the model's only account of what it
    /// is not being shown is the text it gets back.
    pub fn with_images(mut self, images: Vec<crate::session::ImageContent>) -> Self {
        let mut budget = MAX_RESULT_IMAGE_BYTES;
        let mut kept = Vec::with_capacity(images.len());
        let mut dropped = Vec::new();
        for (index, image) in images.into_iter().enumerate() {
            match budget.checked_sub(image.byte_len()) {
                Some(remaining) if dropped.is_empty() => {
                    budget = remaining;
                    kept.push(image);
                }
                _ => dropped.push(format!(
                    "image {} ({}, {} KiB)",
                    index + 1,
                    image.media_type,
                    image.byte_len() / 1024
                )),
            }
        }
        if !dropped.is_empty() {
            self.content.push_str(&format!(
                "\n[dropped {}: a tool result carries at most {} MiB of images]",
                dropped.join(", "),
                MAX_RESULT_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        self.images = kept;
        self
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

    /// Append a trailing note to the content, whether it is a result or
    /// an error — an error the model can act on still needs the note.
    pub(crate) fn with_appended_text(mut self, text: &str) -> Self {
        self.content.push_str(text);
        self
    }

    pub(crate) fn with_todo_state(
        mut self,
        target: std::sync::Arc<std::sync::Mutex<crate::todo::TodoList>>,
        list: crate::todo::TodoList,
    ) -> Self {
        self.state = Some(crate::session::SessionState::TodoList { list: list.clone() });
        self.pending_state_commit = Some(Box::new(PendingStateCommit { target, list }));
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

/// A tool an agent `tools:` allowlist may name on top of the builtins.
/// Every `ToolRegistry::with_*` constructor that installs one names its
/// entry here, and the allowlist below is read from the same list — so
/// what agents may ask for is what the constructors build. An entry a
/// child registry never receives (history is installed for the root
/// session only) is simply never granted: allowlists intersect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildTool(&'static str);

impl ChildTool {
    pub const TASK: Self = Self("task");
    pub const TASKS: Self = Self("tasks");
    pub const SERVICE: Self = Self("service");
    pub const MODELS: Self = Self("models");
    pub const HISTORY: Self = Self("history");

    /// Every non-builtin tool an allowlist may name.
    pub const ALL: &'static [Self] = &[
        Self::TASK,
        Self::TASKS,
        Self::SERVICE,
        Self::MODELS,
        Self::HISTORY,
    ];

    pub const fn name(self) -> &'static str {
        self.0
    }
}

/// Tool names an agent `tools:` allowlist may reference — everything a
/// child registry can contain: the builtin registry's own tools plus
/// [`ChildTool::ALL`].
pub fn child_tool_names() -> Vec<&'static str> {
    child_tool_names_from(ChildTool::ALL)
}

fn child_tool_names_from(optional: &[ChildTool]) -> Vec<&'static str> {
    let mut names = ToolRegistry::builtin().tool_names();
    names.extend(optional.iter().map(|tool| tool.name()));
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

    /// Registry with an optional child tool attached. The [`ChildTool`]
    /// entry is what the allowlist publishes; the assertion keeps the
    /// published name and the registered one from parting ways.
    fn with_child_tool(
        self,
        kind: ChildTool,
        tool: Arc<dyn Tool>,
    ) -> Result<Self, DuplicateToolError> {
        debug_assert_eq!(
            kind.name(),
            tool.name(),
            "child tool registered under another name"
        );
        self.with_tool(tool)
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
        self.with_child_tool(
            ChildTool::MODELS,
            std::sync::Arc::new(models::ModelsTool::new(models)),
        )
    }

    /// Registry that can search its own session's past — everything
    /// ever said, not just what is still in the window.
    pub fn with_history(
        self,
        store: crate::session::SessionStore,
    ) -> Result<Self, DuplicateToolError> {
        self.with_child_tool(
            ChildTool::HISTORY,
            std::sync::Arc::new(history::HistoryTool::new(store)),
        )
    }

    /// Registry with the service tool attached (shared per-session
    /// manager: services die when it drops).
    pub fn with_services(
        self,
        manager: std::sync::Arc<service::ServiceManager>,
    ) -> Result<Self, DuplicateToolError> {
        self.with_child_tool(
            ChildTool::SERVICE,
            std::sync::Arc::new(service::ServiceTool::new(manager)),
        )
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
        self.with_child_tool(
            ChildTool::TASK,
            Arc::new(crate::subagent::TaskTool::new(spawner.clone())),
        )?
        .with_child_tool(
            ChildTool::TASKS,
            Arc::new(crate::subagent::TasksTool::new(spawner)),
        )
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
    use super::{
        ChildTool, MAX_RESULT_IMAGE_BYTES, ToolOutput, ToolRegistry, child_tool_names,
        child_tool_names_from,
    };
    use crate::session::ImageContent;

    fn image(bytes: usize) -> ImageContent {
        ImageContent::new("image/png", &vec![0u8; bytes])
    }

    #[test]
    fn images_under_the_cap_ride_through_untouched() {
        let images = vec![image(1024), image(2048)];
        let output = ToolOutput::text("looked at it").with_images(images.clone());

        assert_eq!(output.content, "looked at it");
        assert_eq!(output.images(), images);
    }

    /// A truncated image is not an image, so the cap drops whole ones —
    /// and the model is told, in the only channel it can read.
    #[test]
    fn images_over_the_cap_are_dropped_whole_and_named_in_the_text() {
        let big = image(4 * 1024 * 1024);
        let output = ToolOutput::text("looked at it").with_images(vec![
            big.clone(),
            big.clone(),
            image(1024),
        ]);

        assert_eq!(output.images(), [big]);
        let note = output
            .content
            .strip_prefix("looked at it\n")
            .unwrap_or_else(|| panic!("no note appended: {:?}", output.content));
        assert!(!note.contains('\n'), "{note:?}");
        assert!(note.contains("image 2"), "{note:?}");
        assert!(note.contains("image 3"), "{note:?}");
        assert!(note.contains("image/png"), "{note:?}");
        assert!(
            note.contains(&format!("{} MiB", MAX_RESULT_IMAGE_BYTES / (1024 * 1024))),
            "{note:?}"
        );
    }

    #[test]
    fn a_root_tool_context_has_no_vision_until_a_turn_says_otherwise() {
        assert!(!super::ToolContext::root(std::env::temp_dir()).vision);
    }

    /// Content identity, not wall-clock: a file rewritten to the same
    /// bytes is still the file the model read.
    #[test]
    fn seen_files_track_contents_not_moments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "alpha\n").unwrap();
        let seen = super::SeenFiles::default();
        assert_eq!(seen.digest_of(&path), None);

        seen.record_from_disk(&path);
        let digest = seen.digest_of(&path).expect("recorded");
        std::fs::write(&path, "alpha\n").unwrap();
        seen.record_from_disk(&path);
        assert_eq!(seen.digest_of(&path), Some(digest));

        std::fs::write(&path, "beta\n").unwrap();
        seen.record_from_disk(&path);
        assert_ne!(seen.digest_of(&path), Some(digest));
    }

    /// A path that resolves to the same file is the same entry, however
    /// the tool call spelled it.
    #[test]
    fn seen_files_key_on_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "alpha\n").unwrap();
        let seen = super::SeenFiles::default();

        std::fs::create_dir(dir.path().join("sub")).unwrap();

        seen.record_from_disk(&path);

        assert!(seen.digest_of(&dir.path().join("./a.txt")).is_some());
        assert!(seen.digest_of(&dir.path().join("sub/../a.txt")).is_some());
    }

    #[test]
    fn a_file_past_the_tracking_cap_is_never_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        std::fs::write(
            &path,
            vec![b'x'; super::MAX_TRACKED_FILE_BYTES as usize + 1],
        )
        .unwrap();
        let seen = super::SeenFiles::default();

        seen.record_from_disk(&path);

        assert_eq!(seen.digest_of(&path), None);
    }

    /// Which compaction it is, not how many there have been: a session
    /// loaded from its replay checkpoint has forgotten the earlier ones,
    /// so a map that reacted to "the second" would never fire again.
    #[test]
    fn seen_files_are_dropped_when_the_latest_compaction_changes() {
        let seen = super::SeenFiles::default();
        let path = std::path::Path::new("/nonexistent/a.txt");
        seen.record(path, b"alpha");

        seen.forget_after_compaction(None);
        assert!(seen.digest_of(path).is_some(), "an uncompacted session");

        seen.forget_after_compaction(Some("compaction-1"));
        assert_eq!(seen.digest_of(path), None);

        seen.record(path, b"alpha");
        seen.forget_after_compaction(Some("compaction-1"));
        assert!(seen.digest_of(path).is_some(), "the same compaction twice");

        seen.forget_after_compaction(Some("compaction-2"));
        assert_eq!(seen.digest_of(path), None, "a later compaction");
    }

    #[test]
    fn one_table_entry_is_all_a_new_child_tool_needs() {
        // A hypothetical tool added to the table alone is allowlistable;
        // nothing else in this module lists tool names.
        const TELEPORT: ChildTool = ChildTool("teleport");
        let names = child_tool_names_from(&[ChildTool::TASK, TELEPORT]);
        assert_eq!(
            names,
            [
                ToolRegistry::builtin().tool_names(),
                vec!["task", "teleport"]
            ]
            .concat()
        );
    }

    #[test]
    fn the_published_allowlist_is_the_builtin_registry_plus_the_table() {
        let builtin = ToolRegistry::builtin().tool_names();
        let expected = [
            builtin,
            ChildTool::ALL.iter().map(|tool| tool.name()).collect(),
        ]
        .concat();
        assert_eq!(child_tool_names(), expected);
    }

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
