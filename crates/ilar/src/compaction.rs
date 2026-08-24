//! Auto-compaction — see meta/issues/auto-compaction.md.

use anyhow::Result;
use futures::StreamExt;

use crate::provider::{
    Provider, ProviderEvent, ProviderResolver, Request, StopReason, ToolDefinition,
};
use crate::session::{
    ChatMessage, ContentBlock, Session, SessionEvent, SessionReader, SessionStore, new_id,
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
context checkpoint: everything above is about to be replaced by what you write now. The \
next turn sees your system prompt, your tools, and this summary — nothing else — so write \
the handover you would want to receive. Do not continue the conversation, do not answer any \
question in it, do not call any tool, and output nothing but the summary.

Write it as this exact Markdown structure, keeping the section order and every heading even \
when a section is empty.

## Objective
- [what the user is trying to accomplish, in their words where it matters]

## Important Details
- [constraints, decisions and why, rejected approaches, facts needed to continue, or \"(none)\"]

## Work State
### Completed
- [finished and verified work, or \"(none)\"]

### Active
- [work in progress, partial changes, investigation state, or \"(none)\"]

### Blocked
- [blockers, failing commands, unknowns, or \"(none)\"]

## Plan
- [the todo list as it now stands, or \"(none)\"]

## Next Move
1. [the immediate concrete action, or \"(none)\"]
2. [the one after it, if known]

## Relevant Files
- [path: why it matters, or \"(none)\"]

## Not Carried
- [what you are leaving behind that may still matter, and the words to search for it, or \"(none)\"]

Rules:
- Copy URLs, PR numbers, branch names, worktree paths, file paths, commands, symbols and \
error strings verbatim. Never paraphrase an identifier.
- Record what was ruled out and why, not only what succeeded: a summary of successes \
invites repeating a rejected approach.
- Nothing here is lost, only out of sight: the whole conversation stays searchable with the \
history tool, which also lists every instruction the user gave and reads around any event. \
The todo tool, called with no arguments, returns the current plan. Summarize with that in \
mind — record what matters and where to look, rather than trying to preserve everything.
- Terse bullets, not prose.
- Do not mention summarizing, compaction, or context limits.";

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

fn is_prior_summary(text: &str) -> bool {
    text.starts_with("<compaction-summary>")
}

fn carries_prior_summary(messages: &[ChatMessage]) -> bool {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Text { text } if is_prior_summary(text)))
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

/// Why this text is not a summary, or `None` when it is one. This only
/// catches the model failing to summarize at all — empty output or an
/// answer to the conversation. Judging the *quality* of a summary is
/// not its job: a model trusted to do the work is trusted to hand it
/// over.
fn degenerate_summary(summary: &str) -> Option<&'static str> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return Some("empty");
    }
    let opening = trimmed.chars().take(40).collect::<String>().to_lowercase();
    if CONTINUATION_TELLS.iter().any(|tell| opening.contains(tell)) {
        return Some("the model answered the conversation instead of summarizing it");
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
///
/// Both variants summarize *everything* before their cut: after a
/// compaction the model is left with its system prompt, its tools and
/// one summary. There is no recency window, because a window has to
/// guess what will matter, and the archive is searchable now — anything
/// the summary did not carry is a `history` query away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompactionCut {
    /// At the current turn's user message. That message is the request
    /// being served, not history, so it stays; everything before it
    /// becomes the summary. The turn-start default.
    TurnBoundary,
    /// Everything, including the turn in progress. Used mid-turn, where
    /// the last user message *is* this turn's prompt so `TurnBoundary`
    /// would summarize nothing, and by explicit idle compaction.
    ActiveHistory,
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
    let summary = summarize_once(provider, request, options.cancel).await?;
    if options.cancel.is_cancelled() {
        return Ok(None);
    }
    // A summary is the whole of what survives, so a bad one is not
    // something to paper over: say what went wrong and leave the
    // session alone.
    if let Some(reason) = degenerate_summary(&summary) {
        anyhow::bail!("compaction produced no usable summary: {reason}");
    }

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
    use crate::session::Role;

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
        // The handover has to say the archive exists, or the model
        // guesses instead of looking things up.
        assert!(instruction.contains("history tool"), "{instruction}");
        assert!(instruction.contains("## Not Carried"), "{instruction}");
        assert!(instruction.contains("## Plan"), "{instruction}");
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
    }

    #[test]
    fn an_apology_is_not_a_summary() {
        assert!(
            degenerate_summary(
                "I\u{2019}m sorry, but I wasn\u{2019}t able to complete and push all four fixes within this run.",
            )
            .is_some()
        );
        assert!(degenerate_summary("I cannot help with that.").is_some());
        assert!(degenerate_summary("   ").is_some());
        assert!(degenerate_summary("").is_some());
        // Length is not judged: a terse summary is the model's call.
        assert!(degenerate_summary("Fixed the typo in README.").is_none());
        assert!(degenerate_summary(&"## Objective\nship the thing\n".repeat(20)).is_none());
    }
}
