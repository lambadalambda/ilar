//! Append-only JSONL session store.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use super::event::{SessionEvent, SessionMeta, new_id, unknown_event_type};
use super::last_by_dir;
use super::model::{ChatMessage, ContentBlock, Role};
use super::replay_index::{
    FileStamp, REPLAY_INDEX_VERSION, ReplayCheckpoint, ReplayIdIndex, checkpoint_checksum,
    committed_line_count, file_stamp, id_record, id_records, invalid_data, read_all_id_records,
    replay_ids_path, write_checkpoint, write_id_records,
};
use super::summary_cache::{CachedKind, Scanned, ScannedFile};
use crate::question::{QUESTION_TOOL_NAME, QuestionRequest, validate_request};
use crate::text::truncate_chars_ellipsis;

/// Owns the session directory; creates/loads sessions.
#[derive(Clone)]
pub struct SessionStore {
    root: PathBuf,
}

/// How many times a writer will re-open a lock whose file was replaced
/// under it. Two deletions racing one acquisition is already a stretch;
/// four is a bound, not a wait.
const LOCK_IDENTITY_ATTEMPTS: usize = 4;

/// Does this handle still hold the file the path names? The check the
/// flock cannot make for us: an unlinked inode locks just as happily as
/// a live one.
#[cfg(unix)]
fn locked_the_named_file(file: &File, path: &std::path::Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = file.metadata()?;
    match std::fs::metadata(path) {
        Ok(named) => Ok(held.dev() == named.dev() && held.ino() == named.ino()),
        // The path is gone: whatever we hold, it is not the session's
        // lock any more.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn locked_the_named_file(_file: &File, _path: &std::path::Path) -> std::io::Result<bool> {
    // Windows keeps an open file's name from being reused, so the
    // question does not arise.
    Ok(true)
}

/// Canonical UUID used for all session and lock path derivation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(id: &str) -> std::io::Result<Self> {
        let parsed = uuid::Uuid::parse_str(id).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid session id: {id:?}"),
            )
        })?;
        if parsed.hyphenated().to_string() != id {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("session id is not a canonical UUID: {id:?}"),
            ));
        }
        Ok(Self(id.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// In-memory replay of one session's event log, plus the append handle.
pub struct Session {
    events: Vec<SessionEvent>,
    file: File,
    _writer: SessionWriter,
    event_base: usize,
    canonical_event_count: usize,
    /// Committed lines in the log file. Unlike `canonical_event_count`
    /// this counts what a rewind abandoned, so tail-parse diagnostics
    /// can name a line the reader will actually find.
    physical_line_count: usize,
    effective_model: String,
    effective_variant: Option<String>,
    todo_list: Option<crate::todo::TodoList>,
    topic: Option<String>,
    checkpoint: Option<ReplayCheckpoint>,
    checkpoint_tail_start: usize,
    observed_stamp: FileStamp,
}

/// Exclusive OS-backed writer ownership for one session. The lock
/// itself is released by the OS on drop or crash; the file that carried
/// it goes with the lease, so the sessions directory does not collect
/// one `.lock` per session ever opened (2,414 of them, measured — see
/// meta/issues/sessions-list-fast-and-true.md).
pub struct SessionWriter {
    _file: File,
    id: SessionId,
    session_path: PathBuf,
    replay_index_path: PathBuf,
    lock_path: PathBuf,
}

struct ReplayData {
    events: Vec<SessionEvent>,
    unanswered_calls: Vec<String>,
    event_base: usize,
    canonical_event_count: usize,
    physical_line_count: usize,
    effective_model: String,
    effective_variant: Option<String>,
    todo_list: Option<crate::todo::TodoList>,
    topic: Option<String>,
    checkpoint: Option<ReplayCheckpoint>,
    checkpoint_tail_start: usize,
    observed_stamp: FileStamp,
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        // Unlinked *before* the release, the order `delete` documents:
        // a waiter that wins the lock on this now-nameless inode
        // re-stats the path, finds it gone and starts over, while
        // unlinking after the release could strand a holder that had
        // already passed that check.
        //
        // And only while the path still names the inode we hold:
        // `delete` unlinks the lock itself while holding it, and a new
        // writer may already own a fresh file at the same path by now.
        // Removing *that* would leave it holding a nameless inode while
        // a third writer locks a new file at the path — two owners of
        // one session, the very thing the identity check exists to
        // prevent. Nothing can replace the file under this check
        // either: replacing it means unlinking it first, which means
        // holding the lock this handle is holding.
        if locked_the_named_file(&self._file, &self.lock_path).unwrap_or(false) {
            let _ = std::fs::remove_file(&self.lock_path);
        }
        let _ = FileExt::unlock(&self._file);
    }
}

/// A validated question tool call awaiting an interactive answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingQuestion {
    pub tool_call_id: String,
    pub request: QuestionRequest,
}

/// What a rewind cut away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindOutcome {
    /// Text of the user message the cut unsent.
    pub unsent: String,
}

/// One entry in the session listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    /// First user message, whitespace-collapsed and bounded; `None` for
    /// sessions without one yet.
    pub title: Option<String>,
    pub modified: std::time::SystemTime,
    /// The directory the session was launched from, when it recorded
    /// one. Resume surfaces lead with the directory they are running
    /// in; sessions from before this was written down have `None` and
    /// group with the rest.
    pub cwd: Option<std::path::PathBuf>,
}

/// A session file's head: enough to summarize it without reading the
/// whole log. The listing is this read applied to every file in the
/// root; [`SessionStore::head`] is the same read for one id.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionHead {
    pub id: String,
    pub meta: SessionMeta,
    pub title: Option<String>,
    pub modified: std::time::SystemTime,
}

/// One subagent task belonging to a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSummary {
    pub id: String,
    /// The agent it runs as, from its persisted metadata.
    pub agent: String,
    pub model: String,
    /// Its opening prompt, whitespace-collapsed and bounded.
    pub title: Option<String>,
    pub modified: std::time::SystemTime,
}

const SUMMARY_SCAN_BYTES: u64 = 256 * 1024;
const SUMMARY_SCAN_EVENTS: usize = 40;
const SUMMARY_TITLE_CHARS: usize = 80;

fn summary_title(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars_ellipsis(&collapsed, SUMMARY_TITLE_CHARS)
}

/// Bytes of a log read to decide whether anyone ever typed into it. A
/// session with no user message is a `Meta` line and perhaps a
/// `ModelChange`; anything larger has a turn in it, and proving that by
/// reading further is the cost this check exists to avoid.
const EMPTY_LOG_BYTES: u64 = 64 * 1024;

/// Whether the log holds a user message — the question "is there
/// anything in this session?" reduces to.
///
/// Every uncertainty answers yes: a file that cannot be read, a line
/// that will not parse, a log too big to check. Keeping a session
/// nobody wanted costs a row; discarding one somebody did costs their
/// work.
fn log_has_user_message(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return true;
    };
    if metadata.len() > EMPTY_LOG_BYTES {
        return true;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return true;
    };
    for line in String::from_utf8_lossy(&bytes).lines() {
        match serde_json::from_str::<SessionEvent>(line) {
            Ok(SessionEvent::UserMessage { .. }) => return true,
            Ok(_) => {}
            // A line nobody can read is not proof that nothing was said.
            Err(_) if !line.trim().is_empty() => return true,
            Err(_) => {}
        }
    }
    false
}

/// Remove the lock files nobody holds, the way the live-turn scratches
/// are swept: every `<id>.lock` in the sessions directory is offered a
/// non-blocking flock, and winning it is the proof that the process it
/// belonged to is gone. A lock somebody does hold refuses the flock and
/// is left exactly alone.
///
/// Writers unlink their own lock on release, so this is only for the
/// ones a crash — or a version that never removed them — left behind.
pub fn sweep_stale_locks(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".lock").map(str::to_string))
        else {
            continue;
        };
        if SessionId::parse(&id).is_err() {
            continue;
        }
        let path = entry.path();
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        if FileExt::try_lock_exclusive(&file).is_err() {
            // Held: a turn is running in this or another process.
            continue;
        }
        // Unlinked while held, then released — the same order
        // `SessionWriter`'s drop uses, and with the same identity
        // check: the lock we just won may be an orphan whose path a
        // live writer has already taken over.
        if locked_the_named_file(&file, &path).unwrap_or(false) {
            let _ = std::fs::remove_file(&path);
        }
        let _ = FileExt::unlock(&file);
    }
}

/// A directory entry as a session file, or `None` for anything that is
/// not named like one — the scan skips those without opening them.
fn scan_file(entry: &std::fs::DirEntry) -> Option<ScannedFile> {
    let name = entry.file_name();
    let id = name.to_str()?.strip_suffix(".jsonl")?.to_string();
    SessionId::parse(&id).ok()?;
    let metadata = entry.metadata().ok()?;
    Some(ScannedFile {
        id,
        path: entry.path(),
        modified: metadata.modified().ok()?,
        len: metadata.len(),
    })
}

/// Parse a session file's head: its metadata and opening prompt,
/// without loading the whole log.
fn read_head(
    path: &std::path::Path,
    id: String,
    modified: std::time::SystemTime,
) -> Option<SessionHead> {
    let file = File::open(path).ok()?;
    let head = std::io::BufReader::new(file.take(SUMMARY_SCAN_BYTES));
    let mut lines = std::io::BufRead::lines(head);
    let meta_line = lines.next()?.ok()?;
    let SessionEvent::Meta { meta, .. } = serde_json::from_str(&meta_line).ok()? else {
        return None;
    };
    // A generated topic beats the opening message, which is often a
    // stack trace or "hey can you look at something".
    let mut opening = None;
    let mut topic = None;
    for event in lines
        .take(SUMMARY_SCAN_EVENTS)
        .map_while(|line| line.ok())
        .filter_map(|line| serde_json::from_str::<SessionEvent>(&line).ok())
    {
        match event {
            SessionEvent::Topic { text, .. } => topic = Some(summary_title(&text)),
            SessionEvent::UserMessage { text, .. } if opening.is_none() => {
                opening = Some(summary_title(&text));
            }
            _ => {}
        }
    }
    // The topic is written after the first turn, so a tool-heavy
    // opening pushes it past the head scan — and it may be rewritten
    // later still. The tail is where the newest one is.
    let title = last_topic_in_tail(path).or(topic).or(opening);
    Some(SessionHead {
        id,
        meta,
        title,
        modified,
    })
}

/// Bytes of the file's tail searched for the newest `Topic` event.
const TOPIC_TAIL_BYTES: u64 = 256 * 1024;

