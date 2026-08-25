//! Append-only JSONL session store.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::PathBuf;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::event::{SessionEvent, SessionMeta, new_id, unknown_event_type};
use super::model::{ChatMessage, ContentBlock, Role};
use crate::question::{QUESTION_TOOL_NAME, QuestionRequest, validate_request};

/// Owns the session directory; creates/loads sessions.
#[derive(Clone)]
pub struct SessionStore {
    root: PathBuf,
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
    effective_model: String,
    effective_variant: Option<String>,
    todo_list: Option<crate::todo::TodoList>,
    topic: Option<String>,
    checkpoint: Option<ReplayCheckpoint>,
    checkpoint_tail_start: usize,
    observed_stamp: FileStamp,
}

/// Exclusive OS-backed writer ownership for one session. The lock file is
/// persistent, but the lock itself is released by the OS on drop or crash.
pub struct SessionWriter {
    _file: File,
    id: SessionId,
    session_path: PathBuf,
    replay_index_path: PathBuf,
}

const REPLAY_INDEX_VERSION: u32 = 2;
const REPLAY_IDS_MAGIC: &[u8; 8] = b"ILARIDS1";
const REPLAY_IDS_HEADER_LEN: u64 = 32;
const REPLAY_ID_RECORD_LEN: u64 = 33;
const REPLAY_ID_PAGE_RECORDS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileStamp {
    len: u64,
    modified_nanos: u64,
    #[serde(default)]
    device: u64,
    #[serde(default)]
    inode: u64,
    #[serde(default)]
    changed_seconds: i64,
    #[serde(default)]
    changed_nanos: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct ReplayCheckpoint {
    version: u32,
    generation: String,
    session_id: String,
    replay_offset: u64,
    canonical_event_count: usize,
    physical_line_count: usize,
    active_start: usize,
    events: Vec<SessionEvent>,
    effective_model: String,
    #[serde(default)]
    effective_variant: Option<String>,
    todo_list: Option<crate::todo::TodoList>,
    #[serde(default)]
    topic: Option<String>,
    id_root: String,
    observed: FileStamp,
    checksum: String,
}

struct ReplayData {
    events: Vec<SessionEvent>,
    unanswered_calls: Vec<String>,
    event_base: usize,
    canonical_event_count: usize,
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
}

/// A session file's head: enough to summarize it without reading the
/// whole log.
struct SessionHead {
    id: String,
    meta: SessionMeta,
    title: Option<String>,
    modified: std::time::SystemTime,
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
    if collapsed.chars().count() > SUMMARY_TITLE_CHARS {
        let mut title: String = collapsed.chars().take(SUMMARY_TITLE_CHARS).collect();
        title.push('…');
        title
    } else {
        collapsed
    }
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
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
        let id = SessionId::parse(id)?;
        let path = self.session_path_for(&id);
        let bytes = std::fs::read(&path)?;
        let complete_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1);
        parse_event_bytes(&bytes[..complete_len], id.as_str(), 0)
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

    fn acquire_writer_id(&self, id: SessionId) -> std::io::Result<SessionWriter> {
        std::fs::create_dir_all(&self.root)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(format!("{id}.lock")))?;
        if let Err(error) = FileExt::try_lock_exclusive(&file) {
            let contended = fs2::lock_contended_error();
            return Err(
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || contended.raw_os_error().is_some()
                        && error.raw_os_error() == contended.raw_os_error()
                {
                    std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        format!("session {id} already active in another turn"),
                    )
                } else {
                    error
                },
            );
        }
        Ok(SessionWriter {
            _file: file,
            session_path: self.session_path_for(&id),
            replay_index_path: self.replay_index_path_for(&id),
            id,
        })
    }

    /// Create a new session; writes the Meta event as the first line.
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
            effective_model: meta.model.clone(),
            effective_variant: None,
            todo_list: None,
            topic: None,
            checkpoint: None,
            checkpoint_tail_start: 0,
            observed_stamp,
        };
        session.append(SessionEvent::Meta {
            meta,
            ts: chrono::Utc::now(),
        })?;
        Ok(session)
    }

    /// List root (non-subagent) sessions, most recently modified first.
    /// Reads only each file's head; unreadable, foreign, headless, or
    /// child-session files are skipped — see
    /// meta/issues/session-list-and-resume-last.md.
    pub fn list(&self) -> Vec<SessionSummary> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut sessions: Vec<SessionSummary> = entries
            .flatten()
            .filter_map(|entry| self.summarize_entry(&entry))
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

    /// The subagent tasks spawned by `parent_id`, newest first.
    /// [`Self::list`] hides children by construction — this is the other
    /// half, and it is scoped: a session sees its own tasks only.
    pub fn children_of(&self, parent_id: &str) -> Vec<ChildSummary> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut children: Vec<(std::time::SystemTime, ChildSummary)> = entries
            .flatten()
            .filter_map(|entry| self.scan_head(&entry))
            .filter(|head| head.meta.parent_id.as_deref() == Some(parent_id))
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

    fn summarize_entry(&self, entry: &std::fs::DirEntry) -> Option<SessionSummary> {
        let head = self.scan_head(entry)?;
        if head.meta.parent_id.is_some() {
            return None;
        }
        Some(SessionSummary {
            id: head.id,
            title: head.title,
            modified: head.modified,
        })
    }

    /// Parse a session file's head: its metadata and opening prompt,
    /// without loading the whole log.
    fn scan_head(&self, entry: &std::fs::DirEntry) -> Option<SessionHead> {
        let name = entry.file_name();
        let id = name.to_str()?.strip_suffix(".jsonl")?.to_string();
        SessionId::parse(&id).ok()?;
        let modified = entry.metadata().ok()?.modified().ok()?;
        let file = File::open(entry.path()).ok()?;
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
        let title = topic.or(opening);
        Some(SessionHead {
            id,
            meta,
            title,
            modified,
        })
    }

    /// Delete a session's files. Refuses sessions whose writer lease is
    /// held (active in some turn) with `WouldBlock`.
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        let parsed = SessionId::parse(id)?;
        let lock_path = self.root.join(format!("{parsed}.lock"));
        // Unlink everything while the lease is held: removing the lock
        // after release would race a new holder of the same path.
        let _writer = self.acquire_writer_id(parsed.clone())?;
        let _ = std::fs::remove_file(self.replay_index_path_for(&parsed));
        for path in self.replay_ids_paths_for(&parsed) {
            let _ = std::fs::remove_file(path);
        }
        std::fs::remove_file(self.session_path_for(&parsed))?;
        let _ = std::fs::remove_file(lock_path);
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
    /// Meta event is rewritten; everything else is verbatim). Returns the
    /// new session id.
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
        let mut events = source.events()[..cut].to_vec();
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

    /// Rewind a session in place: the `UserMessage` at local event
    /// index `cut` becomes unsent, and everything from it on is folded
    /// out of replay by an appended `Rewind` marker. The audit log
    /// keeps the abandoned tail. `tree_restored` and `tree_saved`
    /// record what happened to the working tree; the git work itself is
    /// the caller's.
    pub fn rewind(
        &self,
        id: &str,
        cut: usize,
        tree_restored: Option<String>,
        tree_saved: Option<String>,
    ) -> std::io::Result<RewindOutcome> {
        let session = self.acquire_writer(id)?.load()?;
        session.rewind_to(cut, tree_restored, tree_saved)
    }

    /// Read a session snapshot. Only newline-committed records are parsed;
    /// committed corruption is rejected and an in-progress tail is ignored.
    pub fn load(&self, id: &str) -> std::io::Result<SessionReader> {
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
) -> std::io::Result<ReplayData> {
    if let Ok(replay) = read_indexed_replay(file, path, replay_index_path, id) {
        return Ok(replay);
    }
    let (full_events, unanswered_calls, observed_stamp) = read_events(file, path, id, repair_tail)?;
    let canonical_event_count = full_events.len();
    let (effective_model, effective_variant, todo_list, topic) = replay_state(&full_events);
    let (events, event_base) = if repair_tail {
        (full_events, 0)
    } else {
        active_replay_window(&full_events)
    };
    Ok(ReplayData {
        canonical_event_count,
        events,
        unanswered_calls,
        event_base,
        effective_model,
        effective_variant,
        todo_list,
        topic,
        checkpoint: None,
        checkpoint_tail_start: 0,
        observed_stamp,
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
        effective_model,
        effective_variant,
        todo_list,
        topic,
        checkpoint: Some(checkpoint),
        checkpoint_tail_start,
        observed_stamp: observed,
    })
}

