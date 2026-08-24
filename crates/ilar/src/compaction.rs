//! Auto-compaction — see meta/issues/auto-compaction.md.

use anyhow::Result;
use futures::StreamExt;

use crate::provider::{
    Provider, ProviderEvent, ProviderResolver, Request, StopReason, ToolDefinition,
};
use crate::session::{
    ChatMessage, ContentBlock, Role, Session, SessionEvent, SessionReader, SessionStore, new_id,
};
use chrono::Utc;
use tokio_util::sync::CancellationToken;

/// The instruction that turns the live conversation into a summarization
/// request. It is appended as the final user message rather than sent as
/// a system prompt over a replayed transcript: a model shown a live
/// conversation follows its implied next action over any system
/// instruction, and answers the user instead of summarizing.
///
/// Everything before it stays byte-identical to the turn's own request,
/// so the provider serves the conversation from its prompt cache and the
/// compaction pays for the instruction alone.
const SUMMARIZATION_INSTRUCTION: &str = "Stop working on the task. You are performing a \
context checkpoint: write a handover summary so another agent can resume this work from \
your summary alone. Do not continue the conversation, do not answer any question in it, \
do not call any tool, and output nothing but the summary.

Write it as this exact Markdown structure, keeping the section order and every heading \
even when a section is empty.

## Objective
- [what the user is trying to accomplish, in their own words where possible]