/// The last topic written in the file's tail, without reading the
/// whole log: one seek and one bounded read per session.
fn last_topic_in_tail(path: &std::path::Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TOPIC_TAIL_BYTES);
    file.seek(std::io::SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // A cut mid-line at the front is not a whole event; every later
    // line is.
    let lines: Vec<&str> = text.lines().skip(usize::from(start > 0)).collect();
    lines
        .iter()
        .rev()
        .filter(|line| line.contains("\"topic\""))
        .find_map(
            |line| match serde_json::from_str::<SessionEvent>(line).ok()? {
                SessionEvent::Topic { text, .. } => Some(summary_title(&text)),
                _ => None,
            },
        )
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The directory the store lives in, for an error that has to name
    /// the place it could not write to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn session_path(&self, id: &str) -> std::io::Result<PathBuf> {
        let id = SessionId::parse(id)?;
        Ok(self.session_path_for(&id))
    }

    pub fn replay_index_path(&self, id: &str) -> std::io::Result<PathBuf> {
        let id = SessionId::parse(id)?;
        Ok(self.replay_index_path_for(&id))
    }

    /// Fully replay the canonical JSONL audit log, bypassing disposable
    /// indexes. Unlike `load`, rewind markers are not folded out: the
    /// audit view keeps every line, including abandoned tails.
    pub fn audit_events(&self, id: &str) -> std::io::Result<Vec<SessionEvent>> {
        self.audit_events_until(id, &NEVER)
    }

    /// [`audit_events`](Self::audit_events) that gives up when
    /// `cancelled` is raised. A long archive is a long parse, and a
    /// caller who has gone away — a tool call abandoned, a preview
    /// scrolled past — should not still be paying for it. The error is
    /// [`std::io::ErrorKind::Interrupted`], which is a stop and not a
    /// failure of the log.
    pub fn audit_events_until(
        &self,
        id: &str,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<Vec<SessionEvent>> {
        let id = SessionId::parse(id)?;
        let path = self.session_path_for(&id);
        let bytes = std::fs::read(&path)?;
        Ok(
            parse_event_lines_until(&bytes[..committed_len(&bytes)], id.as_str(), 0, cancelled)?
                .into_iter()
                .map(|(_, event)| event)
                .collect(),
        )
    }

    pub fn acquire_writer(&self, id: &str) -> std::io::Result<SessionWriter> {
        self.acquire_writer_id(SessionId::parse(id)?)
    }

    fn session_path_for(&self, id: &SessionId) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    fn replay_index_path_for(&self, id: &SessionId) -> PathBuf {
        self.root.join(format!("{id}.replay.json"))
    }

    fn lock_path_for(&self, id: &SessionId) -> PathBuf {
        self.root.join(format!("{id}.lock"))
    }

    fn acquire_writer_id(&self, id: SessionId) -> std::io::Result<SessionWriter> {
        std::fs::create_dir_all(&self.root)?;
        let lock_path = self.lock_path_for(&id);
        // A lock is an inode, not a path. `delete()` unlinks the lock
        // while holding it, so a waiter can win the lock on an inode
        // that no longer has a name while a third process locks a fresh
        // file at the same path — and both believe they own the
        // session. Re-stat after locking, and start over if the fd is
        // not what the path names now. The loop is bounded because
        // repeated deletion of the same session is a pathology, not a
        // state to wait out.
        for _ in 0..LOCK_IDENTITY_ATTEMPTS {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)?;
            if let Err(error) = FileExt::try_lock_exclusive(&file) {
                let contended = fs2::lock_contended_error();
                return Err(
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || contended.raw_os_error().is_some()
                            && error.raw_os_error() == contended.raw_os_error()
                    {
                        std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            format!("session {id} is held by {}", holder(&mut file)),
                        )
                    } else {
                        error
                    },
                );
            }
            if !locked_the_named_file(&file, &lock_path)? {
                // Dropping the handle releases the lock on the orphan.
                drop(file);
                continue;
            }
            // Say who holds it, so the refusal above can name a
            // process rather than a possibility. Best effort: a lock
            // that cannot be written is still a lock.
            let _ = write_holder(&file);
            return Ok(SessionWriter {
                _file: file,
                session_path: self.session_path_for(&id),
                replay_index_path: self.replay_index_path_for(&id),
                lock_path,
                id,
            });
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!("session {id} is being deleted out from under its lock"),
        ))
    }
}

/// Who holds a session's lock, as the lock file says. The line is
/// written by the winner right after it takes the lock; a lock from an
/// older ilar, or one whose holder died between locking and writing,
/// says nothing and gets the old wording — which is still true, just
/// less useful.
fn holder(file: &mut File) -> String {
    let mut text = String::new();
    let read = file
        .seek(std::io::SeekFrom::Start(0))
        .and_then(|_| file.read_to_string(&mut text));
    if read.is_err() {
        return "another turn (its driver may be another ilar process)".into();
    }
    let mut lines = text.lines();
    match (lines.next(), lines.next()) {
        // A pid that is not a number is a torn or foreign line, not a
        // process to go looking for.
        (Some(pid), Some(since)) if pid.parse::<u32>().is_ok() => {
            format!("process {pid}, which took it at {since}")
        }
        _ => "another turn (its driver may be another ilar process)".into(),
    }
}

/// Stamp this process onto the lock it just won.
fn write_holder(file: &File) -> std::io::Result<()> {
    let line = format!(
        "{}\n{}\n",
        std::process::id(),
        chrono::Utc::now().to_rfc3339()
    );
    file.set_len(0)?;
    (&*file).seek(std::io::SeekFrom::Start(0))?;
    (&*file).write_all(line.as_bytes())?;
    (&*file).flush()
}

