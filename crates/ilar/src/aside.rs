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
use crate::session::{ChatMessage, SessionStore};
use tokio_util::sync::CancellationToken;

/// Framing that keeps an aside an aside: no tools, no task-continuing.
const ASIDE_PREAMBLE: &str = "This is a quick aside, not part of the task. Answer the question \
below from the conversation so far. Do not call any tool, do not continue or change the work, \
and keep the answer brief. Neither the question nor your answer will be recorded in the \
session.\n\nQuestion: ";

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
    // (or be corrupted by) anything that owns the session.
    let reader = store.load(session_id)?;
    let model = reader.effective_model();
    let mut messages = reader.transcript();
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
        continuations: Vec::new(),
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
