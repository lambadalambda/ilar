//! Append-only JSONL session store.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use super::event::{SessionEvent, SessionMeta};
use super::model::{ChatMessage, ContentBlock, Role};

/// Owns the session directory; creates/loads sessions.
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

    pub fn session_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    /// Create a new session; writes the Meta event as the first line.
    pub fn create(&self, meta: SessionMeta) -> std::io::Result<Session> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.session_path(&meta.session_id);
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
        let path = self.session_path(id);
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
        let mut cut = 0usize;
        let mut summary: Option<&str> = None;
        for (i, event) in self.events.iter().enumerate() {
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
        // A cut landing mid-turn must not orphan tool results whose tool
        // calls were compacted away — advance past any leading results.
        while cut < self.events.len() && matches!(self.events[cut], SessionEvent::ToolResult { .. })
        {
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
        let flush_results = |messages: &mut Vec<ChatMessage>, pending: &mut Vec<ContentBlock>| {
            if !pending.is_empty() {
                push_user_blocks(messages, std::mem::take(pending));
            }
        };

        for event in &self.events[cut..] {
            match event {
                SessionEvent::Meta { .. } => {}
                SessionEvent::UserMessage { text, .. } => {
                    flush_results(&mut messages, &mut pending_results);
                    push_user_blocks(
                        &mut messages,
                        vec![ContentBlock::Text { text: text.clone() }],
                    );
                }
                SessionEvent::AssistantMessage { content, .. } => {
                    flush_results(&mut messages, &mut pending_results);
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
                SessionEvent::Compaction { .. } => {}
            }
        }
        flush_results(&mut messages, &mut pending_results);
        messages
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