impl SessionStore {
    /// Create a new session; writes the Meta event as the first line.
    /// A root session with a launch directory also becomes that
    /// directory's answer to "what was I last doing here?".
    pub fn create(&self, meta: SessionMeta) -> std::io::Result<Session> {
        let id = SessionId::parse(&meta.session_id)?;
        let path = self.session_path_for(&id);
        let writer = self.acquire_writer_id(id)?;
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)?;
        let observed_stamp = file_stamp(&file.metadata()?)?;
        let mut session = Session {
            events: Vec::new(),
            file,
            _writer: writer,
            event_base: 0,
            canonical_event_count: 0,
            physical_line_count: 0,
            effective_model: meta.model.clone(),
            effective_variant: None,
            todo_list: None,
            topic: None,
            checkpoint: None,
            checkpoint_tail_start: 0,
            observed_stamp,
        };
        let launched_in = meta.cwd.clone().filter(|_| meta.parent_id.is_none());
        session.append(SessionEvent::Meta {
            meta,
            ts: chrono::Utc::now(),
        })?;
        if let Some(cwd) = launched_in {
            self.point_directory_at(&cwd, session.session_id());
        }
        Ok(session)
    }

    /// Every session file in the root, resolved through the summary
    /// cache: one cache read, a head read for the files whose stamp
    /// moved, and one cache write when anything did. Children and
    /// unreadable files are not opened once they are known — see
    /// meta/issues/sessions-list-fast-and-true.md.
    fn scan(&self) -> Vec<Scanned> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let files: Vec<ScannedFile> = entries
            .flatten()
            .filter_map(|entry| scan_file(&entry))
            .collect();
        let cache = super::summary_cache::load(&self.root);
        let (scanned, fresh, changed) = super::summary_cache::resolve(files, &cache, |file| {
            read_head(&file.path, file.id.clone(), file.modified)
        });
        if changed {
            super::summary_cache::save(&self.root, &fresh);
        }
        scanned
    }

    /// List root (non-subagent) sessions, most recently modified first.
    /// Reads only each file's head, and only when the cache cannot
    /// answer; unreadable, foreign, headless, or child-session files are
    /// skipped — see meta/issues/session-list-and-resume-last.md.
    pub fn list(&self) -> Vec<SessionSummary> {
        let mut sessions: Vec<SessionSummary> = self
            .scan()
            .into_iter()
            .filter_map(|entry| match entry.kind {
                CachedKind::Root { title, cwd } => Some(SessionSummary {
                    id: entry.id,
                    title,
                    modified: entry.modified,
                    cwd,
                }),
                CachedKind::Child { .. } | CachedKind::Unreadable => None,
            })
            .collect();
        sessions.sort_by(|left, right| {
            right
                .modified
                .cmp(&left.modified)
                .then_with(|| left.id.cmp(&right.id))
        });
        sessions
    }

    /// The most recently modified root session, if any.
    pub fn latest(&self) -> Option<SessionSummary> {
        self.list().into_iter().next()
    }

    /// Point a directory at a session: the answer `--continue` and the
    /// picker read before they list anything. `cwd` must be the
    /// directory the session itself recorded — nothing else is
    /// comparable with what a reader will canonicalize.
    fn point_directory_at(&self, cwd: &Path, id: &str) {
        let Ok(parsed) = SessionId::parse(id) else {
            return;
        };
        let Ok(modified) = std::fs::symlink_metadata(self.session_path_for(&parsed))
            .and_then(|metadata| metadata.modified())
        else {
            return;
        };
        last_by_dir::update(&self.root, |pointers| {
            pointers.set(
                cwd,
                last_by_dir::Pointer {
                    session_id: id.to_string(),
                    modified_nanos: super::summary_cache::modified_nanos(modified),
                },
            )
        });
    }

    /// Remember a session as the last one used in the directory it was
    /// launched from. The directory comes from the session's own head,
    /// so no caller can point a directory at another one's work; a
    /// subagent's session and a session that recorded no directory have
    /// nowhere to be remembered and are skipped.
    pub fn remember_last(&self, id: &str) {
        let Ok(head) = self.head(id) else {
            return;
        };
        if head.meta.parent_id.is_some() {
            return;
        }
        if let Some(cwd) = head.meta.cwd.as_deref() {
            self.point_directory_at(cwd, id);
        }
    }

    /// Forget every directory pointing at one of these sessions — a
    /// session that was deleted must not stay pointed at. One write for
    /// the lot, so a sweep of a hundred does not rewrite the file a
    /// hundred times.
    fn forget(&self, ids: &[&str]) {
        if ids.is_empty() {
            return;
        }
        last_by_dir::update(&self.root, |pointers| {
            // Every id, not the first that matched: `any` would stop
            // early and leave the rest pointed at.
            let mut changed = false;
            for id in ids {
                changed |= pointers.forget(id);
            }
            changed
        });
    }

    /// This directory's last session as the pointer file names it,
    /// without listing the directory: one JSON read and one head read.
    ///
    /// `None` when there is no pointer or it cannot be believed — the
    /// session is gone, is somebody's subagent, was launched somewhere
    /// else, or its file has been replaced by an older one. Callers
    /// fall back to [`Self::latest_in`] and repair the pointer with
    /// [`Self::remember_last`].
    pub fn last_in(&self, cwd: &std::path::Path) -> Option<SessionSummary> {
        let cwd = std::fs::canonicalize(cwd).ok()?;
        let pointer = last_by_dir::load(&self.root).get(&cwd).cloned()?;
        let head = self.head(&pointer.session_id).ok()?;
        if head.meta.parent_id.is_some() || head.meta.cwd.as_deref() != Some(cwd.as_path()) {
            return None;
        }
        // The file the pointer was written for only ever grows. One
        // that has gone backwards is a different file at the same path.
        if super::summary_cache::modified_nanos(head.modified) < pointer.modified_nanos {
            return None;
        }
        Some(SessionSummary {
            id: head.id,
            title: head.title,
            modified: head.modified,
            cwd: head.meta.cwd,
        })
    }

    /// The most recently modified root session launched from `cwd`, if
    /// any. "Continue where I left off" means this directory's work:
    /// the newest session overall may belong to another checkout
    /// entirely, and resuming it here would run its conversation
    /// against these files. Compared canonically, the way a session
    /// records its own launch directory; a session from before that
    /// was written down has no directory and is never "here".
    pub fn latest_in(&self, cwd: &std::path::Path) -> Option<SessionSummary> {
        let cwd = std::fs::canonicalize(cwd).ok()?;
        self.list()
            .into_iter()
            .find(|session| session.cwd.as_deref() == Some(cwd.as_path()))
    }

    /// Every session named as somebody's parent by a file in the root,
    /// from the scan's cached verdicts. One question asked of the whole
    /// directory, because both callers need it for more than one id.
    fn parents_in(scanned: &[Scanned]) -> HashSet<&str> {
        scanned
            .iter()
            .filter_map(|entry| match &entry.kind {
                CachedKind::Child { parent } => Some(parent.as_str()),
                CachedKind::Root { .. } | CachedKind::Unreadable => None,
            })
            .collect()
    }

    /// Remove a root session nobody ever said anything in: the log is
    /// written when `ilar` launches, before a prompt exists, so every
    /// open-and-quit used to leave a "(no messages yet)" row for ever
    /// (104 of 273, measured). Returns whether the session went.
    ///
    /// Refuses anything not plainly disposable: a session with a user
    /// message, a subagent's session, one that spawned children, one
    /// with a completion still in the outbox, one whose head cannot be
    /// read, and one whose writer lease somebody holds — `delete`
    /// declines that last case on its own.
    pub fn remove_if_empty(&self, id: &str, outbox_dir: &Path) -> bool {
        // The file's own answers first, so a session with a user
        // message costs one head read and never walks the directory.
        if !self.is_unspoken_root(id, outbox_dir) || Self::parents_in(&self.scan()).contains(id) {
            return false;
        }
        if self.delete(id).is_err() {
            return false;
        }
        // A directory left naming a session that is gone costs the next
        // `--continue` the listing the pointer exists to skip.
        self.forget(&[id]);
        true
    }

    /// Everything about disposability one file can answer: a root
    /// session, no user message, no mail waiting for it. Whether it has
    /// children takes a directory scan, so the callers add that — once
    /// per id here, once for the whole sweep there.
    ///
    /// Public because whether a session is the disposable kind is a
    /// premise other code rests on: `ilar exec` withholds a session's
    /// id until the turn starts precisely because an unstarted one is
    /// removed on the way out.
    pub fn is_unspoken_root(&self, id: &str, outbox_dir: &Path) -> bool {
        let Ok(parsed) = SessionId::parse(id) else {
            return false;
        };
        let Ok(head) = self.head(id) else {
            return false;
        };
        head.meta.parent_id.is_none()
            && !log_has_user_message(&self.session_path_for(&parsed))
            && !crate::outbox::has_entry(outbox_dir, id)
    }

    /// Remove the empty root sessions untouched for `older_than`: the
    /// ones a crash, a kill or a session switch left behind, since only
    /// a clean runtime end removes its own. Returns how many went.
    ///
    /// One scan for the whole sweep — the candidates and their
    /// parenthood come out of the same pass — and one pointer write at
    /// the end. A sweep that rescanned the directory per candidate
    /// would cost more on startup than the listing it is here to make
    /// cheap.
    ///
    /// `title.is_none()` is the cheap half of the test: a session with
    /// a user message is titled after it, so the precise check only
    /// runs on candidates.
    pub fn sweep_empty_sessions(
        &self,
        outbox_dir: &Path,
        older_than: std::time::Duration,
    ) -> usize {
        let Some(cutoff) = std::time::SystemTime::now().checked_sub(older_than) else {
            return 0;
        };
        let scanned = self.scan();
        let parents = Self::parents_in(&scanned);
        let candidates: Vec<&str> = scanned
            .iter()
            .filter(|entry| matches!(&entry.kind, CachedKind::Root { title: None, .. }))
            .filter(|entry| entry.modified < cutoff)
            .map(|entry| entry.id.as_str())
            .filter(|id| !parents.contains(id))
            .collect();
        let removed: Vec<&str> = candidates
            .into_iter()
            .filter(|id| self.is_unspoken_root(id, outbox_dir) && self.delete(id).is_ok())
            .collect();
        self.forget(&removed);
        removed.len()
    }

    /// The subagent tasks spawned by `parent_id`, newest first.
    /// [`Self::list`] hides children by construction — this is the other
    /// half, and it is scoped: a session sees its own tasks only.
    pub fn children_of(&self, parent_id: &str) -> Vec<ChildSummary> {
        let mut children: Vec<(std::time::SystemTime, ChildSummary)> = self
            .scan()
            .into_iter()
            // Whose child a file is comes out of the cache, so only this
            // session's own tasks are opened — not every log in the root.
            .filter(
                |entry| matches!(&entry.kind, CachedKind::Child { parent } if parent == parent_id),
            )
            .filter_map(|entry| read_head(&entry.path, entry.id, entry.modified))
            .map(|head| {
                (
                    head.modified,
                    ChildSummary {
                        id: head.id,
                        agent: head.meta.agent,
                        model: head.meta.model,
                        title: head.title,
                        modified: head.modified,
                    },
                )
            })
            .collect();
        children.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.id.cmp(&right.1.id))
        });
        children.into_iter().map(|(_, child)| child).collect()
    }

    /// One session's head by id: its metadata and title, without
    /// loading the log. Same read [`Self::list`] performs per file.
    pub fn head(&self, id: &str) -> std::io::Result<SessionHead> {
        let parsed = SessionId::parse(id)?;
        let path = self.session_path_for(&parsed);
        // lstat, matching what the listing reads off its directory
        // entries: the two must not disagree about a session's mtime.
        let modified = std::fs::symlink_metadata(&path)?.modified()?;
        read_head(&path, parsed.as_str().to_string(), modified).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session {parsed}: unreadable head"),
            )
        })
    }

    /// Delete a session's files. Refuses sessions whose writer lease is
    /// held (active in some turn) with `WouldBlock`.
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        let parsed = SessionId::parse(id)?;
        // Unlink everything while the lease is held: removing a file
        // after release would race a new holder of the same path. The
        // lock file is the writer's own to remove — its drop does it,
        // under the identity check that keeps it from taking a
        // successor's file by mistake.
        let _writer = self.acquire_writer_id(parsed.clone())?;
        let _ = std::fs::remove_file(self.replay_index_path_for(&parsed));
        for path in self.replay_ids_paths_for(&parsed) {
            let _ = std::fs::remove_file(path);
        }
        // The scratch too, or a crash-leftover outlives the session it
        // described — and a reader that watches scratches would show an
        // active turn for a session that is gone.
        let _ = std::fs::remove_file(super::live::live_path(&self.session_path_for(&parsed)));
        std::fs::remove_file(self.session_path_for(&parsed))?;
        Ok(())
    }

    /// Every id-index file the session owns. `publish_checkpoint` names
    /// them by generation and a crash before the superseded one is
    /// unlinked strands it, so scan the root instead of trusting the
    /// checkpoint to name the only live generation. Ids are UUIDs, so
    /// the full `{id}.replay.` prefix cannot reach another session's
    /// files. An unreadable root yields nothing — absence is fine here.
    fn replay_ids_paths_for(&self, id: &SessionId) -> Vec<PathBuf> {
        let prefix = format!("{id}.replay.");
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with(&prefix) && name.ends_with(".ids")
            })
            .map(|entry| entry.path())
            .collect()
    }

    /// Fork a session: copy its validated history under a fresh id (the
    /// Meta event is rewritten, the topic is dropped; everything else is
    /// verbatim). Returns the new session id.
    pub fn fork(&self, id: &str) -> std::io::Result<String> {
        let source = self.load(id)?;
        let cut = source.events().len();
        self.fork_events(id, source, cut)
    }

    /// Fork a session at a point: like `fork`, truncated to the active
    /// window's first `cut` events. `cut` must either equal the window
    /// length (a plain fork) or index a `UserMessage`, the same turn
    /// boundary a rewind accepts. Returns the new session id.
    pub fn fork_at(&self, id: &str, cut: usize) -> std::io::Result<String> {
        let source = self.load(id)?;
        if cut != source.events().len()
            && !matches!(
                source.events().get(cut),
                Some(SessionEvent::UserMessage { .. })
            )
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("fork cut {cut} is not a user message in session {id}"),
            ));
        }
        self.fork_events(id, source, cut)
    }

    fn fork_events(&self, id: &str, source: SessionReader, cut: usize) -> std::io::Result<String> {
        // Everything but the name: titling only runs on a session with
        // no topic yet, so a copied Topic left the fork and its source
        // wearing one name for ever. Without it the fork names itself
        // after its next completed turn.
        let mut events: Vec<SessionEvent> = source.events()[..cut]
            .iter()
            .filter(|event| !matches!(event, SessionEvent::Topic { .. }))
            .cloned()
            .collect();
        let new_id = crate::session::new_id();
        match events.first_mut() {
            Some(SessionEvent::Meta { meta, .. }) => {
                meta.session_id = new_id.clone();
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("session {id} does not start with a Meta event"),
                ));
            }
        }
        let parsed = SessionId::parse(&new_id)?;
        let mut output = String::new();
        for event in &events {
            output
                .push_str(&serde_json::to_string(event).map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                })?);
            output.push('\n');
        }
        std::fs::create_dir_all(&self.root)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(self.session_path_for(&parsed))?;
        std::io::Write::write_all(&mut file, output.as_bytes())?;
        file.sync_data()?;
        Ok(new_id)
    }

    /// Read a session snapshot. Only newline-committed records are parsed;
    /// committed corruption is rejected and an in-progress tail is ignored.
    pub fn load(&self, id: &str) -> std::io::Result<SessionReader> {
        self.load_until(id, &NEVER)
    }

    /// [`load`](Self::load) that gives up when `cancelled` is raised.
    /// Replaying a long transcript is the expensive half of seeding a
    /// view; a seed whose target moved on stops here rather than
    /// finishing for nobody.
    pub fn load_until(
        &self,
        id: &str,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<SessionReader> {
        let id = SessionId::parse(id)?;
        let path = self.session_path_for(&id);
        if !path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("session not found: {id}"),
            ));
        }
        let mut file = File::open(&path)?;
        let replay = read_replay(
            &mut file,
            &path,
            &self.replay_index_path_for(&id),
            id.as_str(),
            false,
            cancelled,
        )?;
        let pending_question = pending_question(&replay.events, &replay.unanswered_calls);
        Ok(SessionReader {
            events: replay.events,
            effective_model: replay.effective_model,
            effective_variant: replay.effective_variant,
            todo_list: replay.todo_list,
            topic: replay.topic,
            pending_question,
        })
    }
}

impl SessionWriter {
    pub fn load(self) -> std::io::Result<Session> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.session_path)?;
        let replay = read_replay(
            &mut file,
            &self.session_path,
            &self.replay_index_path,
            self.id.as_str(),
            true,
            &NEVER,
        )?;
        if file_stamp(&file.metadata()?)? != replay.observed_stamp
            || file_stamp(&std::fs::metadata(&self.session_path)?)? != replay.observed_stamp
        {
            return invalid_replay(self.id.as_str(), "session changed after writer replay");
        }
        let observed_stamp = replay.observed_stamp.clone();
        let pending_question = pending_question(&replay.events, &replay.unanswered_calls);
        let mut session = Session {
            events: replay.events,
            file,
            _writer: self,
            event_base: replay.event_base,
            canonical_event_count: replay.canonical_event_count,
            physical_line_count: replay.physical_line_count,
            effective_model: replay.effective_model,
            effective_variant: replay.effective_variant,
            todo_list: replay.todo_list,
            topic: replay.topic,
            checkpoint: replay.checkpoint,
            checkpoint_tail_start: replay.checkpoint_tail_start,
            observed_stamp,
        };
        if session.checkpoint.is_none() {
            let _ = session.rebuild_checkpoint();
        }
        if session.event_base == 0
            && let Some(checkpoint) = &session.checkpoint
        {
            session.events = checkpoint.events.clone();
            session.event_base = checkpoint.active_start;
            session.checkpoint_tail_start = session.events.len();
        }
        for tool_use_id in replay.unanswered_calls {
            if pending_question
                .as_ref()
                .is_some_and(|pending| pending.tool_call_id == tool_use_id)
            {
                continue;
            }
            session.append(SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id,
                content: "Tool call interrupted before completion.".into(),
                is_error: true,
                images: Vec::new(),
                child_session_id: None,
                state: None,
                ts: chrono::Utc::now(),
            })?;
        }
        Ok(session)
    }
}

