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
    /// The directory the session was launched from, canonicalized.
    /// Purely informational — unlike `workspace` it routes nothing and
    /// binds nothing; the resume surfaces read it to lead with the
    /// sessions belonging to the directory you are in. Absent in logs
    /// written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
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
        /// Inline attachments pasted with the message; absent in
        /// sessions from before images existed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<crate::session::ImageContent>,
        ts: DateTime<Utc>,
    },
    /// Associates one child-session turn with the parent task call that invoked it.
    SubagentInvocation {
        id: String,
        parent_tool_call_id: String,
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
        /// Images the tool returned with its text; absent in sessions
        /// from before tool results could carry them, and never written
        /// for a text-only result.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<crate::session::ImageContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<SessionState>,
        ts: DateTime<Utc>,
    },
    /// Shadow git snapshot of the working tree, taken as a user turn
    /// starts. Renders nothing; rewind uses it to restore the tree.
    Checkpoint {
        id: String,
        /// The shadow commit under `refs/ilar/checkpoints/<session-id>`.
        commit: String,
        /// Repository HEAD at capture time; absent on an unborn branch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head: Option<String>,
        ts: DateTime<Utc>,
    },
    /// Runtime model switch; effective from the next provider call.
    ModelChange {
        id: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        variant: Option<String>,
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
    /// A few words naming what this session is about. Session state,
    /// never conversation: it identifies the session in listings and
    /// never reaches the model.
    Topic {
        id: String,
        text: String,
        ts: DateTime<Utc>,
    },
    /// Images in events before canonical index `before` no longer
    /// travel to the provider: the text that named them stays, a note
    /// stands where each picture was. Written by the turn loop when
    /// the pictures in a request outgrow their budget, so the rewrite
    /// happens once and the cached prefix then holds. The images stay
    /// in the log for the transcript view.
    ImageCutoff {
        id: String,
        before: usize,
        ts: DateTime<Utc>,
    },
    /// Rewind boundary: replay behaves as if the log ended just before
    /// canonical event `to` (a `UserMessage`, which becomes unsent).
    /// The log stays append-only — the discarded tail and this marker
    /// remain visible to `audit_events`, like `Compaction` in reverse.
    Rewind {
        id: String,
        /// Canonical event index the session was cut back to.
        to: usize,
        /// Checkpoint commit the working tree was restored to, when the
        /// target turn had a tree snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tree_restored: Option<String>,
        /// Safety snapshot of the tree taken just before restoring.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tree_saved: Option<String>,
        ts: DateTime<Utc>,
    },
    /// A turn that did not finish, and how. Written for a task's turn
    /// by the spawner: without it a child killed by its parent's abort
    /// has a log that simply stops, and nothing can tell it from one
    /// that answered. Session state, never conversation — it does not
    /// reach the model.
    TurnEnded {
        id: String,
        ending: TurnEnding,
        /// The one-sentence headline the parent was told.
        detail: String,
        ts: DateTime<Utc>,
    },
}

/// The verb set for a turn that did not finish — the same one the task
/// notifications use, so a listing and a notification agree on what
/// happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnding {
    /// The turn gave up on its own token: its caller's turn ended.
    Aborted,
    /// A stop someone asked for.
    Cancelled,
    /// Anything the turn did to itself, its iteration limit included.
    Failed,
    /// The stall watchdog stopped it.
    Stalled,
}

impl TurnEnding {
    /// The word a listing uses for it.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Stalled => "stalled",
        }
    }
}

impl SessionEvent {
    pub fn ts(&self) -> DateTime<Utc> {
        match self {
            SessionEvent::Meta { ts, .. }
            | SessionEvent::UserMessage { ts, .. }
            | SessionEvent::SubagentInvocation { ts, .. }
            | SessionEvent::AssistantMessage { ts, .. }
            | SessionEvent::ToolResult { ts, .. }
            | SessionEvent::Checkpoint { ts, .. }
            | SessionEvent::ModelChange { ts, .. }
            | SessionEvent::Compaction { ts, .. }
            | SessionEvent::Topic { ts, .. }
            | SessionEvent::ImageCutoff { ts, .. }
            | SessionEvent::Rewind { ts, .. }
            | SessionEvent::TurnEnded { ts, .. } => *ts,
        }
    }
}

/// The `type` tag of a well-formed event object this build cannot name —
/// the signature of a log written by a newer ilar. `None` for everything
/// else, so a known event with broken fields stays plain corruption.
pub(crate) fn unknown_event_type(line: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let tag = value.get("type")?.as_str()?;
    (!is_known_event_type(tag)).then(|| tag.to_string())
}

/// Asks serde's own variant table instead of duplicating it here: a bare
/// tag never deserializes into a whole event, but only an unrecognized
/// one fails as an unknown variant.
fn is_known_event_type(tag: &str) -> bool {
    match serde_json::from_value::<SessionEvent>(serde_json::json!({ "type": tag })) {
        Ok(_) => true,
        Err(error) => !error.to_string().starts_with("unknown variant"),
    }
}

pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}