fn parse_event_bytes(
    bytes: &[u8],
    id: &str,
    line_offset: usize,
) -> std::io::Result<Vec<SessionEvent>> {
    let mut events = Vec::new();
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
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
        events.push(event);
    }
    Ok(events)
}

fn read_events(
    file: &mut File,
    path: &std::path::Path,
    id: &str,
    repair_tail: bool,
) -> std::io::Result<(Vec<SessionEvent>, Vec<String>, FileStamp)> {
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
    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let events = fold_rewinds(parse_event_bytes(&bytes[..complete_len], id, 0)?);
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
    Ok((events, unanswered_calls, final_stamp))
}

fn file_stamp(metadata: &std::fs::Metadata) -> std::io::Result<FileStamp> {
    let modified_nanos = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(FileStamp {
            len: metadata.len(),
            modified_nanos,
            device: metadata.dev(),
            inode: metadata.ino(),
            changed_seconds: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(FileStamp {
            len: metadata.len(),
            modified_nanos,
            device: 0,
            inode: 0,
            changed_seconds: 0,
            changed_nanos: 0,
        })
    }
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
/// view — and disappears itself. `truncate` tolerates an out-of-range
/// `to` from a damaged file by keeping everything.
fn fold_rewinds(events: Vec<SessionEvent>) -> Vec<SessionEvent> {
    if !events
        .iter()
        .any(|event| matches!(event, SessionEvent::Rewind { .. }))
    {
        return events;
    }
    let mut folded = Vec::with_capacity(events.len());
    for event in events {
        match event {
            SessionEvent::Rewind { to, .. } => folded.truncate(to),
            event => folded.push(event),
        }
    }
    folded
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
        if let SessionEvent::Compaction { kept_from, .. } = event {
            *kept_from = kept_from.saturating_sub(base).saturating_add(1);
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

fn id_records(events: &[SessionEvent]) -> Vec<[u8; REPLAY_ID_RECORD_LEN as usize]> {
    let mut records = Vec::new();
    for event in events {
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
            | SessionEvent::Rewind { id, .. } => Some(id.as_str()),
        };
        if let Some(id) = event_id {
            records.push(id_record(0, id));
        }
        if let SessionEvent::AssistantMessage { content, .. } = event {
            records.extend(content.iter().filter_map(|block| match block {
                ContentBlock::ToolCall { id, .. } => Some(id_record(1, id)),
                _ => None,
            }));
        }
    }
    records
}

fn id_record(namespace: u8, id: &str) -> [u8; REPLAY_ID_RECORD_LEN as usize] {
    let mut record = [0; REPLAY_ID_RECORD_LEN as usize];
    record[0] = namespace;
    record[1..].copy_from_slice(&Sha256::digest(id.as_bytes()));
    record
}

struct ReplayIdIndex {
    file: File,
    count: usize,
    level_counts: Vec<usize>,
    level_offsets: Vec<u64>,
    root: [u8; 32],
    verified_pages: HashMap<usize, Vec<[u8; REPLAY_ID_RECORD_LEN as usize]>>,
}

impl ReplayIdIndex {
    fn open(path: &std::path::Path, generation: &str, root: &str) -> std::io::Result<Self> {
        let generation = uuid::Uuid::parse_str(generation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let mut file = File::open(path)?;
        let mut header = [0u8; REPLAY_IDS_HEADER_LEN as usize];
        file.read_exact(&mut header)?;
        if &header[..8] != REPLAY_IDS_MAGIC || header[8..24] != *generation.as_bytes() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "replay id index generation mismatch",
            ));
        }
        let count = usize::try_from(u64::from_le_bytes(header[24..32].try_into().unwrap()))
            .map_err(|_| invalid_data("replay id count does not fit this platform"))?;
        let level_counts = merkle_level_counts(count);
        let records_len = (count as u64)
            .checked_mul(REPLAY_ID_RECORD_LEN)
            .ok_or_else(|| invalid_data("replay id length overflow"))?;
        let mut offset = REPLAY_IDS_HEADER_LEN
            .checked_add(records_len)
            .ok_or_else(|| invalid_data("replay id length overflow"))?;
        let mut level_offsets = Vec::with_capacity(level_counts.len());
        for level_count in &level_counts {
            level_offsets.push(offset);
            offset = offset
                .checked_add(
                    (*level_count as u64)
                        .checked_mul(32)
                        .ok_or_else(|| invalid_data("replay id tree length overflow"))?,
                )
                .ok_or_else(|| invalid_data("replay id tree length overflow"))?;
        }
        if file.metadata()?.len() != offset {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid replay id index length",
            ));
        }
        Ok(Self {
            file,
            count,
            level_counts,
            level_offsets,
            root: parse_digest(root)?,
            verified_pages: HashMap::new(),
        })
    }

    fn contains(&mut self, target: &[u8; REPLAY_ID_RECORD_LEN as usize]) -> std::io::Result<bool> {
        let mut low = 0;
        let mut high = self.count;
        while low < high {
            let middle = low + (high - low) / 2;
            match self.record_at(middle)?.cmp(target) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(true),
            }
        }
        Ok(false)
    }

    fn record_at(&mut self, index: usize) -> std::io::Result<[u8; REPLAY_ID_RECORD_LEN as usize]> {
        let page = index / REPLAY_ID_PAGE_RECORDS;
        let within = index % REPLAY_ID_PAGE_RECORDS;
        Ok(self.read_page(page)?[within])
    }

    fn read_page(
        &mut self,
        page: usize,
    ) -> std::io::Result<Vec<[u8; REPLAY_ID_RECORD_LEN as usize]>> {
        if let Some(records) = self.verified_pages.get(&page) {
            return Ok(records.clone());
        }
        let first_record = page.saturating_mul(REPLAY_ID_PAGE_RECORDS);
        let count = (self.count - first_record).min(REPLAY_ID_PAGE_RECORDS);
        let mut bytes = vec![0; count * REPLAY_ID_RECORD_LEN as usize];
        self.file.seek(std::io::SeekFrom::Start(
            REPLAY_IDS_HEADER_LEN + first_record as u64 * REPLAY_ID_RECORD_LEN,
        ))?;
        self.file.read_exact(&mut bytes)?;
        let mut hash = digest(&bytes);
        let mut node = page;
        for level in 0..self.level_counts.len().saturating_sub(1) {
            let sibling = if node.is_multiple_of(2) {
                node + 1
            } else {
                node - 1
            };
            let sibling_hash = if sibling < self.level_counts[level] {
                self.read_tree_hash(level, sibling)?
            } else {
                hash
            };
            hash = if node.is_multiple_of(2) {
                digest_pair(&hash, &sibling_hash)
            } else {
                digest_pair(&sibling_hash, &hash)
            };
            node /= 2;
        }
        if hash != self.root {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "replay id Merkle proof mismatch",
            ));
        }
        let records = bytes
            .chunks_exact(REPLAY_ID_RECORD_LEN as usize)
            .map(|bytes| bytes.try_into().map_err(std::io::Error::other))
            .collect::<std::io::Result<Vec<_>>>()?;
        self.verified_pages.insert(page, records.clone());
        Ok(records)
    }

    fn read_tree_hash(&mut self, level: usize, node: usize) -> std::io::Result<[u8; 32]> {
        let offset = self.level_offsets[level]
            .checked_add(
                (node as u64)
                    .checked_mul(32)
                    .ok_or_else(|| invalid_data("replay id tree offset overflow"))?,
            )
            .ok_or_else(|| invalid_data("replay id tree offset overflow"))?;
        self.file.seek(std::io::SeekFrom::Start(offset))?;
        let mut hash = [0; 32];
        self.file.read_exact(&mut hash)?;
        Ok(hash)
    }
}

