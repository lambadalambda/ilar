//! Auto-compaction — see meta/issues/auto-compaction.md.

use anyhow::Result;
use futures::StreamExt;

use crate::provider::{Provider, ProviderEvent, Request};
use crate::session::{Session, SessionEvent, SessionStore, new_id};
use chrono::Utc;

const SUMMARIZER_PROMPT: &str = "You summarize agent conversations. Produce a \
dense summary preserving: tasks attempted and their outcomes, decisions made, \
open questions, important file paths, and user preferences. Write it so work \
can continue immediately.";

/// Rough token estimate: max(last reported input tokens, chars/4).
pub fn estimate_tokens(session: &Session) -> u64 {
    let mut last_input = 0u64;
    for event in session.events() {
        if let SessionEvent::AssistantMessage { usage, .. } = event {
            last_input = last_input.max(usage.input_tokens);
        }
    }
    let chars: usize = session
        .transcript()
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|b| match b {
                    crate::session::ContentBlock::Text { text } => text.len(),
                    crate::session::ContentBlock::Thinking { text, .. } => text.len(),
                    crate::session::ContentBlock::ToolCall { input, .. } => input.to_string().len(),
                    crate::session::ContentBlock::ToolResult { content, .. } => content.len(),
                })
                .sum::<usize>()
                + 8
        })
        .sum();
    let estimated = chars as u64 / 4;
    estimated.max(last_input)
}

/// Compact the session if the transcript exceeds
/// `context_limit * threshold`. Runs once per user turn (before the
/// provider loop); the cut keeps the current user message and everything
/// after it.
pub async fn compact_if_needed(
    provider: &dyn Provider,
    store: &SessionStore,
    session_id: &str,
    context_limit: u64,
    threshold: f64,
) -> Result<bool> {
    let mut session = store.load(session_id)?;
    if estimate_tokens(&session) <= (context_limit as f64 * threshold) as u64 {
        return Ok(false);
    }

    // Cut at the current turn's user message (last UserMessage event).
    let cut = session
        .events()
        .iter()
        .rposition(|e| matches!(e, SessionEvent::UserMessage { .. }))
        .unwrap_or(0);

    // Build the older transcript for summarization.
    let older = Session::from_events_for_compaction(session.events(), cut);
    if older.transcript().is_empty() {
        return Ok(false);
    }

    let request = Request {
        model: session.meta().map(|m| m.model.clone()).unwrap_or_default(),
        system_prompt: Some(SUMMARIZER_PROMPT.into()),
        messages: older.transcript(),
        tools: Vec::new(),
        options: serde_json::Value::Null,
    };
    let mut stream = provider.stream(request)?;
    let mut summary = String::new();
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(t) => summary.push_str(&t),
            ProviderEvent::TurnComplete { .. } => break,
            ProviderEvent::Error(e) => anyhow::bail!("compaction call failed: {e}"),
            _ => {}
        }
    }
    if summary.trim().is_empty() {
        anyhow::bail!("compaction produced an empty summary");
    }

    session.append(SessionEvent::Compaction {
        id: new_id(),
        summary,
        kept_from: cut,
        ts: Utc::now(),
    })?;
    Ok(true)
}
