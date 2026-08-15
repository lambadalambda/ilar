//! Auto-compaction — see meta/issues/auto-compaction.md.

use anyhow::Result;
use futures::StreamExt;

use crate::provider::{Provider, ProviderEvent, ProviderResolver, Request};
use crate::session::{Session, SessionEvent, SessionStore, new_id};
use chrono::Utc;

const SUMMARIZER_PROMPT: &str = "You summarize agent conversations. Produce a \
dense summary preserving: tasks attempted and their outcomes, decisions made, \
open questions, important file paths, and user preferences. Write it so work \
can continue immediately.";

/// Rough active-context estimate: max(latest post-boundary provider usage,
/// rendered transcript chars/4).
pub fn estimate_tokens(session: &Session) -> u64 {
    let events = session.events();
    let active_from = events
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::Compaction { kept_from, .. } => Some(*kept_from),
            _ => None,
        })
        .unwrap_or(0)
        .min(events.len());
    let reported = events[active_from..]
        .iter()
        .filter_map(|event| match event {
            SessionEvent::AssistantMessage { usage, .. }
                if usage.input_token_accounting.is_some() =>
            {
                Some(usage.context_tokens())
            }
            _ => None,
        })
        .next_back()
        .unwrap_or(0);
    let chars: usize = session
        .transcript()
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|b| match b {
                    crate::session::ContentBlock::Text { text } => text.len(),
                    crate::session::ContentBlock::Thinking { text, .. } => text.len(),
                    crate::session::ContentBlock::Reasoning { item } => item.to_string().len(),
                    crate::session::ContentBlock::Diagnostic { .. } => 0,
                    crate::session::ContentBlock::ToolCall { input, .. } => input.to_string().len(),
                    crate::session::ContentBlock::ToolResult { content, .. } => content.len(),
                })
                .sum::<usize>()
                + 8
        })
        .sum();
    let estimated = chars as u64 / 4;
    estimated.max(reported)
}

/// Compact the session if the transcript exceeds
/// `context_limit * threshold`. Runs once per user turn (before the
/// provider loop); the cut keeps the current user message and everything
/// after it.
pub async fn compact_if_needed(
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    context_limit: u64,
    threshold: f64,
) -> Result<bool> {
    let mut session = store.acquire_writer(session_id)?.load()?;
    let model = session.effective_model();
    let provider = resolver.resolve_provider(&model)?;
    compact_if_needed_locked(
        provider.as_provider(),
        &model,
        &mut session,
        context_limit,
        threshold,
    )
    .await
}

pub(crate) async fn compact_if_needed_locked(
    provider: &dyn Provider,
    model: &str,
    session: &mut Session,
    context_limit: u64,
    threshold: f64,
) -> Result<bool> {
    if estimate_tokens(session) <= (context_limit as f64 * threshold) as u64 {
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
        model: model.to_string(),
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