fn read_all_id_records(
    path: &std::path::Path,
    generation: &str,
    root: &str,
) -> std::io::Result<Vec<[u8; REPLAY_ID_RECORD_LEN as usize]>> {
    let mut index = ReplayIdIndex::open(path, generation, root)?;
    let mut records = Vec::with_capacity(index.count);
    for page in 0..index.level_counts.first().copied().unwrap_or(0) {
        records.extend(index.read_page(page)?);
    }
    Ok(records)
}

fn write_id_records(
    path: &std::path::Path,
    generation: &str,
    records: &[[u8; REPLAY_ID_RECORD_LEN as usize]],
) -> std::io::Result<String> {
    let generation = uuid::Uuid::parse_str(generation)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut bytes = Vec::with_capacity(
        REPLAY_IDS_HEADER_LEN as usize + records.len() * REPLAY_ID_RECORD_LEN as usize,
    );
    bytes.extend_from_slice(REPLAY_IDS_MAGIC);
    bytes.extend_from_slice(generation.as_bytes());
    bytes.extend_from_slice(&(records.len() as u64).to_le_bytes());
    for record in records {
        bytes.extend_from_slice(record);
    }
    let mut levels = vec![
        records
            .chunks(REPLAY_ID_PAGE_RECORDS)
            .map(|page| {
                digest(
                    &page
                        .iter()
                        .flat_map(|record| record.iter().copied())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>(),
    ];
    while levels.last().is_some_and(|level| level.len() > 1) {
        let previous = levels.last().unwrap();
        levels.push(
            previous
                .chunks(2)
                .map(|pair| digest_pair(&pair[0], pair.get(1).unwrap_or(&pair[0])))
                .collect(),
        );
    }
    for level in &levels {
        for hash in level {
            bytes.extend_from_slice(hash);
        }
    }
    let root = levels
        .last()
        .and_then(|level| level.first())
        .copied()
        .unwrap_or_else(|| digest(&[]));
    crate::atomic_file::replace(path, &bytes, crate::atomic_file::Mode::Force(0o600))?;
    Ok(digest_to_hex(&root))
}

fn replay_ids_path(replay_index_path: &std::path::Path, id: &str, generation: &str) -> PathBuf {
    replay_index_path.with_file_name(format!("{id}.replay.{generation}.ids"))
}

fn merkle_level_counts(record_count: usize) -> Vec<usize> {
    let mut count = record_count.div_ceil(REPLAY_ID_PAGE_RECORDS);
    let mut levels = Vec::new();
    while count > 0 {
        levels.push(count);
        if count == 1 {
            break;
        }
        count = count.div_ceil(2);
    }
    levels
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn digest_pair(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut bytes = [0; 64];
    bytes[..32].copy_from_slice(left);
    bytes[32..].copy_from_slice(right);
    digest(&bytes)
}

fn digest_to_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest_hex(bytes: &[u8]) -> String {
    digest_to_hex(&digest(bytes))
}

fn parse_digest(value: &str) -> std::io::Result<[u8; 32]> {
    if value.len() != 64 {
        return Err(invalid_data("invalid replay digest length"));
    }
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| invalid_data("invalid replay digest"))?;
    }
    Ok(digest)
}

fn invalid_data(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn checkpoint_checksum(checkpoint: &ReplayCheckpoint) -> std::io::Result<String> {
    let payload = serde_json::to_vec(&(
        checkpoint.version,
        &checkpoint.generation,
        &checkpoint.session_id,
        checkpoint.replay_offset,
        checkpoint.canonical_event_count,
        checkpoint.physical_line_count,
        checkpoint.active_start,
        &checkpoint.events,
        &checkpoint.effective_model,
        &checkpoint.effective_variant,
        &checkpoint.todo_list,
        &checkpoint.topic,
        &checkpoint.id_root,
        &checkpoint.observed,
    ))
    .map_err(std::io::Error::other)?;
    Ok(digest_hex(&payload))
}

fn write_checkpoint(path: &std::path::Path, checkpoint: &ReplayCheckpoint) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(checkpoint).map_err(std::io::Error::other)?;
    crate::atomic_file::replace(path, &bytes, crate::atomic_file::Mode::Force(0o600))
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
            | SessionEvent::Rewind { id, .. } => Some(id),
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
                "rewind markers are appended through SessionStore::rewind",
            ));
        }
        self.append_event(event)
    }

    fn append_event(&mut self, event: SessionEvent) -> std::io::Result<()> {
        let next_canonical_event_count = self
            .canonical_event_count
            .checked_add(1)
            .ok_or_else(|| invalid_data("canonical event count overflow"))?;
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
            physical_line_count: self.canonical_event_count,
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

    let mut pending_results: Vec<ContentBlock> = Vec::new();
    for event in &events[cut..] {
        match event {
            SessionEvent::Meta { .. }
            | SessionEvent::SubagentInvocation { .. }
            | SessionEvent::Checkpoint { .. }
            | SessionEvent::Topic { .. }
            | SessionEvent::Rewind { .. } => {}
            SessionEvent::UserMessage { text, images, .. } => {
                if !pending_results.is_empty() {
                    push_user_blocks(&mut messages, std::mem::take(&mut pending_results));
                }
                let mut blocks = vec![ContentBlock::Text { text: text.clone() }];
                blocks.extend(images.iter().map(|image| ContentBlock::Image {
                    image: image.clone(),
                }));
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
                ..
            } => {
                pending_results.push(ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
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

fn compaction_cut(events: &[SessionEvent]) -> usize {
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
