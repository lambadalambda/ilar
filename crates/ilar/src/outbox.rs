//! Durable outbox for subagent notifications — the core half of
//! meta/issues/completions-survive-the-process.md.
//!
//! A background child's completion exists in exactly one place while
//! undelivered: an in-memory channel. Quit, a session switch, or a crash
//! drops it without a word, and the child's finished work is stranded in
//! a session log its parent never hears about. The outbox is the disk
//! copy of that channel: every publish appends one JSONL line under the
//! parent's session id *before* the channel send, and delivery is
//! derived from the parent's own log — a notification whose text made it
//! into a `UserMessage` was appended as a prompt and needs no requeue.
//! At session open a surface calls [`pending`] to adopt everything its
//! tree published but never delivered.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::session::{SessionEvent, SessionStore};
use crate::subagent::Notification;

/// Cap on the parent-chain walk in the ancestry filter. Session metas
/// come from disk, and a corrupted or maliciously edited pair of logs
/// could name each other as parents; the cap turns that cycle into "not
/// my tree" instead of a hang. Deeper-than-cap nesting is impossible —
/// spawners stop at a single-digit depth limit.
const ANCESTRY_CAP: usize = 64;

/// Append a published notification to the outbox: one JSONL line in
/// `<dir>/<parent_session_id>.jsonl`. Best-effort: an IO failure must
/// not take the publish down — the in-memory channel still delivers in
/// this process's lifetime, and a disk that cannot hold the safety copy
/// has no better place for an error report either.
pub fn record(dir: &Path, notification: &Notification) {
    let _ = try_record(dir, notification);
}

fn try_record(dir: &Path, notification: &Notification) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let line = serde_json::to_string(notification).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(entry_path(dir, &notification.parent_session_id))?;
    writeln!(file, "{line}")
}

fn entry_path(dir: &Path, parent_session_id: &str) -> PathBuf {
    dir.join(format!("{parent_session_id}.jsonl"))
}

/// Everything published but never delivered, for the session tree rooted
/// at `root_session_id`: entries whose parent session still exists,
/// whose ancestry (via `meta.parent_id`) reaches that root, and whose
/// text does not yet appear in any `UserMessage` of the parent session's
/// log (delivery == the text was appended as a prompt). Compacts as it
/// goes: rewrites each touched file dropping delivered entries and
/// entries for dead sessions; removes empty files.
///
/// The ancestry filter exists because several ilar processes can run
/// different root sessions against the same store: a process must only
/// adopt outbox entries belonging to its own tree, and leave the other
/// trees' files exactly as they are.
///
/// Adoption is not exclusive: two processes opened on the *same* root
/// both adopt the same entries. The delivery paths re-check the target
/// log before appending (`route_notification`), which shrinks the
/// double-delivery window from the whole session to the gap between
/// that check and the append; one process per tree remains the
/// supported arrangement, and the OS session writer lock is the
/// backstop underneath it.
pub fn pending(store: &SessionStore, dir: &Path, root_session_id: &str) -> Vec<Notification> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut undelivered = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(parent_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let parent_id = parent_id.to_string();
        let parent = match store.load(&parent_id) {
            Ok(parent) => parent,
            Err(error)
                if matches!(
                    error.kind(),
                    // NotFound: the session is gone. InvalidInput: the
                    // stem could never name one. Either way the file is
                    // noise for every process, whichever tree it was.
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
                ) =>
            {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            // Any other failure is a bad moment, not a dead session:
            // deleting here would turn a transient IO error into
            // permanent loss. Leave the file for the next open.
            Err(_) => continue,
        };
        if !reaches_root(store, &parent_id, root_session_id) {
            // Another process's tree: not ours to adopt or to compact.
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let recorded: Vec<Notification> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let recorded_len = recorded.len();
        // Delivery check: the routing paths may prepend queued steer
        // messages to the prompt they append, so the notification text
        // is a substring of the delivering `UserMessage`, not equal to
        // it. Session logs are append-only (compaction appends, never
        // rewrites), so scanning the full event list is sound. Accepted
        // limitation: two byte-identical notification texts for the same
        // parent dedupe as one — the second reads as delivered by the
        // first's prompt.
        let kept: Vec<Notification> = recorded
            .into_iter()
            .filter(|notification| {
                !parent.events().iter().any(|event| match event {
                    SessionEvent::UserMessage { text, .. } => text.contains(&notification.text),
                    _ => false,
                })
            })
            .collect();
        if kept.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            if kept.len() != recorded_len {
                let _ = rewrite(&path, &kept);
            }
            undelivered.extend(kept);
        }
    }
    undelivered
}

/// Whether `session_id`'s parent chain reaches `root_session_id` —
/// including the trivial chain, a notification published for the root
/// itself.
fn reaches_root(store: &SessionStore, session_id: &str, root_session_id: &str) -> bool {
    let mut current = session_id.to_string();
    for _ in 0..ANCESTRY_CAP {
        if current == root_session_id {
            return true;
        }
        let Ok(session) = store.load(&current) else {
            return false;
        };
        let Some(parent_id) = session.meta().and_then(|meta| meta.parent_id.clone()) else {
            return false;
        };
        current = parent_id;
    }
    false
}

/// Rewrite one outbox file to exactly these entries, atomically enough
/// for a crash mid-compaction to cost a re-delivery check, never an
/// entry: the temp file is complete before it replaces the original.
fn rewrite(path: &Path, kept: &[Notification]) -> std::io::Result<()> {
    let mut lines = String::new();
    for notification in kept {
        lines.push_str(&serde_json::to_string(notification).map_err(std::io::Error::other)?);
        lines.push('\n');
    }
    let temp = path.with_extension("jsonl.tmp");
    std::fs::write(&temp, lines)?;
    std::fs::rename(&temp, path)
}