fn read_replay(
    file: &mut File,
    path: &std::path::Path,
    replay_index_path: &std::path::Path,
    id: &str,
    repair_tail: bool,
    cancelled: &std::sync::atomic::AtomicBool,
) -> std::io::Result<ReplayData> {
    if let Ok(replay) = read_indexed_replay(file, path, replay_index_path, id) {
        return Ok(replay);
    }
    let canonical = read_events(file, path, id, repair_tail, cancelled)?;
    let canonical_event_count = canonical.events.len();
    let (effective_model, effective_variant, todo_list, topic) = replay_state(&canonical.events);
    let (events, event_base) = if repair_tail {
        (canonical.events, 0)
    } else {
        active_replay_window(&canonical.events)
    };
    Ok(ReplayData {
        canonical_event_count,
        physical_line_count: canonical.physical_line_count,
        events,
        unanswered_calls: canonical.unanswered_calls,
        event_base,
        effective_model,
        effective_variant,
        todo_list,
        topic,
        checkpoint: None,
        checkpoint_tail_start: 0,
        observed_stamp: canonical.observed_stamp,
    })
}

fn read_indexed_replay(
    file: &mut File,
    path: &std::path::Path,
    replay_index_path: &std::path::Path,
    id: &str,
) -> std::io::Result<ReplayData> {
    let checkpoint: ReplayCheckpoint =
        serde_json::from_slice(&std::fs::read(replay_index_path)?)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if checkpoint.version != REPLAY_INDEX_VERSION || checkpoint.session_id != id {
        return invalid_replay(id, "replay checkpoint identity mismatch");
    }
    if checkpoint.checksum != checkpoint_checksum(&checkpoint)? {
        return invalid_replay(id, "replay checkpoint checksum mismatch");
    }
    if checkpoint.active_start == 0
        || checkpoint.events.len() < 2
        || !matches!(checkpoint.events.first(), Some(SessionEvent::Meta { .. }))
        || !checkpoint
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::Compaction { .. }))
    {
        return invalid_replay(id, "invalid replay checkpoint window");
    }
    let observed = file_stamp(&file.metadata()?)?;
    if observed != checkpoint.observed || checkpoint.replay_offset > observed.len {
        return invalid_replay(id, "stale replay checkpoint");
    }
    validate_replay(&checkpoint.events, id)?;
    let ids_path = replay_ids_path(replay_index_path, id, &checkpoint.generation);
    let mut ids = ReplayIdIndex::open(&ids_path, &checkpoint.generation, &checkpoint.id_root)?;
    file.seek(std::io::SeekFrom::Start(checkpoint.replay_offset))?;
    let tail_len = observed.len - checkpoint.replay_offset;
    let mut tail = Vec::with_capacity(
        usize::try_from(tail_len)
            .map_err(|_| invalid_data("indexed replay tail does not fit this platform"))?,
    );
    (&mut *file).take(tail_len).read_to_end(&mut tail)?;
    if tail.len() as u64 != tail_len
        || file_stamp(&file.metadata()?)? != observed
        || file_stamp(&std::fs::metadata(path)?)? != observed
    {
        return invalid_replay(id, "session changed during indexed replay");
    }
    if !tail.is_empty() && !tail.ends_with(b"\n") {
        return invalid_replay(id, "indexed tail is not committed");
    }
    let tail_events = parse_event_bytes(&tail, id, checkpoint.physical_line_count)?;
    if tail_events.iter().any(|event| {
        matches!(
            event,
            SessionEvent::Compaction { .. } | SessionEvent::Rewind { .. }
        )
    }) {
        return invalid_replay(id, "stale replay checkpoint generation");
    }
    for record in id_records(&tail_events) {
        if ids.contains(&record)? {
            return invalid_replay(id, "tail id duplicates checkpoint history");
        }
    }
    let mut events = checkpoint.events.clone();
    events.extend(tail_events.iter().cloned());
    let unanswered_calls = validate_replay(&events, id)?;
    let mut effective_model = checkpoint.effective_model.clone();
    let mut effective_variant = checkpoint.effective_variant.clone();
    let mut todo_list = checkpoint.todo_list.clone();
    let mut topic = checkpoint.topic.clone();
    let checkpoint_tail_start = checkpoint.events.len();
    apply_replay_state(
        &tail_events,
        &mut effective_model,
        &mut effective_variant,
        &mut todo_list,
        &mut topic,
    );
    if file_stamp(&file.metadata()?)? != observed
        || file_stamp(&std::fs::metadata(path)?)? != observed
    {
        return invalid_replay(id, "session changed after indexed validation");
    }
    Ok(ReplayData {
        events,
        unanswered_calls,
        event_base: checkpoint.active_start,
        canonical_event_count: checkpoint
            .canonical_event_count
            .checked_add(tail_events.len())
            .ok_or_else(|| invalid_data("canonical event count overflow"))?,
        physical_line_count: checkpoint
            .physical_line_count
            .checked_add(committed_line_count(&tail))
            .ok_or_else(|| invalid_data("physical line count overflow"))?,
        effective_model,
        effective_variant,
        todo_list,
        topic,
        checkpoint: Some(checkpoint),
        checkpoint_tail_start,
        observed_stamp: observed,
    })
}

/// Where the newline-committed prefix of `bytes` ends. A trailing
/// partial line is not a record yet — every reader in this module cuts
/// here before parsing, which is what makes a torn tail harmless.
pub(super) fn committed_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1)
}

/// Parse committed JSONL bytes into events, naming any bad line by its
/// physical position (`line_offset` lines precede `bytes` in the file).
/// Shared with the incremental reader in [`super::tail`].
pub(super) fn parse_event_bytes(
    bytes: &[u8],
    id: &str,
    line_offset: usize,
) -> std::io::Result<Vec<SessionEvent>> {
    Ok(parse_event_lines(bytes, id, line_offset)?
        .into_iter()
        .map(|(_, event)| event)
        .collect())
}

/// [`parse_event_bytes`] keeping each event's physical line number, for
/// a diagnostic that has to name the line — a rewind marker that cuts
/// past the stream is a damaged line, and the reader deserves to know
/// which.
pub(super) fn parse_event_lines(
    bytes: &[u8],
    id: &str,
    line_offset: usize,
) -> std::io::Result<Vec<(usize, SessionEvent)>> {
    parse_event_lines_until(bytes, id, line_offset, &NEVER)
}

/// A flag nothing raises, for the readers nobody can walk away from.
static NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How often a cancellable parse asks whether it is still wanted. Often
/// enough that a caller does not wait on a whole archive, rarely enough
/// that the load costs nothing next to parsing the lines between.
const CANCEL_EVERY: usize = 256;

/// [`parse_event_lines`] that stops when `cancelled` is raised.
pub(super) fn parse_event_lines_until(
    bytes: &[u8],
    id: &str,
    line_offset: usize,
    cancelled: &std::sync::atomic::AtomicBool,
) -> std::io::Result<Vec<(usize, SessionEvent)>> {
    let mut events = Vec::new();
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if index % CANCEL_EVERY == 0 && cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("session {id}: read stopped; nobody is waiting for it"),
            ));
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let line_number = line_offset
            .checked_add(index)
            .and_then(|line| line.checked_add(1))
            .ok_or_else(|| invalid_data("physical line count overflow"))?;
        let line = std::str::from_utf8(line).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session {id}: invalid UTF-8 on line {line_number}: {error}"),
            )
        })?;
        let event = serde_json::from_str::<SessionEvent>(line).map_err(|error| {
            // Fail closed either way: an audit log never skips records.
            // Only the diagnosis differs.
            let message = match unknown_event_type(line) {
                Some(tag) => format!(
                    "session {id}: line {line_number} has unknown event type {tag:?}; written by a newer ilar?"
                ),
                None => format!("session {id}: malformed line {line_number}: {error}"),
            };
            std::io::Error::new(std::io::ErrorKind::InvalidData, message)
        })?;
        events.push((line_number, event));
    }
    Ok(events)
}

/// One full canonical replay: the folded events plus the raw shape of
/// the file they came from.
struct CanonicalReplay {
    events: Vec<SessionEvent>,
    unanswered_calls: Vec<String>,
    physical_line_count: usize,
    observed_stamp: FileStamp,
}

fn read_events(
    file: &mut File,
    path: &std::path::Path,
    id: &str,
    repair_tail: bool,
    cancelled: &std::sync::atomic::AtomicBool,
) -> std::io::Result<CanonicalReplay> {
    let expected = file_stamp(&file.metadata()?)?;
    if file_stamp(&std::fs::metadata(path)?)? != expected {
        return invalid_replay(id, "session path changed before canonical replay");
    }
    file.seek(std::io::SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(expected.len)
            .map_err(|_| invalid_data("canonical session does not fit this platform"))?,
    );
    (&mut *file).take(expected.len).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != expected.len
        || file_stamp(&file.metadata()?)? != expected
        || file_stamp(&std::fs::metadata(path)?)? != expected
    {
        return invalid_replay(id, "session changed during canonical replay");
    }
    let complete_len = committed_len(&bytes);
    let committed = &bytes[..complete_len];
    // Every committed line, counted before rewinds fold any of them away:
    // this is what a later tail-parse diagnostic offsets its line numbers
    // by, and the reader counts lines in the file, not surviving events.
    let physical_line_count = committed_line_count(committed);
    let events = fold_rewinds(parse_event_lines_until(committed, id, 0, cancelled)?, id)?;
    if events.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("session unrecoverable (no committed events): {id}"),
        ));
    }
    let unanswered_calls = validate_replay(&events, id)?;
    // Mutation happens only after every committed record validates.
    if repair_tail && complete_len < bytes.len() {
        if file_stamp(&file.metadata()?)? != expected
            || file_stamp(&std::fs::metadata(path)?)? != expected
        {
            return invalid_replay(id, "session changed before tail repair");
        }
        file.set_len(complete_len as u64)?;
        file.sync_data()?;
    }
    let final_stamp = file_stamp(&file.metadata()?)?;
    if final_stamp != file_stamp(&std::fs::metadata(path)?)?
        || complete_len == bytes.len() && final_stamp != expected
        || repair_tail && complete_len < bytes.len() && final_stamp.len != complete_len as u64
    {
        return invalid_replay(id, "session path changed during canonical replay");
    }
    Ok(CanonicalReplay {
        events,
        unanswered_calls,
        physical_line_count,
        observed_stamp: final_stamp,
    })
}

