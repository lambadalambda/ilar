//! /btw: a quick question over the session, leaving no trace in it.
//!
//! The model answers with the whole conversation in front of it, but
//! neither the question nor the answer is appended to the log — an
//! aside must not steer the ongoing work.
//!
//! Same request shape as compaction, for the same two reasons: the
//! question goes *last* so the model answers it instead of the
//! conversation, and everything before it stays byte-identical to the
//! turn's own request, so the provider serves the conversation from
//! its prompt cache and the aside pays for the question alone.

use anyhow::Result;
use futures::StreamExt;

use crate::provider::{ProviderEvent, ProviderResolver, Request, StopReason, ToolDefinition};
use crate::session::{ChatMessage, ContentBlock, Role, SessionStore};
use tokio_util::sync::CancellationToken;

/// Framing that keeps an aside an aside: no tools, no task-continuing.
const ASIDE_PREAMBLE: &str = "This is a quick aside, not part of the task. Answer the question \
below from the conversation so far. Do not call any tool, do not continue or change the work, \
and keep the answer brief. Neither the question nor your answer will be recorded in the \
session.\n\nQuestion: ";

/// The call ids a message makes.
fn tool_call_ids(message: &ChatMessage) -> impl Iterator<Item = &str> {
    message.content.iter().filter_map(|block| match block {
        ContentBlock::ToolCall { id, .. } => Some(id.as_str()),
        _ => None,
    })
}

/// The call ids a message answers.
fn tool_result_ids(message: &ChatMessage) -> impl Iterator<Item = &str> {
    message.content.iter().filter_map(|block| match block {
        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
        _ => None,
    })
}

/// Is the transcript's last message a loose end — something a provider
/// would reject the request for?
///
/// Two shapes end a request mid-step. The plain one is an assistant
/// message whose tool calls have no results yet. The subtler one is a
/// *user* message carrying only some of that step's results: the turn
/// appends results one at a time, and the store flushes whatever is
/// pending as a trailing user message, so a snapshot taken between two
/// appends answers M of N calls. Either way the request would carry
/// unanswered calls.
fn unsettled_tail(messages: &[ChatMessage]) -> bool {
    let Some(last) = messages.last() else {
        return false;
    };
    match last.role {
        Role::Assistant => tool_call_ids(last).next().is_some(),
        Role::User => {
            let answered: Vec<&str> = tool_result_ids(last).collect();
            // A user message with no results at all is a question, not a
            // half-answered step.
            !answered.is_empty()
                && messages.iter().rev().nth(1).is_some_and(|step| {
                    step.role == Role::Assistant
                        && tool_call_ids(step).next().is_some()
                        && !tool_call_ids(step).all(|id| answered.contains(&id))
                })
        }
    }
}

/// Cut the transcript back to its last settled point. Tool-call /
/// result pairs are adjacent, so dropping trailing loose ends — a
/// half-answered step's partial results, then the step they left
/// unanswered — is the whole job. Repeats until the transcript ends on
/// a settled boundary, keeping every settled step before it: an aside
/// wants as much context as it can legally send.
fn settled(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    while unsettled_tail(&messages) {
        messages.pop();
    }
    messages
}

/// Answer `question` over the session's live transcript. Returns
/// `Ok(None)` when cancelled. Nothing is written anywhere.
pub async fn ask(
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    system_prompt: Option<&str>,
    tools: &[ToolDefinition],
    question: &str,
    cancel: &CancellationToken,
) -> Result<Option<String>> {
    let question = question.trim();
    if question.is_empty() {
        anyhow::bail!("an aside needs a question");
    }
    if cancel.is_cancelled() {
        return Ok(None);
    }
    // Read-only: an aside takes no writer lease, so it can never block
    // (or be corrupted by) anything that owns the session — which is
    // what lets it run beside a live turn.
    let reader = store.load(session_id)?;
    let model = reader.effective_model();
    let mut messages = settled(reader.transcript());
    messages.push(ChatMessage::user_text(format!(
        "{ASIDE_PREAMBLE}{question}"
    )));
    let request = Request {
        model: model.clone(),
        system_prompt: system_prompt.map(str::to_string),
        messages,
        // The turn's own tools, unused but present: dropping them would
        // change the request prefix and forfeit the cache.
        tools: tools.to_vec(),
        cache_key: Some(session_id.to_string()),
        options: crate::model::variant_options(&model, reader.effective_variant().as_deref())?,
    };
    let provider = resolver.resolve_provider(&model)?;
    let mut stream = provider.as_provider().stream(request)?;
    let mut answer = String::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(None),
            next = stream.next() => next,
        };
        let Some(event) = next else {
            anyhow::bail!("aside stream ended before completion");
        };
        match event {
            ProviderEvent::TextDelta(text) => answer.push_str(&text),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                ..
            } => break,
            ProviderEvent::TurnComplete { stop_reason, .. } => {
                anyhow::bail!(
                    "the aside tried to {stop_reason:?} instead of answering (tool use is disabled here)"
                )
            }
            ProviderEvent::Error(error) | ProviderEvent::RetryableError(error) => {
                anyhow::bail!("aside call failed: {error}")
            }
            _ => {}
        }
    }
    Ok(Some(answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_calls(ids: &[&str]) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: ids
                .iter()
                .map(|id| ContentBlock::ToolCall {
                    id: (*id).into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "ilar.toml"}),
                    item_id: None,
                })
                .collect(),
        }
    }

    fn results(ids: &[&str]) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: ids
                .iter()
                .map(|id| ContentBlock::ToolResult {
                    tool_use_id: (*id).into(),
                    content: "ok".into(),
                    is_error: false,
                    images: Vec::new(),
                })
                .collect(),
        }
    }

    fn assistant_text(text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// The invariant a provider enforces: every call in the request is
    /// answered somewhere in it.
    fn unanswered_calls(messages: &[ChatMessage]) -> Vec<String> {
        let answered: Vec<&str> = messages.iter().flat_map(tool_result_ids).collect();
        messages
            .iter()
            .flat_map(tool_call_ids)
            .filter(|id| !answered.contains(id))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn a_half_answered_step_is_dropped_results_and_all() {
        // What a snapshot mid-step looks like: the results land one at a
        // time, so the transcript ends on a *user* message that answers
        // only some of the calls above it.
        let transcript = vec![
            ChatMessage::user_text("check the config"),
            assistant_text("on it"),
            assistant_calls(&["call-1", "call-2"]),
            results(&["call-1"]),
        ];

        let messages = settled(transcript.clone());

        assert!(
            unanswered_calls(&messages).is_empty(),
            "unpaired tool call survived: {messages:?}"
        );
        assert_eq!(messages, transcript[..2]);
    }

    #[test]
    fn a_fully_answered_step_rides_along() {
        // Asides want maximal settled context: a finished step is context,
        // not a loose end.
        let transcript = vec![
            ChatMessage::user_text("check the config"),
            assistant_calls(&["call-1", "call-2"]),
            results(&["call-2", "call-1"]),
        ];

        assert_eq!(settled(transcript.clone()), transcript);
    }

    #[test]
    fn cutting_back_repeats_until_the_transcript_settles() {
        let transcript = vec![
            ChatMessage::user_text("check the config"),
            assistant_calls(&["call-1"]),
            results(&["call-1"]),
            assistant_calls(&["call-2", "call-3"]),
            results(&["call-2"]),
            assistant_calls(&["call-4"]),
        ];

        let messages = settled(transcript.clone());

        assert!(
            unanswered_calls(&messages).is_empty(),
            "unpaired tool call survived: {messages:?}"
        );
        assert_eq!(messages, transcript[..3]);
    }
}
