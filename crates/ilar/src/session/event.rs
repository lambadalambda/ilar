//! Session events: the append-only log line format.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::model::{ContentBlock, Usage};

/// Session metadata; always the first event in a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    /// Parent session, for subagent child sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Agent name (e.g. "build" or a custom markdown agent).
    pub agent: String,
    /// Model the session started with ("provider/model-id").
    pub model: String,
    /// Workspace routing metadata for child sessions. Older/root sessions use
    /// the runtime workspace when this is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::tools::WorkspaceLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionState {
    TodoList { list: crate::todo::TodoList },
}

impl SessionState {
    pub fn todo_list(&self) -> &crate::todo::TodoList {
        match self {
            SessionState::TodoList { list } => list,
        }
    }
}

/// One JSONL line. Self-describing via the `type` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    Meta {
        #[serde(flatten)]
        meta: SessionMeta,
        ts: DateTime<Utc>,
    },
    UserMessage {
        id: String,
        text: String,
        ts: DateTime<Utc>,
    },
    /// A completed assistant turn: text/thinking blocks and tool calls.
    /// Streaming deltas are loop-internal; only completed turns persist.
    AssistantMessage {
        id: String,
        model: String,
        content: Vec<ContentBlock>,
        usage: Usage,
        stop_reason: String,
        ts: DateTime<Utc>,
    },
    ToolResult {
        id: String,
        tool_use_id: String,
        content: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<SessionState>,
        ts: DateTime<Utc>,
    },
    /// Runtime model switch; effective from the next provider call.
    ModelChange {
        id: String,
        model: String,
        ts: DateTime<Utc>,
    },
    /// Compaction boundary: everything before `kept_from` (exclusive) is
    /// replaced by `summary` in the rendered transcript.
    ///
    /// Caveat: `kept_from` indexes the event vector at write time. If a
    /// session file loses lines to corruption, replay shifts indices and
    /// the boundary may keep fewer events than intended. The transcript
    /// stays coherent (summary + tail); a future compaction writer can
    /// anchor to event ids if this ever matters.
    Compaction {
        id: String,
        summary: String,
        /// Event index the compaction kept from.
        kept_from: usize,
        ts: DateTime<Utc>,
    },
}

impl SessionEvent {
    pub fn ts(&self) -> DateTime<Utc> {
        match self {
            SessionEvent::Meta { ts, .. }
            | SessionEvent::UserMessage { ts, .. }
            | SessionEvent::AssistantMessage { ts, .. }
            | SessionEvent::ToolResult { ts, .. }
            | SessionEvent::ModelChange { ts, .. }
            | SessionEvent::Compaction { ts, .. } => *ts,
        }
    }
}

pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}