fn replay_state(
    events: &[SessionEvent],
) -> (
    String,
    Option<String>,
    Option<crate::todo::TodoList>,
    Option<String>,
) {
    let mut effective_model = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::Meta { meta, .. } => Some(meta.model.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let mut todo_list = None;
    let mut effective_variant = None;
    let mut topic = None;
    apply_replay_state(
        events,
        &mut effective_model,
        &mut effective_variant,
        &mut todo_list,
        &mut topic,
    );
    (effective_model, effective_variant, todo_list, topic)
}

/// Fold rewind markers out of a canonical event stream. Each marker
/// truncates the stream back to its `to` index — a position in the
/// already-folded stream, since markers are appended against the folded
/// view — and disappears itself. A marker that cuts past the stream is
/// a damaged file and refuses the replay: `truncate` would have
/// shrugged and kept everything, and the history the marker was meant
/// to abandon would have come back to life without a word.
pub(super) fn fold_rewinds(
    events: Vec<(usize, SessionEvent)>,
    id: &str,
) -> std::io::Result<Vec<SessionEvent>> {
    let mut folded = Vec::with_capacity(events.len());
    for (line, event) in events {
        match event {
            SessionEvent::Rewind { to, .. } => apply_rewind(&mut folded, to, id, line)?,
            event => folded.push(event),
        }
    }
    Ok(folded)
}

/// One rewind marker against a folded stream, the same check for the
/// full replay and the incremental tail: `to` may cut anywhere up to the
/// stream's end, and not past it.
pub(super) fn apply_rewind(
    folded: &mut Vec<SessionEvent>,
    to: usize,
    id: &str,
    line: usize,
) -> std::io::Result<()> {
    check_rewind(folded.len(), to, id, line)?;
    folded.truncate(to);
    Ok(())
}

/// The check alone, for a reader that wants to refuse a whole slab
/// before applying any of it.
pub(super) fn check_rewind(len: usize, to: usize, id: &str, line: usize) -> std::io::Result<()> {
    if to > len {
        return invalid_replay(
            id,
            format!(
                "rewind marker on line {line} cuts to event {to}, but only {len} events precede it"
            ),
        );
    }
    Ok(())
}

fn active_replay_window(events: &[SessionEvent]) -> (Vec<SessionEvent>, usize) {
    if !events
        .iter()
        .any(|event| matches!(event, SessionEvent::Compaction { .. }))
    {
        return (events.to_vec(), 0);
    }
    let base = compaction_cut(events);
    if base == 0 {
        return (events.to_vec(), 0);
    }
    let mut active = Vec::with_capacity(events.len() - base + 1);
    active.push(events[0].clone());
    active.extend(events[base..].iter().cloned());
    for event in &mut active {
        match event {
            SessionEvent::Compaction { kept_from, .. } => {
                *kept_from = kept_from.saturating_sub(base).saturating_add(1);
            }
            // The same re-basing: a cutoff is a canonical index too.
            SessionEvent::ImageCutoff { before, .. } => {
                *before = before.saturating_sub(base).saturating_add(1);
            }
            _ => {}
        }
    }
    (active, base)
}

fn apply_replay_state(
    events: &[SessionEvent],
    effective_model: &mut String,
    effective_variant: &mut Option<String>,
    todo_list: &mut Option<crate::todo::TodoList>,
    topic: &mut Option<String>,
) {
    for event in events {
        match event {
            SessionEvent::ModelChange { model, variant, .. } => {
                *effective_model = model.clone();
                *effective_variant = variant.clone();
            }
            SessionEvent::ToolResult {
                state: Some(crate::session::SessionState::TodoList { list }),
                ..
            } => *todo_list = Some(list.clone()),
            SessionEvent::Topic { text, .. } => *topic = Some(text.clone()),
            _ => {}
        }
    }
}

fn validate_replay(events: &[SessionEvent], id: &str) -> std::io::Result<Vec<String>> {
    let Some(SessionEvent::Meta { meta, .. }) = events.first() else {
        return invalid_replay(id, "metadata must be the first event");
    };
    if meta.session_id != id {
        return invalid_replay(
            id,
            format!(
                "metadata session id {:?} does not match filename",
                meta.session_id
            ),
        );
    }

    let mut event_ids = HashSet::new();
    let mut tool_call_ids = HashSet::new();
    let mut unanswered_calls: Vec<(String, String)> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if index > 0 && matches!(event, SessionEvent::Meta { .. }) {
            return invalid_replay(id, "duplicate metadata event");
        }

        let event_id = match event {
            SessionEvent::Meta { .. } => None,
            SessionEvent::UserMessage { id, .. }
            | SessionEvent::SubagentInvocation { id, .. }
            | SessionEvent::AssistantMessage { id, .. }
            | SessionEvent::ToolResult { id, .. }
            | SessionEvent::Checkpoint { id, .. }
            | SessionEvent::ModelChange { id, .. }
            | SessionEvent::Compaction { id, .. }
            | SessionEvent::Topic { id, .. }
            | SessionEvent::ImageCutoff { id, .. }
            | SessionEvent::MemoryRecall { id, .. }
            | SessionEvent::Rewind { id, .. }
            | SessionEvent::TurnEnded { id, .. } => Some(id),
        };
        if let Some(event_id) = event_id
            && !event_ids.insert(event_id)
        {
            return invalid_replay(id, format!("duplicate event id {event_id:?}"));
        }

        match event {
            SessionEvent::AssistantMessage { content, .. } => {
                if !unanswered_calls.is_empty() {
                    return invalid_replay(id, "new event before tool calls received results");
                }
                for block in content {
                    if let ContentBlock::ToolCall {
                        id: call_id, name, ..
                    } = block
                    {
                        if !tool_call_ids.insert(call_id) {
                            return invalid_replay(
                                id,
                                format!("duplicate tool call id {call_id:?}"),
                            );
                        }
                        unanswered_calls.push((call_id.clone(), name.clone()));
                    }
                }
            }
            SessionEvent::ToolResult {
                tool_use_id,
                is_error,
                state,
                ..
            } => {
                let Some(position) = unanswered_calls
                    .iter()
                    .position(|(call_id, _)| call_id == tool_use_id)
                else {
                    return invalid_replay(id, format!("orphan tool result for {tool_use_id:?}"));
                };
                let (_, tool_name) = &unanswered_calls[position];
                if let Some(state) = state {
                    if *is_error {
                        return invalid_replay(
                            id,
                            "error tool result cannot persist session state",
                        );
                    }
                    if tool_name != "todo" {
                        return invalid_replay(
                            id,
                            format!("todo state attached to non-todo tool {tool_name:?}"),
                        );
                    }
                    state.todo_list().validate().map_err(|error| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("session {id}: invalid todo state: {error}"),
                        )
                    })?;
                }
                unanswered_calls.remove(position);
            }
            SessionEvent::Meta { .. } | SessionEvent::SubagentInvocation { .. } => {}
            _ if !unanswered_calls.is_empty() => {
                return invalid_replay(id, "new event before tool calls received results");
            }
            _ => {}
        }
    }
    Ok(unanswered_calls
        .into_iter()
        .map(|(call_id, _)| call_id)
        .collect())
}

fn pending_question(
    events: &[SessionEvent],
    unanswered_calls: &[String],
) -> Option<PendingQuestion> {
    let [tool_call_id] = unanswered_calls else {
        return None;
    };
    let input = events.iter().rev().find_map(|event| match event {
        SessionEvent::AssistantMessage { content, .. } => {
            content.iter().find_map(|block| match block {
                ContentBlock::ToolCall {
                    id, name, input, ..
                } if id == tool_call_id && name == QUESTION_TOOL_NAME => Some(input),
                _ => None,
            })
        }
        _ => None,
    })?;
    let request: QuestionRequest = serde_json::from_value(input.clone()).ok()?;
    validate_request(&request).ok()?;
    Some(PendingQuestion {
        tool_call_id: tool_call_id.clone(),
        request,
    })
}

fn invalid_replay<T>(id: &str, message: impl std::fmt::Display) -> std::io::Result<T> {
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("session {id}: {message}"),
    ))
}

impl Session {
    /// The sole valid unanswered structured question, if this writable
    /// session was restored in a suspended state.
    pub fn pending_question(&self) -> Option<PendingQuestion> {
        let unanswered = validate_replay(&self.events, self.session_id()).ok()?;
        pending_question(&self.events, &unanswered)
    }

    /// Active replay events, in log order. Use `SessionStore::audit_events`
    /// when compacted-away canonical history is required.
    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// Whether this id has ever identified a tool call in the canonical
    /// session, including history omitted from the active compaction window.
    pub(crate) fn contains_tool_call_id(&self, id: &str) -> std::io::Result<bool> {
        if self.events.iter().any(|event| {
            matches!(event, SessionEvent::AssistantMessage { content, .. }
                if content.iter().any(|block| matches!(block,
                    ContentBlock::ToolCall { id: call_id, .. } if call_id == id)))
        }) {
            return Ok(true);
        }
        let Some(checkpoint) = &self.checkpoint else {
            return Ok(false);
        };
        let path = replay_ids_path(
            &self._writer.replay_index_path,
            self.session_id(),
            &checkpoint.generation,
        );
        ReplayIdIndex::open(&path, &checkpoint.generation, &checkpoint.id_root)?
            .contains(&id_record(1, id))
    }

    /// Session metadata (the Meta event), if present.
    pub fn meta(&self) -> Option<&SessionMeta> {
        self.events.iter().find_map(|e| match e {
            SessionEvent::Meta { meta, .. } => Some(meta),
            _ => None,
        })
    }

    /// The model this session currently runs on: the last ModelChange
    /// event, falling back to the session's meta model.
    pub fn effective_model(&self) -> String {
        self.effective_model.clone()
    }

    pub fn effective_variant(&self) -> Option<String> {
        self.effective_variant.clone()
    }

    /// Session id (empty string only in a pathological no-meta session).
    pub fn session_id(&self) -> &str {
        self.meta()
            .map(|m| m.session_id.as_str())
            .unwrap_or_default()
    }

