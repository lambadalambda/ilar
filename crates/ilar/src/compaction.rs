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
    /// Compact regardless of the threshold (user-requested).
    pub force: bool,
    pub cut: CompactionCut,
    pub system_prompt: Option<&'a str>,
    pub tools: &'a [ToolDefinition],
    pub cancel: &'a CancellationToken,
}

/// Where to cut the history when compacting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompactionCut {
    /// At the current turn's user message: everything before it is
    /// summarized, the turn itself is untouched. The turn-start default.
    TurnBoundary,
    /// Summarize the complete active transcript. Used only by explicit
    /// idle-session compaction; automatic compaction always preserves a tail.
    ActiveHistory,
    /// Inside the current turn, keeping only the most recent steps.
    /// A single agentic turn can outgrow the window on its own, and
    /// `TurnBoundary` cannot help there — mid-turn the last user message
    /// *is* this turn's prompt, so it would summarize nothing.
    RecentSteps,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ManualCompactionOutcome {
    Compacted {
        summary: String,
        context_tokens: u64,
    },
    NothingToCompact,
    Aborted,
}

/// Tokens at which compaction should fire for a given limit.
pub fn trigger_tokens(limit: u64, threshold: f64) -> u64 {
    (limit as f64 * threshold) as u64
}

/// Rough token cost of one event, mirroring `estimate_tokens_from`'s
/// chars/4 accounting closely enough to size a recency window.
fn event_tokens(event: &SessionEvent) -> u64 {
    let chars = match event {
        SessionEvent::UserMessage { text, .. } => text.chars().count(),
        SessionEvent::AssistantMessage { content, .. } => content
            .iter()
            .map(|block| match block {
                crate::session::ContentBlock::Text { text }
                | crate::session::ContentBlock::Thinking { text, .. } => text.chars().count(),
                crate::session::ContentBlock::ToolCall { input, .. } => {
                    input.to_string().chars().count()
                }
                crate::session::ContentBlock::ToolResult { content, .. } => content.chars().count(),
                crate::session::ContentBlock::Reasoning { item } => {
                    item.to_string().chars().count()
                }
                crate::session::ContentBlock::ProviderReplay { content, .. } => {
                    content.to_string().chars().count()
                }
                crate::session::ContentBlock::ReasoningSummary { .. }
                | crate::session::ContentBlock::Diagnostic { .. } => 0,
            })
            .sum(),
        SessionEvent::ToolResult { content, .. } => content.chars().count(),
        _ => 0,
    };
    (chars / 4) as u64 + 2
}

