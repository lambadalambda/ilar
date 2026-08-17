//! Append-only JSONL session store.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use fs2::FileExt;

use super::event::{SessionEvent, SessionMeta, new_id};
use super::model::{ChatMessage, ContentBlock, Role};

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
}

/// Exclusive OS-backed writer ownership for one session. The lock file is
/// persistent, but the lock itself is released by the OS on drop or crash.
pub struct SessionWriter {
    _file: File,
    id: SessionId,
    session_path: PathBuf,
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._file);
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

    pub fn acquire_writer(&self, id: &str) -> std::io::Result<SessionWriter> {
        self.acquire_writer_id(SessionId::parse(id)?)
    }

    fn session_path_for(&self, id: &SessionId) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
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
        let mut session = Session {
            events: Vec::new(),
            file,
            _writer: writer,
        };
        session.append(SessionEvent::Meta {
            meta,
            ts: chrono::Utc::now(),
        })?;
        Ok(session)
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
        let (events, _) = read_events(&path, id.as_str(), false)?;
        Ok(SessionReader { events })
    }
}

impl SessionWriter {
    pub fn load(self) -> std::io::Result<Session> {
        let (events, unanswered_calls) = read_events(&self.session_path, self.id.as_str(), true)?;
        let file = OpenOptions::new().append(true).open(&self.session_path)?;
        let mut session = Session {
            events,
            file,
            _writer: self,
        };
        for tool_use_id in unanswered_calls {
            session.append(SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id,
                content: "Tool call interrupted before completion.".into(),
                is_error: true,
                state: None,
                ts: chrono::Utc::now(),
            })?;
        }
        Ok(session)
    }
}

fn read_events(
    path: &std::path::Path,
    id: &str,
    repair_tail: bool,
) -> std::io::Result<(Vec<SessionEvent>, Vec<String>)> {
    let bytes = std::fs::read(path)?;
    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let mut events = Vec::new();
    for (n, line) in bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let line = std::str::from_utf8(line).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session {id}: invalid UTF-8 on line {}: {error}", n + 1),
            )
        })?;
        match serde_json::from_str::<SessionEvent>(line) {
            Ok(event) => events.push(event),
            Err(error) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("session {id}: malformed line {}: {error}", n + 1),
                ));
            }
        }
    }
    if events.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("session unrecoverable (no committed events): {id}"),
        ));
    }
    let unanswered_calls = validate_replay(&events, id)?;
    // Mutation happens only after every committed record validates.
    if repair_tail && complete_len < bytes.len() {
        OpenOptions::new()
            .write(true)
            .open(path)?
            .set_len(complete_len as u64)?;
    }
    Ok((events, unanswered_calls))
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
            | SessionEvent::AssistantMessage { id, .. }
            | SessionEvent::ToolResult { id, .. }
            | SessionEvent::ModelChange { id, .. }
            | SessionEvent::Compaction { id, .. } => Some(id),
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
            SessionEvent::Meta { .. } => {}
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

fn invalid_replay<T>(id: &str, message: impl std::fmt::Display) -> std::io::Result<T> {
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("session {id}: {message}"),
    ))
}

impl Session {
    /// All events, in log order.
    pub fn events(&self) -> &[SessionEvent] {
        &self.events
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
        self.events
            .iter()
            .rev()
            .find_map(|e| match e {
                SessionEvent::ModelChange { model, .. } => Some(model.clone()),
                _ => None,
            })
            .or_else(|| self.meta().map(|m| m.model.clone()))
            .unwrap_or_default()
    }

    /// Session id (empty string only in a pathological no-meta session).
    pub fn session_id(&self) -> &str {
        self.meta()
            .map(|m| m.session_id.as_str())
            .unwrap_or_default()
    }

    /// Append an event: persists one JSONL line, then updates the model.
    pub fn append(&mut self, event: SessionEvent) -> std::io::Result<()> {
        let mut line = serde_json::to_string(&event).map_err(std::io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.events.push(event);
        Ok(())
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
        todo_list_of(&self.events)
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
        SessionReader {
            events: events[..cut.min(events.len())].to_vec(),
        }
    }
}

/// Read-only session view (compaction input).
pub struct SessionReader {
    events: Vec<SessionEvent>,
}

impl SessionReader {
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
        self.events
            .iter()
            .rev()
            .find_map(|event| match event {
                SessionEvent::ModelChange { model, .. } => Some(model.clone()),
                _ => None,
            })
            .or_else(|| self.meta().map(|meta| meta.model.clone()))
            .unwrap_or_default()
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
        todo_list_of(&self.events)
    }
}

fn todo_list_of(events: &[SessionEvent]) -> Option<&crate::todo::TodoList> {
    events.iter().rev().find_map(|event| match event {
        SessionEvent::ToolResult {
            state: Some(crate::session::SessionState::TodoList { list }),
            ..
        } => Some(list),
        _ => None,
    })
}

/// Pure transcript rendering over an event slice.
fn transcript_of(events: &[SessionEvent]) -> Vec<ChatMessage> {
    let mut cut = 0usize;
    let mut summary: Option<&str> = None;
    for (i, event) in events.iter().enumerate() {
        if let SessionEvent::Compaction {
            kept_from,
            summary: s,
            ..
        } = event
        {
            cut = (*kept_from).min(i).max(cut);
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
                text: format!("<compaction-summary>\n{summary}\n</compaction-summary>"),
            }],
        });
    }

    let mut pending_results: Vec<ContentBlock> = Vec::new();
    for event in &events[cut..] {
        match event {
            SessionEvent::Meta { .. } => {}
            SessionEvent::UserMessage { text, .. } => {
                if !pending_results.is_empty() {
                    push_user_blocks(&mut messages, std::mem::take(&mut pending_results));
                }
                push_user_blocks(
                    &mut messages,
                    vec![ContentBlock::Text { text: text.clone() }],
                );
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
