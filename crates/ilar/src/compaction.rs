//! Auto-compaction — see meta/issues/auto-compaction.md.

use anyhow::Result;
use futures::StreamExt;

use crate::provider::{
    Provider, ProviderEvent, ProviderResolver, Request, StopReason, ToolDefinition,
};
use crate::session::{ChatMessage, Session, SessionEvent, SessionReader, SessionStore, new_id};
use chrono::Utc;
use tokio_util::sync::CancellationToken;

const SUMMARIZER_PROMPT: &str = "You summarize agent conversations. Produce a \
dense summary preserving: tasks attempted and their outcomes, decisions made, \
open questions, important file paths, and user preferences. Write it so work \
can continue immediately.";

#[derive(Clone, Copy)]
pub struct CompactionOptions<'a> {
    pub context_limit: u64,
    pub threshold: f64,
    pub system_prompt: Option<&'a str>,
    pub tools: &'a [ToolDefinition],
    pub cancel: &'a CancellationToken,
}

/// Rough active-context estimate: max(latest post-boundary provider usage,
/// rendered transcript chars/4).
pub fn estimate_tokens(session: &Session) -> u64 {
    estimate_tokens_with_request(session, None, &[])
}

pub fn estimate_tokens_with_request(
    session: &Session,
    system_prompt: Option<&str>,
    tools: &[ToolDefinition],
) -> u64 {
    estimate_tokens_from(
        session.events(),
        &session.transcript(),
        system_prompt,
        tools,
    )
}

pub fn estimate_reader_tokens_with_request(
    session: &SessionReader,
    system_prompt: Option<&str>,
    tools: &[ToolDefinition],
) -> u64 {
    estimate_tokens_from(
        session.events(),
        &session.transcript(),
        system_prompt,
        tools,
    )
}

fn estimate_tokens_from(
    events: &[SessionEvent],
    transcript: &[ChatMessage],
    system_prompt: Option<&str>,
    tools: &[ToolDefinition],
) -> u64 {
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
    let chars: usize = transcript
        .iter()
        .map(|m| {
            let replay = m
                .content
                .iter()
                .filter_map(|block| match block {
                    crate::session::ContentBlock::ProviderReplay { content, .. } => {
                        Some(content.to_string().chars().count())
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            let neutral = m
                .content
                .iter()
                .map(|block| match block {
                    crate::session::ContentBlock::Text { text } => text.chars().count(),
                    crate::session::ContentBlock::Thinking { text, .. } => text.chars().count(),
                    crate::session::ContentBlock::ReasoningSummary { .. } => 0,
                    crate::session::ContentBlock::Reasoning { item } => {
                        item.to_string().chars().count()
                    }
                    crate::session::ContentBlock::ProviderReplay { .. } => 0,
                    crate::session::ContentBlock::Diagnostic { .. } => 0,
                    crate::session::ContentBlock::ToolCall { input, .. } => {
                        input.to_string().chars().count()
                    }
                    crate::session::ContentBlock::ToolResult { content, .. } => {
                        content.chars().count()
                    }
                })
                .sum::<usize>();
            replay.max(neutral) + 8
        })
        .sum::<usize>()
        + system_prompt
            .map(str::chars)
            .map(Iterator::count)
            .unwrap_or(0)
        + serde_json::to_string(tools)
            .map(|tools| tools.chars().count())
            .unwrap_or(0);
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
    options: CompactionOptions<'_>,
) -> Result<bool> {
    if options.cancel.is_cancelled() {
        return Ok(false);
    }
    let mut session = store.acquire_writer(session_id)?.load()?;
    let model = session.effective_model();
    let provider = resolver.resolve_provider(&model)?;
    compact_if_needed_locked(provider.as_provider(), &model, &mut session, options).await
}

pub(crate) async fn compact_if_needed_locked(
    provider: &dyn Provider,
    model: &str,
    session: &mut Session,
    options: CompactionOptions<'_>,
) -> Result<bool> {
    if options.cancel.is_cancelled() {
        return Ok(false);
    }
    if estimate_tokens_with_request(session, options.system_prompt, options.tools)
        <= (options.context_limit as f64 * options.threshold) as u64
    {
        return Ok(false);
    }

    // Cut at the current turn's user message (last UserMessage event).
    let mut cut = session
        .events()
        .iter()
        .rposition(|e| matches!(e, SessionEvent::UserMessage { .. }))
        .unwrap_or(0);
    if cut > 0
        && matches!(
            session.events()[cut - 1],
            SessionEvent::SubagentInvocation { .. }
        )
    {
        cut -= 1;
    }

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
        continuations: Vec::new(),
        cache_key: None,
        options: crate::model::variant_options(model, session.effective_variant().as_deref())?,
    };
    let mut stream = provider.stream(request)?;
    let mut summary = String::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = options.cancel.cancelled() => return Ok(false),
            next = stream.next() => next,
        };
        let Some(event) = next else {
            anyhow::bail!("compaction stream ended before completion");
        };
        match event {
            ProviderEvent::TextDelta(t) => summary.push_str(&t),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                ..
            } => break,
            ProviderEvent::TurnComplete { stop_reason, .. } => {
                anyhow::bail!("compaction ended with invalid stop reason {stop_reason:?}")
            }
            ProviderEvent::Error(e) => anyhow::bail!("compaction call failed: {e}"),
            _ => {}
        }
    }
    if summary.trim().is_empty() {
        anyhow::bail!("compaction produced an empty summary");
    }
    if options.cancel.is_cancelled() {
        return Ok(false);
    }

    session.append(SessionEvent::Compaction {
        id: new_id(),
        summary,
        kept_from: cut,
        ts: Utc::now(),
    })?;
    Ok(true)
}