/// Cut that keeps the most recent `keep` tokens of history.
///
/// Any index is a safe cut: assistant messages precede their results, so
/// keeping a message keeps its results, and `transcript_of` drops the
/// orphaned results that lead the kept region. Returns `None` when there
/// is nothing before the recency window worth summarizing.
fn recent_steps_cut(
    events: &[SessionEvent],
    floor: usize,
    keep: u64,
    min_savings: u64,
) -> Option<usize> {
    let mut kept = 0_u64;
    let mut cut = events.len();
    for index in (floor..events.len()).rev() {
        if kept >= keep {
            break;
        }
        kept = kept.saturating_add(event_tokens(&events[index]));
        cut = index;
    }
    // Snap back to the message that opens this step, so the window
    // starts on a complete step rather than on dangling tool results.
    while cut > floor
        && !matches!(
            events[cut],
            SessionEvent::AssistantMessage { .. } | SessionEvent::UserMessage { .. }
        )
    {
        cut -= 1;
    }
    if cut <= floor || cut >= events.len() {
        return None;
    }
    // When the bulk sits in the recency window there is nothing worth
    // summarizing — compacting would drop the task and save nothing, so
    // don't spend a provider call on it.
    let savings: u64 = events[floor..cut].iter().map(event_tokens).sum();
    (savings >= min_savings).then_some(cut)
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

/// Compact a session according to the supplied threshold and cut policy.
///
/// Agent turns use the boundary and recent-step policies. Interactive manual
/// compaction should use [`compact_session`] instead of assembling options.
pub async fn compact_if_needed(
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    options: CompactionOptions<'_>,
) -> Result<Option<String>> {
    if options.cancel.is_cancelled() {
        return Ok(None);
    }
    let mut session = store.acquire_writer(session_id)?.load()?;
    let model = session.effective_model();
    let provider = resolver.resolve_provider(&model)?;
    compact_if_needed_locked(provider.as_provider(), &model, &mut session, options).await
}

/// Immediately replace the complete active provider transcript with one
/// handover summary. Canonical audit history remains append-only.
pub async fn compact_session(
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    system_prompt: Option<&str>,
    tools: &[ToolDefinition],
    cancel: &CancellationToken,
) -> Result<ManualCompactionOutcome> {
    if cancel.is_cancelled() {
        return Ok(ManualCompactionOutcome::Aborted);
    }
    let mut session = store.acquire_writer(session_id)?.load()?;
    if session.transcript().is_empty() {
        return Ok(ManualCompactionOutcome::NothingToCompact);
    }
    let model = session.effective_model();
    let provider = resolver.resolve_provider(&model)?;
    let summary = compact_if_needed_locked(
        provider.as_provider(),
        &model,
        &mut session,
        CompactionOptions {
            context_limit: 0,
            threshold: 0.0,
            force: true,
            cut: CompactionCut::ActiveHistory,
            system_prompt,
            tools,
            cancel,
        },
    )
    .await?;
    match summary {
        Some(summary) => Ok(ManualCompactionOutcome::Compacted {
            context_tokens: estimate_tokens_with_request(&session, system_prompt, tools),
            summary,
        }),
        None if cancel.is_cancelled() => Ok(ManualCompactionOutcome::Aborted),
        None => Ok(ManualCompactionOutcome::NothingToCompact),
    }
}

/// Returns the compaction summary when one was performed.
pub(crate) async fn compact_if_needed_locked(
    provider: &dyn Provider,
    model: &str,
    session: &mut Session,
    options: CompactionOptions<'_>,
) -> Result<Option<String>> {
    if options.cancel.is_cancelled() {
        return Ok(None);
    }
    if !options.force
        && estimate_tokens_with_request(session, options.system_prompt, options.tools)
            <= trigger_tokens(options.context_limit, options.threshold)
    {
        return Ok(None);
    }

    let previous_cut = session
        .events()
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            SessionEvent::Compaction { kept_from, .. } => Some((*kept_from).min(index)),
            _ => None,
        })
        .max()
        .unwrap_or(0);

    let cut = match options.cut {
        CompactionCut::ActiveHistory => session.events().len(),
        CompactionCut::TurnBoundary => {
            // Cut at the current turn's user message (last UserMessage).
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
            cut
        }
        CompactionCut::RecentSteps => {
            // Keep a recency window of roughly a third of the budget, so
            // the compacted transcript has room to grow again.
            let trigger = trigger_tokens(options.context_limit, options.threshold);
            match recent_steps_cut(
                session.events(),
                previous_cut,
                (trigger / 3).max(1),
                (trigger / 4).max(1),
            ) {
                Some(cut) => cut,
                None => return Ok(None),
            }
        }
    };
    if cut <= previous_cut {
        return Ok(None);
    }

    // Build the older transcript for summarization.
    let older = Session::from_events_for_compaction(session.events(), cut);
    if older.transcript().is_empty() {
        return Ok(None);
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
            _ = options.cancel.cancelled() => return Ok(None),
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
            ProviderEvent::Error(e) | ProviderEvent::RetryableError(e) => {
                anyhow::bail!("compaction call failed: {e}")
            }
            _ => {}
        }
    }
    if summary.trim().is_empty() {
        anyhow::bail!("compaction produced an empty summary");
    }
    if options.cancel.is_cancelled() {
        return Ok(None);
    }

    session.append(SessionEvent::Compaction {
        id: new_id(),
        summary: summary.clone(),
        kept_from: cut,
        ts: Utc::now(),
    })?;
    Ok(Some(summary))
}
