//! Append-only JSONL session store.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::PathBuf;

use fs2::FileExt;

use super::event::{SessionEvent, SessionMeta, new_id, unknown_event_type};
use super::model::{ChatMessage, ContentBlock, Role};
use super::replay_index::{
    FileStamp, REPLAY_INDEX_VERSION, ReplayCheckpoint, ReplayIdIndex, checkpoint_checksum,
    committed_line_count, file_stamp, id_record, id_records, invalid_data, read_all_id_records,
    replay_ids_path, write_checkpoint, write_id_records,
};
use crate::question::{QUESTION_TOOL_NAME, QuestionRequest, validate_request};
use crate::text::truncate_chars_ellipsis;

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

/// Exclusive OS-backed writer ownership for one session. The lock file is
/// persistent, but the lock itself is released by the OS on drop or crash.
pub struct SessionWriter {
    _file: File,
    id: SessionId,
    session_path: PathBuf,
    replay_index_path: PathBuf,
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

/// A directory entry's head, or `None` for anything that is not a
/// readable session file — the listing skips those by construction.
fn scan_head(entry: &std::fs::DirEntry) -> Option<SessionHead> {
    let name = entry.file_name();
    let id = name.to_str()?.strip_suffix(".jsonl")?.to_string();
    SessionId::parse(&id).ok()?;
    let modified = entry.metadata().ok()?.modified().ok()?;
    read_head(&entry.path(), id, modified)
}

fn summarize_entry(entry: &std::fs::DirEntry) -> Option<SessionSummary> {
    let head = scan_head(entry)?;
    if head.meta.parent_id.is_some() {
        return None;
    }
    Some(SessionSummary {
        id: head.id,
        title: head.title,
        modified: head.modified,
        cwd: head.meta.cwd,
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
    let title = topic.or(opening);
    Some(SessionHead {
        id,
        meta,
        title,
        modified,
    })
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
        parse_event_bytes(&bytes[..committed_len(&bytes)], id.as_str(), 0)
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
            physical_line_count: 0,
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
            .filter_map(|entry| summarize_entry(&entry))
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
            .filter_map(|entry| scan_head(&entry))
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
) -> std::io::Result<ReplayData> {
    if let Ok(replay) = read_indexed_replay(file, path, replay_index_path, id) {
        return Ok(replay);
    }
    let canonical = read_events(file, path, id, repair_tail)?;
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
    let events = fold_rewinds(parse_event_bytes(committed, id, 0)?);
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
/// view — and disappears itself. `truncate` tolerates an out-of-range
/// `to` from a damaged file by keeping everything.
pub(super) fn fold_rewinds(events: Vec<SessionEvent>) -> Vec<SessionEvent> {
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
                images,
                ..
            } => {
                pending_results.push(ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                    images: images.clone(),
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

#[cfg(test)]
mod tests {
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
