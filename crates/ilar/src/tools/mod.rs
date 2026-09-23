//! Built-in tools — see meta/issues/core-tools.md.

pub mod bash;
pub mod binary;
pub mod edit;
pub mod executor;
pub mod glob;
pub mod grep;
pub mod history;
pub mod image_gen;
pub mod models;
mod process;
pub mod read;
pub mod secrets_tool;
pub mod service;
pub mod sudo;
pub mod web;
pub mod write;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

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
        let (root, common_dir) = git_paths(&requested_cwd).await.map_err(|error| {
            anyhow::anyhow!(
                "workspace cwd {requested_cwd:?} is not inside a Git repository (expected the \
                 cwd of a registered Git worktree): {error:#}"
            )
        })?;
        // The anchor is the repository containing the requested path. A
        // session whose cwd sits inside a repository pins that repository:
        // the worktree must belong to it and be a different checkout. A
        // session whose cwd sits *above* its repositories pins none, so
        // any repository beneath its cwd anchors the request — cwd
        // `~/repos`, task path `~/repos/project` → worktree of `project`.
        match git_paths(parent.cwd()).await {
            Ok((parent_root, parent_common)) => {
                if root == parent_root {
                    anyhow::bail!(
                        "isolated workspace must use a different Git worktree: \
                         {requested_cwd:?} resolves to the parent's own checkout {parent_root:?}"
                    );
                }
                if common_dir != parent_common {
                    anyhow::bail!(
                        "isolated workspace must belong to the parent Git repository: \
                         {requested_cwd:?} belongs to {common_dir:?}, but the session's \
                         checkout {:?} belongs to {parent_common:?}",
                        parent.cwd(),
                    );
                }
            }
            Err(probe_error) => {
                // Only a genuine "no repository here" answer relaxes the
                // same-repository rules. A timeout, a killed git, or an
                // unreadable session cwd must not silently downgrade
                // validation to containment-only — that path could admit
                // the parent's own checkout under the parent's lock.
                if !format!("{probe_error:#}").contains("not a git repository") {
                    return Err(probe_error.context(format!(
                        "could not determine whether the session cwd {:?} is inside a Git \
                         repository",
                        parent.cwd(),
                    )));
                }
                if !common_dir.starts_with(parent.cwd()) {
                    anyhow::bail!(
                        "workspace cwd {requested_cwd:?} belongs to repository {common_dir:?}, \
                         which is outside the session cwd {:?} — the session cwd is in no Git \
                         repository, so only repositories beneath it can anchor a worktree",
                        parent.cwd(),
                    );
                }
            }
        }
        if !requested_cwd.starts_with(&root) {
            anyhow::bail!(
                "workspace cwd {requested_cwd:?} is outside its Git worktree root {root:?}"
            );
        }

        let output = git_output(&root, &["worktree", "list", "--porcelain", "-z"]).await?;
        let listed = output.split(|byte| *byte == 0).any(|field| {
            field
                .strip_prefix(b"worktree ")
                .and_then(|path| std::str::from_utf8(path).ok())
                .and_then(|path| std::fs::canonicalize(path).ok())
                .is_some_and(|path| path == root)
        });
        if !listed {
            anyhow::bail!(
                "workspace {root:?} is not a registered Git worktree of the repository at \
                 {common_dir:?}"
            );
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

/// The probe command: git with the caller's Git environment stripped
/// and its language pinned to C. The pin is not cosmetic — a failed
/// probe's stderr is *read* ("not a git repository" is what lets a
/// worktree relax to containment), and on a German machine git says
/// "kein Git-Repository", so an unpinned locale turns every
/// repositoryless session into the wrong refusal.
fn git_command(cwd: &std::path::Path, args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command.arg("-C").arg(cwd).args(args).kill_on_drop(true);
    for (key, _) in std::env::vars_os().filter(|(key, _)| is_git_environment_variable(key)) {
        command.env_remove(key);
    }
    command.env("LC_ALL", "C").env("LANG", "C");
    command
}

/// A `git` probe that has not answered in this long is not going to;
/// named and reported, because a bare "timed out" says nothing about
/// how long anyone waited.
const GIT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn git_output(cwd: &std::path::Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let mut command = git_command(cwd, args);
    let output = tokio::time::timeout(GIT_PROBE_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "git workspace validation timed out after {}",
                crate::text::format_duration(GIT_PROBE_TIMEOUT)
            )
        })??;
    if !output.status.success() {
        anyhow::bail!(
            "git workspace validation failed: {}",
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

    /// Reads are advisory: a reader never waits and never makes anyone
    /// wait — it runs in place, sees everything, and accepts that the
    /// tree may shift while it looks (the edit gate guards the write
    /// side of that bargain). Only mutators exclude each other, because
    /// two of them interleaving in one checkout is the failure nothing
    /// can repair after the fact.
    pub async fn acquire(&self, access: WorkspaceAccess) -> WorkspacePermit {
        match access {
            WorkspaceAccess::None | WorkspaceAccess::ReadOnly => WorkspacePermit::None,
            WorkspaceAccess::Mutating => WorkspacePermit::Mutating {
                _guard: self.lock().write_owned().await,
            },
        }
    }

    /// The permit if it is free right now; `None` means a mutable task
    /// holds the workspace and the caller will have to wait.
    pub fn try_acquire(&self, access: WorkspaceAccess) -> Option<WorkspacePermit> {
        match access {
            WorkspaceAccess::None | WorkspaceAccess::ReadOnly => Some(WorkspacePermit::None),
            WorkspaceAccess::Mutating => Some(WorkspacePermit::Mutating {
                _guard: self.lock().try_write_owned().ok()?,
            }),
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
        let permit = self.try_acquire(access)?;
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

/// The wait a caller can still hit now that reads are advisory and a
/// mutating tool is refused rather than held behind another job: a
/// mutable task behind another one in the same checkout (and, should a
/// concurrent mutating tool ever exist, one behind a sibling of its own
/// step). Every waiter says so with this sentence, on the
/// same channel a running tool reports its output tail on — silence
/// there reads as a hang, and docs/agents-and-skills.md promises the
/// row names itself.
pub const WORKSPACE_WAIT_NOTICE: &str = "waiting for the workspace — a mutable task holds it";

/// A bound [`WORKSPACE_WAIT_NOTICE`] reporter: the call id whose row is
/// waiting, and the sink that row is drawn from. `None` when nothing is
/// listening (a background job, a context without a UI).
#[derive(Clone)]
pub struct WorkspaceWaitNotice {
    call_id: String,
    sink: OutputTailSink,
}

impl WorkspaceWaitNotice {
    pub fn from_context(ctx: &ToolContext) -> Option<Self> {
        let (call_id, sink) = ctx.call_id.clone().zip(ctx.output_tail.clone())?;
        Some(Self { call_id, sink })
    }

    /// Announce the wait when anyone is listening. Called on the branch
    /// that is about to block, never speculatively: a notice on a wait
    /// that never happened is noise the row keeps showing.
    pub fn announce(notice: Option<&Self>) {
        if let Some(notice) = notice {
            notice
                .sink
                .report(&notice.call_id, WORKSPACE_WAIT_NOTICE.into());
        }
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

/// Progress signal for a background task's stall watchdog. A foreground
/// task blocks the turn that called it, so that turn's own event channel
/// is silent for as long as the child runs; the child touches this on
/// every event of its own instead, at any depth, so a busy descendant is
/// never mistaken for a hang.
#[derive(Clone, Debug)]
pub struct Heartbeat {
    last: Arc<std::sync::Mutex<std::time::Instant>>,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

impl Heartbeat {
    pub fn new() -> Self {
        Self {
            last: Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
        }
    }

    pub fn touch(&self) {
        *self.lock() = std::time::Instant::now();
    }

    /// Time since the last touch.
    pub fn elapsed(&self) -> std::time::Duration {
        self.lock().elapsed()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::time::Instant> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    /// The nearest background ancestor's stall watchdog, when this call
    /// runs under one. Inherited by foreground children; a background
    /// child starts its own.
    pub heartbeat: Option<Heartbeat>,
    /// The secret store and the grants given so far, for the tools
    /// that take `secrets`. `None` in a context built without one:
    /// every named secret is then refused.
    pub secrets: Option<crate::secrets::Secrets>,
    /// Paths no tool call in this session may name: the assistant's
    /// memory while it is sitting in a room, say, where withholding
    /// the memory *tools* still leaves `read` and `bash` pointed at
    /// the same files.
    ///
    /// Absolute paths: a relative one is compared against the absolute
    /// paths a walk and a resolved argument both carry, and would match
    /// nothing.
    ///
    /// A guard rail, not a boundary. It refuses a call that spells a
    /// withheld path out — which is what a model helpfully going to
    /// look does — and cannot stop a shell command that arrives at one
    /// by another spelling. The kernel sandbox
    /// (meta/issues/kernel-sandbox-for-tool-processes.md) is the only
    /// thing that can.
    pub withheld: Arc<[PathBuf]>,
}

/// What a call naming a withheld path is told. One sentence, and not
/// the path: a room that must not read the file must not be handed its
/// location either.
const WITHHELD_REFUSAL: &str = "that path is not available in this chat";

/// Whether any string in a call's arguments names a withheld path:
/// spelled out, so a `bash` command that cats the file is caught, or
/// resolved against `cwd`, so `../memory/USER.md` from the workspace
/// next door is the same answer as the absolute path. Lexical
/// resolution — the path need not exist, and a tool that is refused
/// must not first be allowed to probe the filesystem.
///
/// What it cannot catch is a call that never names the place it ends
/// up. The walkers step around their own subtrees — see
/// [`WithheldSubtrees`] — but a symlink to a withheld file, or a shell
/// that `cd`s first, arrives with nothing either of them can read. See
/// [`ToolContext::withheld`] for why that is the sandbox's job and not
/// this function's.
fn names_withheld(input: &serde_json::Value, cwd: &Path, withheld: &[PathBuf]) -> bool {
    let withheld = WithheldSubtrees::new(withheld);
    if withheld.is_empty() {
        return false;
    }
    let mut named = false;
    visit_strings(input, &mut |spelled| {
        named = named || withheld.named_by(spelled, cwd);
    });
    named
}

/// The withheld paths, as the two rules that read them need them.
/// [`names_withheld`] refuses a call that spells one out; a walk over
/// the parent spells nothing, so `grep` and `glob` pointed at the home
/// directory named nothing withheld and read the memory on their way
/// past. The walkers now step around these subtrees the way they step
/// around `.git` — silently, because a room that may not read the
/// memory may not be told it is there either.
///
/// Empty in every session that withholds nothing, which is the one to
/// stay cheap for: [`WithheldSubtrees::admits`] is then a length check
/// per walked entry.
///
/// The paths are expected absolute, as everything they are compared
/// against is: see [`ToolContext::withheld`].
#[derive(Clone)]
pub(crate) struct WithheldSubtrees {
    roots: Vec<PathBuf>,
}

impl WithheldSubtrees {
    pub(crate) fn new(withheld: &[PathBuf]) -> Self {
        Self {
            roots: withheld
                .iter()
                // An empty path is under every path: it would hide the
                // whole session rather than one directory of it.
                .filter(|path| !path.as_os_str().is_empty())
                .map(|path| lexically_normal(path))
                .collect(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Whether a walked path may be descended into and reported. The
    /// candidate is normalised because a walk's own root carries
    /// whatever the caller wrote — `grep` at `..` yields
    /// `<cwd>/../memory/USER.md`, which starts with no withheld path
    /// until the `..` is spent.
    pub(crate) fn admits(&self, path: &Path) -> bool {
        if self.is_empty() {
            return true;
        }
        let resolved = lexically_normal(path);
        !self.roots.iter().any(|root| within(&resolved, root))
    }

    /// Whether a string a call handed us names one: resolved against
    /// `cwd`, or spelled out inside something longer, which is how a
    /// `bash` command line names its paths.
    fn named_by(&self, spelled: &str, cwd: &Path) -> bool {
        if !self.admits(&cwd.join(spelled)) {
            return true;
        }
        let spelled = spelled.to_ascii_lowercase();
        self.roots.iter().any(|root| {
            root.to_str()
                .is_some_and(|root| spelled.contains(&root.to_ascii_lowercase()))
        })
    }
}

/// Whether `path` is `root` or sits under it: component by component,
/// so `/h/memory` does not claim `/h/memory-notes`, and without regard
/// to ASCII case.
///
/// The case rule is not politeness. The filesystem this most often runs
/// on does not distinguish either, and `canonicalize` does not correct a
/// capital: `<home>/Memory` opens the directory withheld as
/// `<home>/memory`, and one shifted letter is well inside "a model that
/// goes looking". What it costs is a sibling differing from a withheld
/// path only in case — which cannot exist on the filesystem where the
/// rule is needed.
fn within(path: &Path, root: &Path) -> bool {
    let mut parts = path.components();
    root.components().all(|wanted| {
        parts
            .next()
            .is_some_and(|part| part.as_os_str().eq_ignore_ascii_case(wanted.as_os_str()))
    })
}

/// Every string in a JSON value, at any depth: a tool's arguments are
/// its own shape, and the one thing they have in common is that a path
/// arrives as a string somewhere in them.
fn visit_strings(value: &serde_json::Value, visit: &mut impl FnMut(&str)) {
    match value {
        serde_json::Value::String(text) => visit(text),
        serde_json::Value::Array(items) => items.iter().for_each(|item| visit_strings(item, visit)),
        serde_json::Value::Object(fields) => fields
            .values()
            .for_each(|field| visit_strings(field, visit)),
        _ => {}
    }
}

/// `a/b/../c` as `a/c`, without touching the disk. `..` at the root is
/// the root, as the kernel would have it.
fn lexically_normal(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                normal.pop();
            }
            std::path::Component::CurDir => {}
            part => normal.push(part),
        }
    }
    normal
}

impl ToolContext {
    /// Context for a root (non-subagent) session. Panics on a cwd that
    /// cannot be resolved — for tests and callers that own the path.
    /// Anything built from a *stored* cwd (a resumed session's, a
    /// worktree that may since have been deleted) wants
    /// [`ToolContext::try_root`], which refuses instead of aborting the
    /// process.
    pub fn root(cwd: std::path::PathBuf) -> Self {
        Self::try_root(cwd).unwrap_or_else(|error| panic!("{error:#}"))
    }

    /// Context for a root session, refusing a cwd that is not there.
    pub fn try_root(cwd: std::path::PathBuf) -> anyhow::Result<Self> {
        let location = WorkspaceLocation::try_shared(cwd)?;
        Ok(Self {
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
            heartbeat: None,
            secrets: None,
            withheld: Arc::from(Vec::new()),
        })
    }

    /// Context with the secret store attached.
    pub fn with_secrets(mut self, secrets: crate::secrets::Secrets) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Context in which no tool call may name `paths`.
    pub fn with_withheld(mut self, paths: Arc<[PathBuf]>) -> Self {
        self.withheld = paths;
        self
    }

    /// The refusal a call earns for naming a withheld path, or `None`
    /// when it names none. The refusal does not repeat the path: the
    /// model already knew it, and the chat need not learn it.
    pub fn withheld_refusal(&self, input: &serde_json::Value) -> Option<&'static str> {
        names_withheld(input, &self.cwd, &self.withheld).then_some(WITHHELD_REFUSAL)
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

    /// The secrets a call named, each granted or the whole call
    /// refused. `detail` is what the person reads before saying yes:
    /// the command, verbatim. A context without a store refuses any
    /// name at all.
    pub async fn grant_secrets(
        &self,
        tool: &str,
        names: &[String],
        detail: &str,
    ) -> Result<Vec<crate::secrets::Granted>, String> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let Some(secrets) = self.secrets.as_ref() else {
            return Err(format!(
                "{tool}: this session has no secret store, so {} cannot be provided",
                names.join(", ")
            ));
        };
        secrets
            .resolve(crate::secrets::Request {
                tool,
                names,
                detail,
                session_id: &self.session_id,
                tool_call_id: self.call_id.as_deref(),
                cancel: &self.cancel,
            })
            .await
            .map_err(|error| format!("{tool}: {error}"))
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

    /// The same output with every stored secret value replaced in its
    /// text, and anything this call's own asks had to admit — an Always
    /// the store would not keep — appended. A sealed store the scrub
    /// could not see is admitted too, once per runtime. Images and
    /// state are untouched.
    pub fn scrubbed(mut self, secrets: &crate::secrets::Secrets, call_id: Option<&str>) -> Self {
        let (content, sealed) = secrets.scrub_admitting(&self.content);
        self.content = content;
        for note in secrets.take_notes(call_id).into_iter().chain(sealed) {
            self.content.push_str(&format!("\n({note})"));
        }
        self
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
    /// The access this one call needs, for a tool whose actions differ:
    /// reading a service's logs does not change the checkout, starting
    /// one does. Defaults to the tool's.
    fn workspace_access_for(&self, _input: &serde_json::Value) -> WorkspaceAccess {
        self.workspace_access()
    }
    fn supports_background(&self) -> bool {
        false
    }
    fn manages_workspace_access(&self) -> bool {
        false
    }
    fn accepts_executor_workspace_lease(&self) -> bool {
        false
    }
    /// Whether this tool belongs in the schema the model is shown right
    /// now. Almost every tool is there for the whole session and says
    /// so by saying nothing. A tool whose reason to exist can appear or
    /// vanish mid-session answers for itself, rather than being decided
    /// once when the registry was built.
    ///
    /// Hidden, not disabled: [`ToolRegistry::get`] ignores this, so a
    /// model working from a schema fetched a turn ago — or from another
    /// tool's description, which names its companions unconditionally —
    /// still gets a real answer rather than "no such tool". A tool that
    /// hides itself owes a sensible reply to a call that arrives anyway.
    fn is_published(&self) -> bool {
        true
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
    /// The session's service manager, when the service tool is
    /// installed: compaction asks it what is still running.
    services: Option<Arc<service::ServiceManager>>,
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
    pub const TASK_MESSAGE: Self = Self("task_message");
    pub const SERVICE: Self = Self("service");
    pub const MODELS: Self = Self("models");
    pub const HISTORY: Self = Self("history");
    pub const MEMORY: Self = Self("memory");
    pub const MEMORY_SEARCH: Self = Self("memory_search");
    pub const MEMORY_GET: Self = Self("memory_get");
    pub const IMAGE_GEN: Self = Self("image_gen");
    pub const SECRETS: Self = Self("secrets");
    pub const SUDO: Self = Self("sudo");

    /// Every non-builtin tool an allowlist may name.
    pub const ALL: &'static [Self] = &[
        Self::TASK,
        Self::TASKS,
        Self::TASK_MESSAGE,
        Self::SERVICE,
        Self::MODELS,
        Self::HISTORY,
        Self::MEMORY,
        Self::MEMORY_SEARCH,
        Self::MEMORY_GET,
        Self::IMAGE_GEN,
        Self::SECRETS,
        Self::SUDO,
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
            services: None,
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
            services: None,
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    pub fn tool_names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    /// The names the model was actually offered: what [`Self::definitions`]
    /// publishes, plus `question` when a frontend is attached — the
    /// registry's own tool list cannot hold that one. Not
    /// [`Self::tool_names`], which is every *registered* tool, including
    /// any that [`Tool::is_published`] hides right now.
    ///
    /// This is the list to answer "what could the model have called".
    pub fn published_tool_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self
            .tools
            .iter()
            .filter(|tool| tool.is_published())
            .map(|tool| tool.name())
            .collect();
        if self.questions.is_some() {
            names.push(crate::question::QUESTION_TOOL_NAME);
        }
        names
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
        self.add(tool)?;
        Ok(self)
    }

    /// Add a tool to a registry that already exists — a driver adding
    /// its own to a runtime's after start.
    pub fn add(&mut self, tool: Arc<dyn Tool>) -> Result<(), DuplicateToolError> {
        if tool.name() == crate::question::QUESTION_TOOL_NAME
            || self
                .tools
                .iter()
                .any(|existing| existing.name() == tool.name())
        {
            return Err(DuplicateToolError(tool.name()));
        }
        self.tools.push(tool);
        Ok(())
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

    /// Registry with the secrets listing attached. When it shows itself
    /// is [`secrets_tool::SecretsTool`]'s own business.
    pub fn with_secrets(
        self,
        store: crate::secrets::SecretStore,
    ) -> Result<Self, DuplicateToolError> {
        self.with_child_tool(
            ChildTool::SECRETS,
            std::sync::Arc::new(secrets_tool::SecretsTool::new(store)),
        )
    }

    /// Registry with the sudo tool attached — for a configuration that
    /// turned it on (`agent.sudo`).
    pub fn with_sudo(self) -> Result<Self, DuplicateToolError> {
        self.with_child_tool(
            ChildTool::SUDO,
            std::sync::Arc::new(sudo::SudoTool::default()),
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

    /// Registry that remembers across sessions: the core files and the
    /// archive under one store, three tools. Root sessions only, like
    /// history — a subagent's memory would be its parent's.
    pub fn with_memory(
        self,
        store: Arc<crate::memory::MemoryStore>,
    ) -> Result<Self, DuplicateToolError> {
        use crate::memory::{MemoryGetTool, MemorySearchTool, MemoryTool};
        self.with_child_tool(ChildTool::MEMORY, MemoryTool::new(store.clone()))?
            .with_child_tool(
                ChildTool::MEMORY_SEARCH,
                MemorySearchTool::new(store.clone()),
            )?
            .with_child_tool(ChildTool::MEMORY_GET, MemoryGetTool::new(store))
    }

    /// Registry with image generation attached — installed when the
    /// openai provider is configured, on the credentials it has.
    pub fn with_image_gen(
        self,
        backend: image_gen::ImageGenBackend,
    ) -> Result<Self, DuplicateToolError> {
        self.with_child_tool(
            ChildTool::IMAGE_GEN,
            std::sync::Arc::new(image_gen::ImageGenTool::new(backend)),
        )
    }

    /// Registry with the service tool attached (shared per-session
    /// manager: services die when it drops).
    pub fn with_services(
        self,
        manager: std::sync::Arc<service::ServiceManager>,
    ) -> Result<Self, DuplicateToolError> {
        let mut registry = self.with_child_tool(
            ChildTool::SERVICE,
            std::sync::Arc::new(service::ServiceTool::new(manager.clone())),
        )?;
        registry.services = Some(manager);
        Ok(registry)
    }

    /// The services still running, one line each (`name · command`), or
    /// nothing when no service tool is installed. What compaction
    /// carries so an agent does not start a second dev server after a
    /// handover.
    pub fn running_services(&self) -> Vec<String> {
        self.services
            .as_ref()
            .map(|manager| {
                manager
                    .running_services()
                    .into_iter()
                    .map(|(name, command)| format!("{name} · {command}"))
                    .collect()
            })
            .unwrap_or_default()
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
            Arc::new(crate::subagent::TasksTool::new(spawner.clone())),
        )?
        .with_child_tool(
            ChildTool::TASK_MESSAGE,
            Arc::new(crate::subagent::TaskMessageTool::new(spawner)),
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
            .filter(|t| t.is_published())
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

/// What every file-taking tool says about its `path`, once: read,
/// write and edit all resolve a relative path from cwd and take an
/// absolute one as it stands, and three different sentences about that
/// read as three different rules.
pub(crate) const PATH_DESCRIPTION: &str = "Relative to cwd, or absolute";

/// Parse tool input; on failure return a ToolOutput error instead of
/// panicking (malformed model output must not crash the loop).
///
/// One shape for every tool error, `tool: message`, so a model reading
/// its own failures does not have to learn a per-tool dialect.
pub fn parse_input<T: serde::de::DeserializeOwned>(
    input: serde_json::Value,
    tool_name: &str,
) -> Result<T, ToolOutput> {
    serde_json::from_value(input)
        .map_err(|e| ToolOutput::error(format!("{tool_name}: invalid input: {e}")))
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
        child_tool_names_from, git_command,
    };
    use crate::session::ImageContent;

    fn image(bytes: usize) -> ImageContent {
        ImageContent::new("image/png", &vec![0u8; bytes])
    }

    /// The one comparison two rules share: a call's arguments and every
    /// entry of a walk. Its edges are where a guard rail of this shape
    /// usually breaks — a sibling that merely starts with the same
    /// letters, a shifted capital on a filesystem that does not care.
    #[test]
    fn a_withheld_subtree_claims_itself_and_nothing_beside_it() {
        use super::WithheldSubtrees;
        use std::path::{Path, PathBuf};

        let withheld = WithheldSubtrees::new(&[PathBuf::from("/home/a/memory")]);
        for inside in [
            "/home/a/memory",
            "/home/a/memory/USER.md",
            "/home/a/./memory/notes/2026.md",
            "/home/a/workspace/../memory/USER.md",
            // One capital letter opens the same directory on the
            // filesystem this most often runs on.
            "/home/a/Memory/USER.md",
        ] {
            assert!(!withheld.admits(Path::new(inside)), "{inside} got in");
        }
        for outside in [
            "/home/a/memory-notes/x.md",
            "/home/a/memoryx",
            "/home/a/SOUL.md",
            "/home/b/memory/USER.md",
        ] {
            assert!(withheld.admits(Path::new(outside)), "{outside} was refused");
        }

        // A withheld *file* is one path, not a prefix of its neighbours.
        let file = WithheldSubtrees::new(&[PathBuf::from("/home/a/SOUL.md")]);
        assert!(!file.admits(Path::new("/home/a/SOUL.md")));
        assert!(file.admits(Path::new("/home/a/SOUL.mdx")));

        // Nothing withheld admits everything — the cheap path a
        // terminal session's every walk takes.
        let nothing = WithheldSubtrees::new(&[PathBuf::new()]);
        assert!(nothing.is_empty(), "an empty path is under every path");
        assert!(nothing.admits(Path::new("/anywhere/at/all")));
    }

    /// The validator reads git's stderr to decide, so it must be git's
    /// English, not the operator's language.
    #[test]
    fn the_git_probe_speaks_c() {
        let command = git_command(std::path::Path::new("/tmp"), &["rev-parse"]);
        let pinned: Vec<_> = command
            .as_std()
            .get_envs()
            .filter(|(key, _)| *key == "LC_ALL" || *key == "LANG")
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(
            pinned.contains(&("LC_ALL".to_string(), Some("C".to_string())))
                && pinned.contains(&("LANG".to_string(), Some("C".to_string()))),
            "the probe ran in the caller's locale: {pinned:?}"
        );
    }

    /// The one-sentence rule: mutable work excludes mutable work;
    /// read-only work runs in place, sees everything, blocks nothing,
    /// and accepts that the tree may shift while it looks.
    #[tokio::test]
    async fn read_leases_are_advisory_and_writers_exclude_writers() {
        use super::{WorkspaceAccess, WorkspaceScheduler};
        let scheduler = WorkspaceScheduler::new();

        let reader = scheduler.acquire_lease(WorkspaceAccess::ReadOnly).await;
        // A held read lease blocks nobody, writer included…
        let writer = scheduler
            .try_acquire(WorkspaceAccess::Mutating)
            .expect("a reader must not block a writer");
        // …and a held write lease blocks no reader, only the next writer.
        assert!(scheduler.try_acquire(WorkspaceAccess::ReadOnly).is_some());
        assert!(scheduler.try_acquire(WorkspaceAccess::Mutating).is_none());
        drop(writer);
        assert!(scheduler.try_acquire(WorkspaceAccess::Mutating).is_some());
        drop(reader);
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

    /// Every tool error reads `tool: …`, the input ones included: five
    /// tools used to hand-roll "invalid input for X" instead.
    #[test]
    fn a_malformed_input_is_refused_in_the_one_shape() {
        let refusal = super::parse_input::<std::collections::HashMap<String, String>>(
            serde_json::json!([1, 2]),
            "todo",
        )
        .expect_err("an array is not an object");
        assert!(refusal.is_error);
        assert!(
            refusal.content.starts_with("todo: invalid input: "),
            "{}",
            refusal.content
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