## Important Details
- [constraints, decisions and why, rejected approaches, facts needed to continue, or \"(none)\"]

## Work State
### Completed
- [finished and verified work, or \"(none)\"]

### Active
- [work in progress, partial changes, investigation state, or \"(none)\"]

### Blocked
- [blockers, failing commands, unknowns, or \"(none)\"]

## Next Move
1. [the immediate concrete action, or \"(none)\"]
2. [the one after it, if known]

## Relevant Files
- [path: why it matters, or \"(none)\"]

Rules:
- Copy URLs, PR numbers, branch names, worktree paths, file paths, commands, symbols and \
error strings verbatim. Never paraphrase an identifier.
- Record what was ruled out and why, not only what succeeded: a summary of successes \
invites repeating a rejected approach.
- If the conversation shows a todo item is finished, obsolete or wrong, say so under Work \
State: the list is appended below your summary and the next turn corrects it.
- Terse bullets, not prose.
- Do not mention summarizing, compaction, or context limits.

Everything from this conversation stays searchable with the history tool, so record what \
matters and where to look rather than trying to preserve every detail.";

/// Appended when the conversation already carries a summary. ilar keeps
/// only the newest one, so anything this summary leaves out is gone —
/// the model deserves to know that before it decides what to drop.
const SUMMARY_CARRY_FORWARD: &str = "

The conversation opens with a <compaction-summary> covering everything before it. That \
summary is discarded once yours exists: anything you do not carry forward is lost. Keep its \
objectives, constraints, user directives, decisions and parallel workstreams even where the \
later conversation never mentions them, dropping only what is finished and no longer needed. \
Where the two disagree the later conversation wins: state the corrected fact and drop the \
old claim.";

/// Budget for the verbatim user requests pinned above a summary.
const PINNED_REQUEST_CHARS: usize = 4_000;
/// Longest single request kept whole in that block.
const PINNED_REQUEST_MAX: usize = 2_000;
/// Below this a summary of a large conversation is not a summary.
const MIN_SUMMARY_CHARS: usize = 200;
/// Only conversations past this size are held to that floor. The rule
/// exists for the pathological case — a quarter-million tokens
/// summarized to "Done." — and a false positive here costs a retry and
/// then a hard compaction failure, which is the symptom being fixed.
const LARGE_CONVERSATION_CHARS: usize = 50_000;
/// Openings that mean the model answered the conversation instead of
/// summarizing it.
const CONTINUATION_TELLS: &[&str] = &[
    "i'm sorry",
    "i\u{2019}m sorry",
    "i am sorry",
    "i apologize",
    "i apologise",
    "sorry,",
    "i wasn't able",
    "i wasn\u{2019}t able",
    "i was not able",
    "i can't",
    "i can\u{2019}t",
    "i cannot",
    "i couldn't",
    "i couldn\u{2019}t",
    "unfortunately, i",
];

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}\u{2026}")
}

/// The user's own words in the region being summarized, in order. Tool
/// results ride in user-role messages and are not requests; neither is
/// the previous summary.
fn user_requests(messages: &[ChatMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !is_prior_summary(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn is_prior_summary(text: &str) -> bool {
    text.starts_with("<compaction-summary>")
}

fn carries_prior_summary(messages: &[ChatMessage]) -> bool {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Text { text } if is_prior_summary(text)))
}

/// Rough size of the material a summary has to cover.
fn transcript_chars(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|message| {
            message
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => {
                        text.chars().count()
                    }
                    ContentBlock::ToolResult { content, .. } => content.chars().count(),
                    _ => 0,
                })
                .sum::<usize>()
        })
        .sum()
}

/// The conversation exactly as the turn sent it, plus the instruction as
/// a final user message. The shared prefix is what makes this cheap.
fn summarizer_messages(transcript: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut instruction = String::from(SUMMARIZATION_INSTRUCTION);
    if carries_prior_summary(transcript) {
        instruction.push_str(SUMMARY_CARRY_FORWARD);
    }
    let mut messages = transcript.to_vec();
    messages.push(ChatMessage::user_text(instruction));
    messages
}

/// Pin the working plan below the summary. The todo list lives in tool
/// results, so a compaction that drops them leaves the model with no
/// evidence its own plan exists — it keeps working, having quietly
/// forgotten what it meant to do next, while the sidebar still shows
/// the list. State the model cannot see is state it cannot act on.
fn pin_todos(summary: &str, todos: Option<&crate::todo::TodoList>) -> String {
    let Some(checklist) = todos
        .filter(|list| !list.items.is_empty())
        .map(crate::todo::TodoList::checklist)
    else {
        return summary.to_string();
    };
    format!(
        "{summary}\n\nThe todo list at this point, which the todo tool still owns:\n\
{checklist}\nRe-read it before planning, and correct it with the todo tool if the work \
above finished or invalidated an item."
    )
}

/// Pin the user's own words above the summary. They are the cheapest
/// tokens in a transcript and the ones a summarizer is most likely to
/// paraphrase away; their survival should not depend on its judgement.
/// The first request is the objective and is always kept; the rest fill
/// the budget newest-first and are restored to order.
fn pin_requests(summary: &str, requests: &[String]) -> String {
    if requests.is_empty() {
        return summary.to_string();
    }
    let first = truncate_chars(&requests[0], PINNED_REQUEST_MAX);
    let mut remaining = PINNED_REQUEST_CHARS.saturating_sub(first.chars().count());
    let mut kept = vec![(0usize, first)];
    for (index, request) in requests.iter().enumerate().skip(1).rev() {
        let text = truncate_chars(request, PINNED_REQUEST_MAX);
        let cost = text.chars().count();
        if cost > remaining {
            continue;
        }
        remaining -= cost;
        kept.push((index, text));
    }
    kept.sort_by_key(|(index, _)| *index);
    let dropped = requests.len() - kept.len();
    let mut block = String::from("Requests from the summarized history, verbatim:\n");
    for (_, request) in &kept {
        block.push_str(&format!("<request>\n{request}\n</request>\n"));
    }
    if dropped > 0 {
        block.push_str(&format!("({dropped} older requests omitted)\n"));
    }
    format!("{block}\n{summary}")
}

/// Why this text is not a summary, or `None` when it is one. A stored
/// summary replaces real history, so rejecting a bad one costs a retry
/// and accepting it costs the session.
fn degenerate_summary(summary: &str, conversation_chars: usize) -> Option<&'static str> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return Some("empty");
    }
    // An apology is never a summary, whatever the size of the input.
    let opening = trimmed.chars().take(40).collect::<String>().to_lowercase();
    if CONTINUATION_TELLS.iter().any(|tell| opening.contains(tell)) {
        return Some("the model answered the conversation instead of summarizing it");
    }
    if conversation_chars >= LARGE_CONVERSATION_CHARS && trimmed.chars().count() < MIN_SUMMARY_CHARS
    {
        return Some("too short for the material it covers");
    }
    None
}

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
    // A turn's tree checkpoint travels with its user message: cutting
    // between them would strand the snapshot outside the window.
    while cut > floor
        && matches!(events[cut], SessionEvent::UserMessage { .. })
        && matches!(events[cut - 1], SessionEvent::Checkpoint { .. })
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