    /// Append an event: persists one JSONL line, then updates the model.
    ///
    /// `Rewind` markers are reserved for `rewind_to`: appended raw they
    /// would leave this in-memory session unfolded while the file says
    /// otherwise.
    pub fn append(&mut self, event: SessionEvent) -> std::io::Result<()> {
        if matches!(event, SessionEvent::Rewind { .. }) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "rewind markers are appended through Session::rewind_to",
            ));
        }
        self.append_event(event)
    }

    fn append_event(&mut self, event: SessionEvent) -> std::io::Result<()> {
        let next_canonical_event_count = self
            .canonical_event_count
            .checked_add(1)
            .ok_or_else(|| invalid_data("canonical event count overflow"))?;
        let next_physical_line_count = self
            .physical_line_count
            .checked_add(1)
            .ok_or_else(|| invalid_data("physical line count overflow"))?;
        if file_stamp(&self.file.metadata()?)? != self.observed_stamp
            || file_stamp(&std::fs::metadata(&self._writer.session_path)?)? != self.observed_stamp
        {
            self.checkpoint = None;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "session {} changed outside its active writer",
                    self.session_id()
                ),
            ));
        }
        let local_compaction_cut = match &event {
            SessionEvent::Compaction { kept_from, .. } => Some(*kept_from),
            _ => None,
        };
        let canonical_event = match &event {
            SessionEvent::Compaction {
                id,
                summary,
                kept_from,
                ts,
            } => SessionEvent::Compaction {
                id: id.clone(),
                summary: summary.clone(),
                kept_from: self.canonical_index(*kept_from)?,
                ts: *ts,
            },
            SessionEvent::ImageCutoff { id, before, ts } => SessionEvent::ImageCutoff {
                id: id.clone(),
                before: self.canonical_index(*before)?,
                ts: *ts,
            },
            _ => event.clone(),
        };
        let mut line = serde_json::to_string(&canonical_event).map_err(std::io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        let observed_stamp = file_stamp(&self.file.metadata()?)?;
        if file_stamp(&std::fs::metadata(&self._writer.session_path)?)? != observed_stamp {
            self.checkpoint = None;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session {} path changed while appending", self.session_id()),
            ));
        }
        self.observed_stamp = observed_stamp;
        match &event {
            SessionEvent::Meta { meta, .. } => {
                self.effective_model = meta.model.clone();
                self.effective_variant = None;
            }
            SessionEvent::ModelChange { model, variant, .. } => {
                self.effective_model = model.clone();
                self.effective_variant = variant.clone();
            }
            SessionEvent::ToolResult {
                state: Some(crate::session::SessionState::TodoList { list }),
                ..
            } => self.todo_list = Some(list.clone()),
            _ => {}
        }
        self.events.push(event);
        self.canonical_event_count = next_canonical_event_count;
        self.physical_line_count = next_physical_line_count;
        if let Some(cut) = local_compaction_cut {
            let _ = self.publish_checkpoint(cut);
        } else {
            let _ = self.refresh_checkpoint();
        }
        Ok(())
    }

    /// Consume the session, appending a rewind marker that folds replay
    /// back to `cut` — the local index of a `UserMessage`, which
    /// becomes unsent. Consuming is the point: the in-memory state is
    /// pre-rewind, so nothing may keep using it; the next load sees the
    /// folded log.
    pub(crate) fn rewind_to(
        mut self,
        cut: usize,
        tree_restored: Option<String>,
        tree_saved: Option<String>,
    ) -> std::io::Result<RewindOutcome> {
        let unsent = self.rewind_target(cut)?.to_string();
        let to = self.canonical_index(cut)?;
        // Drop the replay index *before* the marker lands: with no index
        // on disk and `checkpoint` cleared, no crash point can leave a
        // stamp-valid index describing the pre-rewind window. (A crash
        // before the append merely costs the next writer a full parse.)
        if let Some(checkpoint) = &self.checkpoint {
            let ids_path = replay_ids_path(
                &self._writer.replay_index_path,
                self.session_id(),
                &checkpoint.generation,
            );
            let _ = std::fs::remove_file(ids_path);
        }
        let _ = std::fs::remove_file(&self._writer.replay_index_path);
        self.checkpoint = None;
        self.append_event(SessionEvent::Rewind {
            id: new_id(),
            to,
            tree_restored,
            tree_saved,
            ts: chrono::Utc::now(),
        })?;
        Ok(RewindOutcome { unsent })
    }

    /// Validate a rewind/fork cut, returning the user message text it
    /// would unsend.
    pub(crate) fn rewind_target(&self, cut: usize) -> std::io::Result<&str> {
        let invalid =
            |message: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, message);
        if self.pending_question().is_some() {
            return Err(invalid(
                "session has a pending question; answer or abort it before rewinding".into(),
            ));
        }
        let Some(SessionEvent::UserMessage { text, .. }) = self.events.get(cut) else {
            return Err(invalid(format!(
                "rewind cut {cut} is not a user message in session {}",
                self.session_id()
            )));
        };
        Ok(text)
    }

    /// Render the event log into provider-neutral chat messages.
    ///
    /// Tool results are grouped into a user message (matching assistant
    /// tool calls), as providers expect. The last compaction boundary
    /// replaces everything before it with the summary. Adjacent user
    /// messages are coalesced — providers enforce strict user/assistant
    /// alternation, and a compaction summary followed by a kept user
    /// message would otherwise violate it.
    pub fn transcript(&self) -> Vec<ChatMessage> {
        transcript_of(&self.events)
    }

    pub fn todo_list(&self) -> Option<&crate::todo::TodoList> {
        self.todo_list.as_ref()
    }

    pub fn topic(&self) -> Option<&str> {
        self.topic.as_deref()
    }

    fn canonical_index(&self, local: usize) -> std::io::Result<usize> {
        if self.event_base == 0 || local == 0 {
            Ok(local)
        } else {
            self.event_base
                .checked_add(local - 1)
                .ok_or_else(|| invalid_data("canonical event index overflow"))
        }
    }

    fn refresh_checkpoint(&mut self) -> std::io::Result<()> {
        let Some(mut checkpoint) = self.checkpoint.clone() else {
            return Ok(());
        };
        self.file.sync_data()?;
        if file_stamp(&self.file.metadata()?)? != self.observed_stamp
            || file_stamp(&std::fs::metadata(&self._writer.session_path)?)? != self.observed_stamp
        {
            return invalid_replay(
                self.session_id(),
                "session changed while sealing checkpoint",
            );
        }
        checkpoint.observed = self.observed_stamp.clone();
        checkpoint.checksum = checkpoint_checksum(&checkpoint)?;
        write_checkpoint(&self._writer.replay_index_path, &checkpoint)?;
        self.checkpoint = Some(checkpoint);
        Ok(())
    }

    fn publish_checkpoint(&mut self, local_cut: usize) -> std::io::Result<()> {
        if local_cut == 0 || local_cut >= self.events.len() {
            return Ok(());
        }
        self.file.sync_data()?;
        if file_stamp(&self.file.metadata()?)? != self.observed_stamp
            || file_stamp(&std::fs::metadata(&self._writer.session_path)?)? != self.observed_stamp
        {
            return invalid_replay(self.session_id(), "session changed while checkpointing");
        }
        let active_start = self.canonical_index(local_cut)?;
        let mut events = Vec::with_capacity(self.events.len() - local_cut + 1);
        events.push(self.events[0].clone());
        events.extend(self.events[local_cut..].iter().cloned());
        let Some(compaction) = events
            .iter_mut()
            .rev()
            .find(|event| matches!(event, SessionEvent::Compaction { .. }))
        else {
            return Ok(());
        };
        if let SessionEvent::Compaction { kept_from, .. } = compaction {
            *kept_from = 1;
        }
        // A cutoff in the window moves with it, like the compaction did.
        for event in &mut events {
            if let SessionEvent::ImageCutoff { before, .. } = event {
                *before = before.saturating_sub(local_cut).saturating_add(1);
            }
        }
        validate_replay(&self.events, self.session_id())?;
        validate_replay(&events, self.session_id())?;
        let generation = uuid::Uuid::new_v4().to_string();
        let mut records = if let Some(previous) = &self.checkpoint {
            let path = replay_ids_path(
                &self._writer.replay_index_path,
                self.session_id(),
                &previous.generation,
            );
            read_all_id_records(&path, &previous.generation, &previous.id_root)?
        } else {
            Vec::new()
        };
        let new_events = &self.events[self.checkpoint_tail_start.min(self.events.len())..];
        let mut new_records = id_records(new_events);
        new_records.sort_unstable();
        if new_records.windows(2).any(|pair| pair[0] == pair[1])
            || new_records
                .iter()
                .any(|record| records.binary_search(record).is_ok())
        {
            return invalid_replay(self.session_id(), "duplicate id while checkpointing");
        }
        records.extend(new_records);
        records.sort_unstable();
        let ids_path = replay_ids_path(
            &self._writer.replay_index_path,
            self.session_id(),
            &generation,
        );
        let id_root = write_id_records(&ids_path, &generation, &records)?;
        let mut checkpoint = ReplayCheckpoint {
            version: REPLAY_INDEX_VERSION,
            generation: generation.clone(),
            session_id: self.session_id().to_string(),
            replay_offset: self.file.metadata()?.len(),
            canonical_event_count: self.canonical_event_count,
            physical_line_count: self.physical_line_count,
            active_start,
            events,
            effective_model: self.effective_model.clone(),
            effective_variant: self.effective_variant.clone(),
            todo_list: self.todo_list.clone(),
            topic: self.topic.clone(),
            id_root,
            observed: self.observed_stamp.clone(),
            checksum: String::new(),
        };
        checkpoint.checksum = checkpoint_checksum(&checkpoint)?;
        write_checkpoint(&self._writer.replay_index_path, &checkpoint)?;
        let previous_generation = self
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.generation.clone());
        self.checkpoint = Some(checkpoint);
        self.checkpoint_tail_start = self.events.len();
        if let Some(previous_generation) = previous_generation
            && previous_generation != generation
        {
            let old_path = replay_ids_path(
                &self._writer.replay_index_path,
                self.session_id(),
                &previous_generation,
            );
            let _ = std::fs::remove_file(old_path);
        }
        Ok(())
    }

    fn rebuild_checkpoint(&mut self) -> std::io::Result<()> {
        if self
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::Compaction { .. }))
        {
            self.publish_checkpoint(compaction_cut(&self.events))
        } else {
            Ok(())
        }
    }
}

/// Append blocks as a user message, coalescing with a preceding user
/// message to preserve user/assistant alternation.
fn push_user_blocks(messages: &mut Vec<ChatMessage>, blocks: Vec<ContentBlock>) {
    match messages.last_mut() {
        Some(last) if last.role == Role::User => last.content.extend(blocks),
        _ => messages.push(ChatMessage {
            role: Role::User,
            content: blocks,
        }),
    }
}

impl Session {
    /// In-memory view of `events[..cut]` for summarization (compaction).
    pub fn from_events_for_compaction(events: &[SessionEvent], cut: usize) -> SessionReader {
        let events = events[..cut.min(events.len())].to_vec();
        let (effective_model, effective_variant, todo_list, topic) = replay_state(&events);
        SessionReader {
            events,
            effective_model,
            effective_variant,
            todo_list,
            topic,
            pending_question: None,
        }
    }
}

/// Read-only session view (compaction input).
pub struct SessionReader {
    events: Vec<SessionEvent>,
    effective_model: String,
    effective_variant: Option<String>,
    todo_list: Option<crate::todo::TodoList>,
    topic: Option<String>,
    pending_question: Option<PendingQuestion>,
}

impl SessionReader {
    /// Active replay events. Canonical audit history remains available through
    /// `SessionStore::audit_events` without being materialized on normal loads.
    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    pub fn meta(&self) -> Option<&SessionMeta> {
        self.events.iter().find_map(|event| match event {
            SessionEvent::Meta { meta, .. } => Some(meta),
            _ => None,
        })
    }

    pub fn effective_model(&self) -> String {
        self.effective_model.clone()
    }

    pub fn effective_variant(&self) -> Option<String> {
        self.effective_variant.clone()
    }

    pub fn session_id(&self) -> &str {
        self.meta()
            .map(|meta| meta.session_id.as_str())
            .unwrap_or_default()
    }

    pub fn transcript(&self) -> Vec<ChatMessage> {
        transcript_of(&self.events)
    }

    pub fn todo_list(&self) -> Option<&crate::todo::TodoList> {
        self.todo_list.as_ref()
    }

    /// A few words naming what this session is about, once one has been
    /// generated.
    pub fn topic(&self) -> Option<&str> {
        self.topic.as_deref()
    }

