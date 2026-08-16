//! Neutral provider events, streamed during a turn.

use serde::{Deserialize, Serialize};

use crate::session::Usage;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    /// The model declined (content refusal / filter).
    Refusal,
    /// Server-side pause (Anthropic `pause_turn`): re-issue the request.
    Paused,
    Stopped,
}

/// Streamed events from a provider during one API call.
///
/// Ordering contract:
/// - text: any number of `TextDelta`s; a new text run begins after any
///   other event kind (loop segments on non-text boundaries — this is how
///   text → tool call → more text round-trips without block ids)
/// - thinking: `ThinkingDelta`s, then one `ThinkingCompleted` carrying the
///   provider signature (if any)
/// - tool calls: `ToolCallStarted`, zero+ `ToolCallInputDelta`, exactly one
///   `ToolCallCompleted`
/// - `TurnComplete` is always the last event of a successful call;
///   `Error` terminates the stream
///
/// Convention for malformed tool-call arguments (e.g. truncated on
/// max_output_tokens): emit `ToolCallCompleted` with `input: Value::Null`
/// and `StopReason::MaxTokens` on the turn. The loop treats null-input
/// calls as failed tools.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    /// A chunk of assistant text.
    TextDelta(String),
    /// A chunk of extended thinking (providers may forward none).
    ThinkingDelta(String),
    /// A thinking block completed; signature if the provider signs.
    ThinkingCompleted { signature: Option<String> },
    /// Opaque provider reasoning item, preserved exactly for replay.
    ReasoningItem { item: serde_json::Value },
    /// A tool call was announced (id + name known, args not yet).
    ToolCallStarted { id: String, name: String },
    /// Incremental tool-call argument JSON.
    ToolCallInputDelta { id: String, delta: String },
    /// Tool call finished; `input` is the parsed JSON value.
    ToolCallCompleted {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Exact provider assistant content for replaying a continued response.
    /// Emitted immediately before the terminal event when available.
    ResponseContent {
        provider: String,
        content: serde_json::Value,
    },
    /// The API call finished. Always terminal (on success).
    TurnComplete {
        stop_reason: StopReason,
        usage: Usage,
    },
    /// Fatal provider error; stream terminates after this. String message
    /// keeps events Clone/PartialEq for tests; impls stringify typed errors.
    Error(String),
}

impl ProviderEvent {
    /// Text accumulated from all deltas (test/debug helper).
    pub fn text_of(events: &[ProviderEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Stop reason if this is a `TurnComplete` (test/assert helper).
    pub fn stop_reason(&self) -> Option<StopReason> {
        match self {
            ProviderEvent::TurnComplete { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        }
    }
}