/// One summarization call. Errors are the provider's; judging the text
/// is the caller's job.
async fn summarize_once(
    provider: &dyn Provider,
    request: Request,
    cancel: &CancellationToken,
) -> Result<String> {
    let mut stream = provider.stream(request)?;
    let mut summary = String::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(String::new()),
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
    Ok(summary)
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
            // The invocation link and the tree checkpoint travel with
            // their user message; cutting between them would strand
            // them outside the window (a rewind to this turn would lose
            // its tree snapshot).
            while cut > 0
                && matches!(
                    session.events()[cut - 1],
                    SessionEvent::SubagentInvocation { .. } | SessionEvent::Checkpoint { .. }
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

    let transcript = older.transcript();
    // The request the turn itself would have sent, with the instruction
    // appended: same system prompt, same tools, same session cache key,
    // so the provider serves the conversation from its prompt cache and
    // only the instruction is new.
    let request = Request {
        model: model.to_string(),
        system_prompt: options.system_prompt.map(str::to_string),
        messages: summarizer_messages(&transcript),
        tools: options.tools.to_vec(),
        continuations: Vec::new(),
        cache_key: Some(session.session_id().to_string()),
        options: crate::model::variant_options(model, session.effective_variant().as_deref())?,
    };
    let material = transcript_chars(&transcript);
    let mut summary = String::new();
    // One retry: a summarizer that answered the conversation usually
    // summarizes on the second ask, and a bad summary is stored
    // history, not a passing error.
    for attempt in 0..2 {
        summary = summarize_once(provider, request.clone(), options.cancel).await?;
        if options.cancel.is_cancelled() {
            return Ok(None);
        }
        match degenerate_summary(&summary, material) {
            None => break,
            Some(reason) if attempt == 1 => {
                anyhow::bail!("compaction produced no usable summary: {reason}")
            }
            Some(_) => continue,
        }
    }

    // The user's own words survive whatever the summarizer decided to
    // paraphrase away.
    let summary = pin_requests(&summary, &user_requests(&transcript));
    let summary = pin_todos(&summary, session.todo_list());
    session.append(SessionEvent::Compaction {
        id: new_id(),
        summary: summary.clone(),
        kept_from: cut,
        ts: Utc::now(),
    })?;
    Ok(Some(summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_text(text: &str) -> ChatMessage {
        ChatMessage::user_text(text)
    }

    fn tool_result(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error: false,
            }],
        }
    }

    fn text_of(message: &ChatMessage) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_instruction_is_appended_after_an_untouched_conversation() {
        let transcript = vec![
            user_text("fix the firehose bundling"),
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "go test ./..."}),
                    item_id: None,
                }],
            },
            tool_result("call-1", "ok"),
        ];

        let messages = summarizer_messages(&transcript);

        // Every original message survives byte-identical and in order:
        // that shared prefix is what the provider serves from cache.
        assert_eq!(messages.len(), transcript.len() + 1);
        assert_eq!(&messages[..transcript.len()], &transcript[..]);
        let instruction = text_of(messages.last().unwrap());
        assert!(
            instruction.contains("Stop working on the task"),
            "{instruction}"
        );
        assert!(instruction.contains("## Objective"), "{instruction}");
        assert!(instruction.contains("verbatim"), "{instruction}");
        // No prior summary here, so no carry-forward clause.
        assert!(
            !instruction.contains("discarded once yours exists"),
            "{instruction}"
        );
    }

    #[test]
    fn a_prior_summary_adds_the_carry_forward_clause() {
        let transcript = vec![
            user_text("<compaction-summary>\nearlier work\n</compaction-summary>"),
            user_text("now do the next thing"),
        ];

        let instruction = text_of(summarizer_messages(&transcript).last().unwrap());

        assert!(
            instruction.contains("discarded once yours exists"),
            "{instruction}"
        );
        // The prior summary is not one of the user's requests.
        assert_eq!(
            user_requests(&transcript),
            vec!["now do the next thing".to_string()]
        );
    }

    #[test]
    fn tool_results_are_not_mistaken_for_requests() {
        let transcript = vec![
            user_text("the objective"),
            tool_result("call-1", "a wall of output"),
        ];

        assert_eq!(
            user_requests(&transcript),
            vec!["the objective".to_string()]
        );
    }

    #[test]
    fn pinned_requests_keep_the_first_and_the_newest() {
        let requests = vec![
            "the original objective".to_string(),
            "filler ".repeat(500),
            "another filler ".repeat(300),
            "the latest instruction".to_string(),
        ];

        let pinned = pin_requests("MODEL SUMMARY", &requests);

        assert!(pinned.contains("the original objective"), "{pinned}");
        assert!(pinned.contains("the latest instruction"), "{pinned}");
        assert!(pinned.contains("1 older requests omitted"), "{pinned}");
        assert!(pinned.ends_with("MODEL SUMMARY"), "{pinned}");
        assert!(
            pinned.chars().count() < PINNED_REQUEST_CHARS * 2,
            "pinned block grew to {}",
            pinned.chars().count()
        );
        // Chronological order, whatever the budget dropped.
        assert!(
            pinned.find("the original objective") < pinned.find("the latest instruction"),
            "{pinned}"
        );

        // Nothing to pin: the summary passes through untouched.
        assert_eq!(pin_requests("MODEL SUMMARY", &[]), "MODEL SUMMARY");
    }

    #[test]
    fn the_plan_is_pinned_only_when_there_is_one() {
        let list = crate::todo::TodoList {
            items: vec![
                crate::todo::TodoItem {
                    content: "read the config".into(),
                    status: crate::todo::Status::Completed,
                },
                crate::todo::TodoItem {
                    content: "fix the parser".into(),
                    status: crate::todo::Status::InProgress,
                },
            ],
        };

        let pinned = pin_todos("SUMMARY", Some(&list));

        assert!(pinned.starts_with("SUMMARY"), "{pinned}");
        assert!(pinned.contains("[x] read the config"), "{pinned}");
        assert!(pinned.contains("[>] fix the parser"), "{pinned}");
        // The tool stays authoritative; the pin is a reminder, not a copy
        // the model is invited to edit in place.
        assert!(pinned.contains("todo tool"), "{pinned}");

        // Nothing to pin: untouched.
        assert_eq!(pin_todos("SUMMARY", None), "SUMMARY");
        assert_eq!(
            pin_todos("SUMMARY", Some(&crate::todo::TodoList::default())),
            "SUMMARY"
        );
    }

    #[test]
    fn an_apology_is_not_a_summary() {
        let long = LARGE_CONVERSATION_CHARS + 1;
        assert!(
            degenerate_summary(
                "I\u{2019}m sorry, but I wasn\u{2019}t able to complete and push all four fixes within this run.",
                long
            )
            .is_some()
        );
        assert!(degenerate_summary("I cannot help with that.", long).is_some());
        assert!(degenerate_summary("   ", long).is_some());
        assert!(degenerate_summary("", 10).is_some());
        // Short but honest summaries of short sessions stand.
        assert!(degenerate_summary("Fixed the typo in README.", 200).is_none());
        // A real summary of a long session stands.
        assert!(degenerate_summary(&"## Objective\nship the thing\n".repeat(20), long).is_none());
    }

    #[test]
    fn transcript_chars_counts_the_material_a_summary_must_cover() {
        let transcript = vec![user_text("12345"), tool_result("call-1", "678")];
        assert_eq!(transcript_chars(&transcript), 8);
    }
}