    /// Returns the sole validated question tool call awaiting a result.
    pub fn pending_question(&self) -> Option<&PendingQuestion> {
        self.pending_question.as_ref()
    }
}

/// Pure transcript rendering over an event slice.
pub fn transcript_of(events: &[SessionEvent]) -> Vec<ChatMessage> {
    let mut cut = compaction_cut(events);
    let mut summary: Option<&str> = None;
    for event in events {
        if let SessionEvent::Compaction { summary: s, .. } = event {
            summary = Some(s);
        }
    }
    while cut < events.len() && matches!(events[cut], SessionEvent::ToolResult { .. }) {
        cut += 1;
    }

    let mut messages: Vec<ChatMessage> = Vec::new();
    if let Some(summary) = summary {
        messages.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                // The trailing line outranks any stop-flavored wording the
                // summarizer may have carried over: the checkpoint replaced
                // the conversation, never the goal.
                text: format!(
                    "<compaction-summary>\n{summary}\n</compaction-summary>\n\
                    Continue the task from this state — the checkpoint replaced the \
                    earlier conversation, not the goal."
                ),
            }],
        });
    }

    // Pictures before the latest image cutoff do not travel; a note
    // stands where each was.
    let image_cut = crate::image::image_cut(events);
    let mut pending_results: Vec<ContentBlock> = Vec::new();
    for (index, event) in events.iter().enumerate().skip(cut) {
        let pictures_travel = index >= image_cut;
        match event {
            SessionEvent::Meta { .. }
            | SessionEvent::SubagentInvocation { .. }
            | SessionEvent::Checkpoint { .. }
            | SessionEvent::Topic { .. }
            | SessionEvent::ImageCutoff { .. }
            | SessionEvent::Rewind { .. }
            | SessionEvent::TurnEnded { .. } => {}
            // After the user message it was surfaced for, as one more
            // block of that message.
            SessionEvent::MemoryRecall { text, .. } => {
                if !pending_results.is_empty() {
                    push_user_blocks(&mut messages, std::mem::take(&mut pending_results));
                }
                push_user_blocks(
                    &mut messages,
                    vec![ContentBlock::Text { text: text.clone() }],
                );
            }
            SessionEvent::UserMessage { text, images, .. } => {
                if !pending_results.is_empty() {
                    push_user_blocks(&mut messages, std::mem::take(&mut pending_results));
                }
                let mut blocks = vec![ContentBlock::Text { text: text.clone() }];
                if pictures_travel {
                    blocks.extend(images.iter().map(|image| ContentBlock::Image {
                        image: image.clone(),
                    }));
                } else if !images.is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: crate::image::IMAGE_ELIDED.to_string(),
                    });
                }
                push_user_blocks(&mut messages, blocks);
            }
            SessionEvent::AssistantMessage { content, .. } => {
                if !pending_results.is_empty() {
                    push_user_blocks(&mut messages, std::mem::take(&mut pending_results));
                }
                if !content.is_empty() {
                    messages.push(ChatMessage {
                        role: Role::Assistant,
                        content: content.clone(),
                    });
                }
            }
            SessionEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
                images,
                ..
            } => {
                let (content, images) = if pictures_travel || images.is_empty() {
                    (content.clone(), images.clone())
                } else {
                    (
                        format!("{content}\n{}", crate::image::IMAGE_ELIDED),
                        Vec::new(),
                    )
                };
                pending_results.push(ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content,
                    is_error: *is_error,
                    images,
                });
            }
            SessionEvent::ModelChange { .. } | SessionEvent::Compaction { .. } => {}
        }
    }
    if !pending_results.is_empty() {
        push_user_blocks(&mut messages, pending_results);
    }
    messages
}

