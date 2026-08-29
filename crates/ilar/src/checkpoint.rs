//! Shadow git snapshots of the working tree, one per user turn.
//!
//! A snapshot is a commit built through a temporary index: the user's
//! real index, HEAD, branch, and working tree are never touched, and
//! ignored files are never captured. Each session's snapshots form a
//! chain under `refs/ilar/checkpoints/<session-id>`, which keeps them
//! reachable and out of `git gc`'s reach without appearing anywhere in
//! the user's log or reflog.
//!
//! Known limitations, both standard `git add -A` semantics: submodules
//! and embedded repositories are captured as bare gitlinks (their
//! content is not snapshotted), and in a sparse checkout the files
//! outside the sparse cone are recorded as deleted.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;

/// One captured working-tree state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSnapshot {
    /// The shadow commit holding the tree.
    pub commit: String,
    /// Repository HEAD at capture time; `None` on an unborn branch.
    pub head: Option<String>,
}

/// Snapshot the working tree at `cwd` into the repository's object
/// database. Returns `Ok(None)` when `cwd` is not inside a git
/// repository; the caller treats any `Err` as "no snapshot this turn"
/// rather than failing the turn.
pub async fn snapshot(cwd: &Path, session_id: &str) -> anyhow::Result<Option<TreeSnapshot>> {
    // Synchronous pre-check: outside a repository the snapshot must cost
    // nothing — no subprocess, and no await that shifts the turn's poll
    // schedule.
    if !cwd.ancestors().any(|path| path.join(".git").exists()) {
        return Ok(None);
    }
    if git(cwd, NO_ENV, &["rev-parse", "--git-dir"]).await.is_err() {
        return Ok(None);
    }
    let head = git_optional(cwd, &["rev-parse", "--verify", "HEAD"]).await;

    let index = TempIndex::new(session_id)?;
    // Seed from HEAD so tracked-but-ignored files stay followed;
    // `add -A` then folds in the working tree (ignore rules only apply
    // to untracked files, which is exactly the capture rule we want).
    if head.is_some() {
        git(cwd, index.env(), &["read-tree", "HEAD"]).await?;
    }
    git(cwd, index.env(), &["add", "-A"]).await?;
    let tree = git(cwd, index.env(), &["write-tree"]).await?;

    let reference = checkpoint_ref(session_id);
    let parent = git_optional(cwd, &["rev-parse", "--verify", &reference]).await;
    let message = format!("ilar checkpoint {session_id}");
    let mut commit_args = vec!["commit-tree", tree.as_str(), "-m", message.as_str()];
    if let Some(parent) = parent.as_deref() {
        commit_args.extend(["-p", parent]);
    }
    let commit = git(cwd, IDENTITY_ENV, &commit_args).await?;
    git(cwd, NO_ENV, &["update-ref", &reference, &commit]).await?;
    Ok(Some(TreeSnapshot { commit, head }))
}

/// Make the working tree match `commit`'s snapshot: overwrite changed
/// files, recreate deleted ones, and delete files the snapshot did not
/// have — while never touching ignored files, the user's index, HEAD,
/// or the branch. Directories emptied by the deletions are removed.
pub async fn restore(cwd: &Path, commit: &str) -> anyhow::Result<()> {
    let root = PathBuf::from(git(cwd, NO_ENV, &["rev-parse", "--show-toplevel"]).await?);

    // Both file lists come first: the current set must describe the
    // pre-restore tree, and a bad commit id must fail before anything
    // is written.
    let current = git(
        &root,
        NO_ENV,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )
    .await?;
    let index = TempIndex::new(commit)?;
    git(&root, index.env(), &["read-tree", commit]).await?;
    let snapshot: std::collections::HashSet<String> =
        split_z(&git(&root, index.env(), &["ls-files", "-z"]).await?)
            .map(str::to_string)
            .collect();

    git(&root, index.env(), &["checkout-index", "-a", "-f"]).await?;

    for path in split_z(&current).filter(|path| !snapshot.contains(*path)) {
        let absolute = root.join(path);
        // A file↔directory type conflict means `checkout-index -f`
        // already replaced this path with snapshot content: a directory
        // here is snapshot-owned, and a missing path (or one whose
        // parent became a file) needs no deletion.
        match std::fs::symlink_metadata(&absolute) {
            Err(_) => continue,
            Ok(metadata) if metadata.is_dir() => continue,
            Ok(_) => {}
        }
        match std::fs::remove_file(&absolute) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).context(format!("failed to remove {}", absolute.display()));
            }
        }
        // Prune directories the deletion emptied; `remove_dir` refuses
        // non-empty ones, which is exactly the stop condition.
        let mut parent = absolute.parent();
        while let Some(directory) = parent {
            if directory == root || std::fs::remove_dir(directory).is_err() {
                break;
            }
            parent = directory.parent();
        }
    }
    Ok(())
}

fn split_z(list: &str) -> impl Iterator<Item = &str> {
    list.split('\0').filter(|path| !path.is_empty())
}

fn checkpoint_ref(session_id: &str) -> String {
    // Session ids are canonical UUIDs, but this function cannot assume
    // its caller checked; a ref-unsafe id must not turn into a silent
    // permanent `update-ref` failure.
    format!("refs/ilar/checkpoints/{}", sanitize(session_id))
}

const NO_ENV: &[(&str, &str)] = &[];

/// A fixed identity for shadow commits: `commit-tree` must not depend
/// on (or leak) the user's configured identity, and must work when
/// none is configured.
const IDENTITY_ENV: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "ilar"),
    ("GIT_AUTHOR_EMAIL", "checkpoint@ilar.invalid"),
    ("GIT_COMMITTER_NAME", "ilar"),
    ("GIT_COMMITTER_EMAIL", "checkpoint@ilar.invalid"),
];

/// A throwaway index file, removed on drop.
struct TempIndex {
    path: PathBuf,
    env: [(&'static str, String); 1],
}

impl TempIndex {
    fn new(label: &str) -> anyhow::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "ilar-index-{}-{}",
            sanitize(label),
            uuid::Uuid::new_v4()
        ));
        let env = [("GIT_INDEX_FILE", path.display().to_string())];
        Ok(Self { path, env })
    }

    fn env(&self) -> &[(&'static str, String)] {
        &self.env
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

async fn git_optional(cwd: &Path, args: &[&str]) -> Option<String> {
    git(cwd, NO_ENV, args).await.ok()
}

/// Run git with a scrubbed `GIT_*` environment plus `extra_env`,
/// returning trimmed stdout. Mirrors `tools::git_output`, with the
/// environment hook snapshots need.
async fn git<K, V>(cwd: &Path, extra_env: &[(K, V)], args: &[&str]) -> anyhow::Result<String>
where
    K: AsRef<std::ffi::OsStr>,
    V: AsRef<std::ffi::OsStr>,
{
    let mut command = tokio::process::Command::new("git");
    command.arg("-C").arg(cwd).args(args).kill_on_drop(true);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    // Pinned like `tools::git_command`: a shadow-ref failure is read by
    // whoever debugs it, and it should read the same on every machine.
    command.env("LC_ALL", "C").env("LANG", "C");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .map_err(|_| anyhow::anyhow!("git {} timed out", args.first().unwrap_or(&"")))?
        .context("failed to run git")?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
