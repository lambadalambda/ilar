//! Append-only JSONL session store.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use super::event::{SessionEvent, SessionMeta};
use super::model::{ChatMessage, ContentBlock, Role};

/// Owns the session directory; creates/loads sessions.
#[derive(Clone)]
pub struct SessionStore {
    root: PathBuf,
}

/// In-memory replay of one session's event log, plus the append handle.
pub struct Session {
    events: Vec<SessionEvent>,
    file: File,
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn session_path(&self, id: &str) -> std::io::Result<PathBuf> {
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
        Ok(self.root.join(format!("{id}.jsonl")))
    }

    /// Create a new session; writes the Meta event as the first line.
    pub fn create(&self, meta: SessionMeta) -> std::io::Result<Session> {
        let path = self.session_path(&meta.session_id)?;
        std::fs::create_dir_all(&self.root)?;
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)?;
        let mut session = Session {
            events: Vec::new(),
            file,
        };
        session.append(SessionEvent::Meta {
            meta,
            ts: chrono::Utc::now(),
        })?;
        Ok(session)
    }

    /// Load and replay a session file. Malformed lines are skipped with a
    /// warning (torn trailing writes must not destroy a session).
    pub fn load(&self, id: &str) -> std::io::Result<Session> {
        let path = self.session_path(id)?;
        if !path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("session not found: {id}"),
            ));
        }
        let file = OpenOptions::new().append(true).open(&path)?;
        let reader = BufReader::new(File::open(&path)?);
        let mut events = Vec::new();
        for (n, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionEvent>(&line) {
                Ok(event) => events.push(event),
                Err(e) => eprintln!("ilar: session {id}: skipping malformed line {}: {e}", n + 1),
            }
        }
        if events.is_empty() {
            return Err(std::io::Error::other(format!(
                "session unrecoverable (no valid events): {id}"
            )));
        }
        Ok(Session { events, file })
    }
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
    pub fn transcript(&self) -> Vec<ChatMessage> {
        transcript_of(&self.events)
    }
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