pub fn compaction_cut(events: &[SessionEvent]) -> usize {
    events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            SessionEvent::Compaction { kept_from, .. } => Some((*kept_from).min(index)),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_cutoff_is_re_based_with_the_compaction_it_follows() {
        use super::*;
        let now = chrono::Utc::now();
        let user = |id: &str| SessionEvent::UserMessage {
            id: id.into(),
            text: id.into(),
            images: vec![],
            ts: now,
        };
        let events = vec![
            SessionEvent::Meta {
                meta: crate::session::SessionMeta {
                    session_id: "s".into(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "m".into(),
                    workspace: None,
                    cwd: None,
                },
                ts: now,
            },
            user("u1"),
            user("u2"),
            SessionEvent::Compaction {
                id: "c".into(),
                summary: "sum".into(),
                kept_from: 2,
                ts: now,
            },
            SessionEvent::ImageCutoff {
                id: "x".into(),
                before: 4,
                ts: now,
            },
            user("u3"),
        ];
        let (active, base) = active_replay_window(&events);
        assert_eq!(base, 2);
        let before = active
            .iter()
            .find_map(|event| match event {
                SessionEvent::ImageCutoff { before, .. } => Some(*before),
                _ => None,
            })
            .unwrap();
        // Canonical 4 (the cutoff's own slot) becomes local 3: meta at
        // 0, then u2, compaction, cutoff.
        assert_eq!(before, 3);
    }

    #[test]
    fn an_image_cutoff_keeps_the_words_and_drops_the_pictures_before_it() {
        use super::*;
        use crate::session::{ImageContent, Usage};
        let image = ImageContent::new("image/png", b"pix");
        let now = chrono::Utc::now();
        let events = vec![
            SessionEvent::UserMessage {
                id: "u1".into(),
                text: "first".into(),
                images: vec![image.clone()],
                ts: now,
            },
            SessionEvent::AssistantMessage {
                id: "a1".into(),
                model: "m".into(),
                content: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                    item_id: None,
                }],
                usage: Usage::default(),
                stop_reason: "tool_use".into(),
                ts: now,
            },
            SessionEvent::ToolResult {
                id: "r1".into(),
                tool_use_id: "c1".into(),
                content: "(binary file: a.png)".into(),
                is_error: false,
                images: vec![image.clone()],
                child_session_id: None,
                state: None,
                ts: now,
            },
            SessionEvent::ImageCutoff {
                id: "x1".into(),
                before: 3,
                ts: now,
            },
            SessionEvent::UserMessage {
                id: "u2".into(),
                text: "second".into(),
                images: vec![image.clone()],
                ts: now,
            },
        ];
        let messages = transcript_of(&events);
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(matches!(
            &messages[0].content[..],
            [ContentBlock::Text { text }, ContentBlock::Text { text: note }]
                if text == "first" && note == crate::image::IMAGE_ELIDED
        ));
        // The tool result and the next user message share one user
        // message: the result's picture is gone, the newer one travels.
        let [
            ContentBlock::ToolResult {
                content, images, ..
            },
            ContentBlock::Text { text },
            ContentBlock::Image { .. },
        ] = &messages[2].content[..]
        else {
            panic!("{messages:?}");
        };
        assert!(images.is_empty());
        assert!(content.ends_with(crate::image::IMAGE_ELIDED), "{content}");
        assert_eq!(text, "second");
    }

    use super::*;

    fn user_message(text: &str) -> SessionEvent {
        SessionEvent::UserMessage {
            id: new_id(),
            text: text.into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        }
    }

    fn assistant_message(text: &str) -> SessionEvent {
        SessionEvent::AssistantMessage {
            id: new_id(),
            model: "test/model".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: super::super::model::Usage::default(),
            stop_reason: "end_turn".into(),
            ts: chrono::Utc::now(),
        }
    }

    /// A flock holds an inode, and an inode outlives its name. This is
    /// the check that tells the two apart — without it, a waiter that
    /// wins the lock on a file `delete()` has already unlinked believes
    /// it owns a session a third process is locking freshly at the same
    /// path.
    #[cfg(unix)]
    #[test]
    fn a_lock_knows_whether_it_still_holds_the_path_it_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(locked_the_named_file(&file, &path).unwrap());

        std::fs::remove_file(&path).unwrap();
        assert!(
            !locked_the_named_file(&file, &path).unwrap(),
            "an unlinked lock still claimed its path"
        );

        // A third process opening the same path gets a different inode.
        std::fs::write(&path, b"").unwrap();
        assert!(
            !locked_the_named_file(&file, &path).unwrap(),
            "the orphan claimed the file that replaced it"
        );
    }

    /// The store scans for `.jsonl` and nothing else. Pinned because the
    /// session directory now also holds live-turn scratches, which are
    /// ephemera the audit view must never see — not in the listing, not
    /// as a child, not as a head.
    #[test]
    fn the_listing_ignores_everything_that_is_not_a_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let id = new_id();
        let session = store
            .create(SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "test/model".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        drop(session);

        // The sidecars a running turn leaves beside the log, plus a
        // scratch whose stem is a perfectly good session id.
        let scratch = super::super::live::live_path(&store.session_path(&id).unwrap());
        std::fs::write(
            &scratch,
            b"{\"type\":\"turn_started\",\"turn\":\"t-1\",\"step\":0}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(format!("{}.live", new_id())), b"").unwrap();

        assert_eq!(
            store
                .list()
                .iter()
                .map(|s| s.id.clone())
                .collect::<Vec<_>>(),
            vec![id.clone()]
        );
        assert!(store.children_of(&id).is_empty());
        assert!(scratch.exists(), "the store did not touch the scratch");
    }

    /// The pointer is written when a root session is created, believed
    /// without reading the directory, and refused the moment it names
    /// something it should not.
    #[test]
    fn a_directory_is_pointed_at_the_session_last_used_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let here = tempfile::tempdir().unwrap();
        let here = std::fs::canonicalize(here.path()).unwrap();
        let meta = |session_id: &str, parent_id: Option<&str>, cwd: Option<&Path>| SessionMeta {
            session_id: session_id.into(),
            parent_id: parent_id.map(str::to_string),
            agent: "build".into(),
            model: "test/model".into(),
            workspace: None,
            cwd: cwd.map(Path::to_path_buf),
        };

        let first = new_id();
        drop(store.create(meta(&first, None, Some(&here))).unwrap());
        assert_eq!(
            store.last_in(&here).map(|session| session.id),
            Some(first.clone()),
            "creating a session did not point its directory at it"
        );

        // A child records no directory of its own and never becomes
        // one's answer.
        let child = new_id();
        drop(
            store
                .create(meta(&child, Some(&first), Some(&here)))
                .unwrap(),
        );
        assert_eq!(
            store.last_in(&here).map(|session| session.id),
            Some(first.clone())
        );

        // The pointer is what is believed, not the newest file: nothing
        // lists the directory to find this out.
        let second = new_id();
        drop(store.create(meta(&second, None, Some(&here))).unwrap());
        assert_eq!(
            store.last_in(&here).map(|session| session.id),
            Some(second.clone())
        );
        store.remember_last(&first);
        assert_eq!(
            store.last_in(&here).map(|session| session.id),
            Some(first.clone())
        );

        // Deleted, and no longer pointed at.
        store.delete(&first).unwrap();
        assert!(store.last_in(&here).is_none(), "a dead session was named");
        // The repair a caller makes after falling back to the listing.
        store.remember_last(&second);
        assert_eq!(store.last_in(&here).map(|session| session.id), Some(second));

        // Another directory's session is never this directory's answer,
        // however the pointer came to name it.
        let elsewhere = tempfile::tempdir().unwrap();
        let elsewhere = std::fs::canonicalize(elsewhere.path()).unwrap();
        assert!(store.last_in(&elsewhere).is_none());
    }

    /// A session created by a launch and never typed into goes when its
    /// runtime ends — and one that was typed into, one that spawned a
    /// task, and one with mail waiting in the outbox all stay.
    #[test]
    fn only_a_session_with_nothing_in_it_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let outbox = dir.path().join("outbox");
        let here = tempfile::tempdir().unwrap();
        let here = std::fs::canonicalize(here.path()).unwrap();
        let meta = |session_id: &str, parent_id: Option<&str>| SessionMeta {
            session_id: session_id.into(),
            parent_id: parent_id.map(str::to_string),
            agent: "build".into(),
            model: "test/model".into(),
            workspace: None,
            cwd: Some(here.clone()),
        };

        // Opened and quit: a Meta line and nothing else.
        let untouched = new_id();
        drop(store.create(meta(&untouched, None)).unwrap());
        assert!(store.remove_if_empty(&untouched, &outbox));
        assert!(!store.session_path(&untouched).unwrap().exists());
        assert!(
            last_by_dir::load(&store.root).get(&here).is_none(),
            "the directory is still pointed at a session that is gone"
        );
        // Idempotent: a session that is already gone is not an error.
        assert!(!store.remove_if_empty(&untouched, &outbox));

        // Somebody said something.
        let spoken = new_id();
        let mut session = store.create(meta(&spoken, None)).unwrap();
        session.append(user_message("do the thing")).unwrap();
        drop(session);
        assert!(!store.remove_if_empty(&spoken, &outbox));

        // Empty, but it has a task of its own: the child's log names it
        // as its parent, and an orphan is worse than a stale row.
        let parent = new_id();
        drop(store.create(meta(&parent, None)).unwrap());
        drop(store.create(meta(&new_id(), Some(&parent))).unwrap());
        assert!(!store.remove_if_empty(&parent, &outbox));

        // Empty, but a completion is waiting for it.
        let addressee = new_id();
        drop(store.create(meta(&addressee, None)).unwrap());
        std::fs::create_dir_all(&outbox).unwrap();
        std::fs::write(outbox.join(format!("{addressee}.jsonl")), b"{}\n").unwrap();
        assert!(!store.remove_if_empty(&addressee, &outbox));

        // A child's own session is never removed this way: its runtime
        // is its parent's, and Task is what ends it.
        let child = new_id();
        drop(store.create(meta(&child, Some(&new_id()))).unwrap());
        assert!(!store.remove_if_empty(&child, &outbox));
    }

    /// The startup sweep is for what a crash or a switch left behind, so
    /// it waits a day: a session opened minutes ago may be sitting at a
    /// blank prompt in another terminal right now.
    #[test]
    fn the_sweep_takes_only_the_empty_sessions_that_have_aged() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let outbox = dir.path().join("outbox");
        let meta = |session_id: &str| SessionMeta {
            session_id: session_id.into(),
            parent_id: None,
            agent: "build".into(),
            model: "test/model".into(),
            workspace: None,
            cwd: None,
        };
        let fresh = new_id();
        drop(store.create(meta(&fresh)).unwrap());
        let old = new_id();
        drop(store.create(meta(&old)).unwrap());
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
        File::options()
            .write(true)
            .open(store.session_path(&old).unwrap())
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(long_ago)
                    .set_accessed(long_ago),
            )
            .unwrap();

        let removed =
            store.sweep_empty_sessions(&outbox, std::time::Duration::from_secs(24 * 3600));

        assert_eq!(removed, 1);
        assert!(!store.session_path(&old).unwrap().exists());
        assert!(store.session_path(&fresh).unwrap().exists());
    }

    /// A lease that ends takes its lock file with it, and a lease that
    /// is held keeps it — otherwise every session ever opened leaves a
    /// `.lock` in the directory for ever (2,414 of them, measured).
    #[test]
    fn a_lock_file_lives_exactly_as_long_as_the_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let id = new_id();
        let session = store
            .create(SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "test/model".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        let lock = dir.path().join(format!("{id}.lock"));
        assert!(lock.exists(), "a live lease has no lock file");

        drop(session);
        assert!(!lock.exists(), "the lock outlived its lease");

        // And a fresh writer makes one again: the lease, not the file,
        // is what exclusion is built on.
        let writer = store.acquire_writer(&id).unwrap();
        assert!(lock.exists());
        let Err(error) = store.acquire_writer(&id) else {
            panic!("a second writer got in");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        drop(writer);
        assert!(!lock.exists());
    }

    /// `delete` unlinks the lock while holding it, so by the time that
    /// writer drops, the path may already belong to somebody else. The
    /// drop must not take that file with it: a holder left with a
    /// nameless inode while a third writer locks a fresh file at the
    /// same path is two owners of one session.
    #[cfg(unix)]
    #[test]
    fn a_drop_never_removes_a_lock_file_that_is_not_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let id = new_id();
        let writer = store.acquire_writer(&id).unwrap();
        let lock = dir.path().join(format!("{id}.lock"));

        // What `delete` does to a lock it holds.
        std::fs::remove_file(&lock).unwrap();
        // And what the next writer does at the same path.
        let successor = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        FileExt::try_lock_exclusive(&successor).unwrap();

        drop(writer);

        assert!(
            lock.exists(),
            "the drop unlinked the lock file its successor is holding"
        );
        // And the sweep leaves it alone too: the lock is held.
        sweep_stale_locks(dir.path());
        assert!(lock.exists());
        drop(successor);
    }

    /// The startup sweep takes the locks a crash left behind and leaves
    /// the one a running turn holds.
    #[test]
    fn the_startup_sweep_removes_only_the_locks_nobody_holds() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let held = new_id();
        let writer = store.acquire_writer(&held).unwrap();
        // What a killed process leaves: the file, with no lock on it.
        let abandoned = new_id();
        std::fs::write(dir.path().join(format!("{abandoned}.lock")), b"").unwrap();
        // Not a session's lock at all, and not the sweep's business.
        std::fs::write(dir.path().join("outbox.lock"), b"").unwrap();

        sweep_stale_locks(dir.path());

        assert!(
            !dir.path().join(format!("{abandoned}.lock")).exists(),
            "a stale lock survived the sweep"
        );
        assert!(
            dir.path().join(format!("{held}.lock")).exists(),
            "the sweep took a lock a turn was holding"
        );
        assert!(dir.path().join("outbox.lock").exists());
        drop(writer);
    }

    /// The listing answers out of the summary cache while a file's
    /// stamp holds, and rereads it when the stamp moves. Observed the
    /// only way a caller can see it: content the cache cannot know
    /// about, written under an unchanged stamp.
    #[test]
    fn the_listing_rereads_a_session_only_when_its_stamp_moved() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "test/model".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session.append(user_message("the first thing")).unwrap();
        drop(session);

        assert_eq!(
            store.list().first().and_then(|row| row.title.clone()),
            Some("the first thing".into())
        );
        assert!(
            super::super::summary_cache::cache_path(dir.path()).exists(),
            "the listing wrote no cache"
        );

        // Same length, same mtime, different words: only a reread could
        // see this, and the cache's whole job is not to.
        let path = store.session_path(&id).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let rewritten = std::fs::read_to_string(&path)
            .unwrap()
            .replace("the first thing", "the second thin");
        std::fs::write(&path, &rewritten).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(before.modified().unwrap())
                    .set_accessed(before.modified().unwrap()),
            )
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before.len());
        assert_eq!(
            store.list().first().and_then(|row| row.title.clone()),
            Some("the first thing".into()),
            "the listing reopened a file whose stamp had not moved"
        );

        // The stamp moves the moment the log grows, and the reread
        // catches up.
        let mut session = store.acquire_writer(&id).unwrap().load().unwrap();
        session
            .append(SessionEvent::Topic {
                id: new_id(),
                text: "a name of its own".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);
        assert_eq!(
            store.list().first().and_then(|row| row.title.clone()),
            Some("a name of its own".into())
        );
    }

    /// The cache remembers which files are children, so a roster opens
    /// its own tasks and nothing else — and still finds a child whose
    /// file appeared after the cache was written.
    #[test]
    fn a_roster_finds_its_children_through_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let parent = new_id();
        let meta = |session_id: &str, parent_id: Option<&str>| SessionMeta {
            session_id: session_id.into(),
            parent_id: parent_id.map(str::to_string),
            agent: "build".into(),
            model: "test/model".into(),
            workspace: None,
            cwd: None,
        };
        drop(store.create(meta(&parent, None)).unwrap());
        let stranger = new_id();
        drop(store.create(meta(&stranger, Some(&new_id()))).unwrap());
        // Warm the cache.
        assert_eq!(store.list().len(), 1);

        let kid = new_id();
        let mut child = store.create(meta(&kid, Some(&parent))).unwrap();
        child.append(user_message("go and look")).unwrap();
        drop(child);

        let children = store.children_of(&parent);
        assert_eq!(children.len(), 1, "{children:?}");
        assert_eq!(children[0].id, kid);
        assert_eq!(children[0].title.as_deref(), Some("go and look"));
        assert!(store.children_of(&stranger).is_empty());
    }

    /// Tail-parse diagnostics name a line in the file, so the offset the
    /// checkpoint carries has to be a physical line count. A rewind makes
    /// the folded event count smaller than the file — the two numbers must
    /// not be confused.
    #[test]
    fn tail_diagnostics_report_the_physical_line_after_a_rewind() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "test/model".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        // Lines 2..5: two turns, the second of which the rewind abandons.
        session.append(user_message("first")).unwrap();
        session.append(assistant_message("did first")).unwrap();
        session.append(user_message("second")).unwrap();
        session.append(assistant_message("did second")).unwrap();
        // Line 6: the rewind marker. Replay now folds back to 3 events
        // while the file holds 6 lines.
        session.rewind_to(3, None, None).unwrap();

        // Lines 7..9: a fresh turn plus a compaction, which publishes a
        // checkpoint whose tail offset is what we are pinning.
        let mut session = store.acquire_writer(&id).unwrap().load().unwrap();
        session.append(user_message("third")).unwrap();
        session.append(assistant_message("did third")).unwrap();
        session
            .append(SessionEvent::Compaction {
                id: new_id(),
                summary: "the story so far".into(),
                kept_from: 3,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        // Line 10: corrupt. Re-seal the checkpoint over the new file so
        // the indexed path is the one that reports it.
        let session_path = store.session_path(&id).unwrap();
        let index_path = store.replay_index_path(&id).unwrap();
        let mut file = OpenOptions::new().append(true).open(&session_path).unwrap();
        file.write_all(b"{\"broken\"\n").unwrap();
        file.sync_data().unwrap();
        let mut checkpoint: ReplayCheckpoint =
            serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
        checkpoint.observed = file_stamp(&std::fs::metadata(&session_path).unwrap()).unwrap();
        checkpoint.checksum = checkpoint_checksum(&checkpoint).unwrap();
        write_checkpoint(&index_path, &checkpoint).unwrap();

        let mut file = File::open(&session_path).unwrap();
        let Err(error) = read_indexed_replay(&mut file, &session_path, &index_path, &id) else {
            panic!("expected the corrupt tail to be rejected");
        };
        assert!(
            error.to_string().contains("malformed line 10"),
            "expected the real file line, got: {error}"
        );
    }
}
