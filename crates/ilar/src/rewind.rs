//! Rewind a session and its working tree together.

use std::path::Path;

use crate::checkpoint;
use crate::session::{SessionEvent, SessionStore};

/// What a completed rewind did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindReport {
    /// Text of the user message the rewind unsent.
    pub unsent: String,
    /// Whether the working tree was restored from a checkpoint.
    pub tree_restored: bool,
    /// The repository HEAD moved between the checkpoint and the rewind;
    /// files were still restored, but commits made since remain.
    pub head_moved: bool,
}

/// Rewind `session_id` back to the user turn at local event index `cut`
/// — pointing at the `UserMessage` to unsend, whose event id must be
/// `target_user_id` (the caller chose it from an earlier load; the id
/// check catches the session having changed since). When the turn has a
/// tree checkpoint, the working tree is restored to it, after a safety
/// snapshot that keeps the pre-rewind tree reachable from
/// `refs/ilar/checkpoints/<id>`.
///
/// The writer lease is held from the first validation to the final
/// append, so an active turn (here or in another process) rejects the
/// rewind before anything touches the tree. Within the lease the order
/// is safety snapshot, tree restore, session marker: a failure between
/// the last two leaves the tree restored with the log unchanged, which
/// the safety snapshot at the ref tip keeps recoverable by hand — an
/// unrecorded conversation cut would not be.
pub async fn rewind_session(
    store: &SessionStore,
    session_id: &str,
    cut: usize,
    target_user_id: &str,
    cwd: &Path,
) -> anyhow::Result<RewindReport> {
    let session = store.acquire_writer(session_id)?.load()?;
    session.rewind_target(cut)?;
    let events = session.events();
    if !matches!(
        events.get(cut),
        Some(SessionEvent::UserMessage { id, .. }) if id == target_user_id
    ) {
        anyhow::bail!("the session changed since the rewind target was chosen; pick again");
    }
    // The turn's tree state is the checkpoint taken right before its
    // user message; turns predating checkpointing rewind conversation
    // only.
    let target = match cut.checked_sub(1).and_then(|index| events.get(index)) {
        Some(SessionEvent::Checkpoint { commit, head, .. }) => Some((commit.clone(), head.clone())),
        _ => None,
    };

    let mut head_moved = false;
    let mut tree_restored = None;
    let mut tree_saved = None;
    if let Some((commit, recorded_head)) = target {
        let saved = checkpoint::snapshot(cwd, session_id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the turn has a tree checkpoint but {} is no longer a git repository",
                    cwd.display()
                )
            })?;
        head_moved = saved.head != recorded_head;
        checkpoint::restore(cwd, &commit).await?;
        tree_saved = Some(saved.commit);
        tree_restored = Some(commit);
    }

    let outcome = session.rewind_to(cut, tree_restored.clone(), tree_saved)?;
    Ok(RewindReport {
        unsent: outcome.unsent,
        tree_restored: tree_restored.is_some(),
        head_moved,
    })
}
