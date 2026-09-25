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
const SUMMARIZATION_INSTRUCTION: &str = "Stop working on the task — for this one turn \
only: you are performing a context checkpoint, and the task itself resumes immediately \
afterwards. Everything above is about to be replaced by what you write now. The \
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

## Services
- [each service still running: name, command, what it is for and how to check it, or \
\"(none)\" — the reader owns these processes and must not start them twice]

## Running
- [each background task or bash job still running: task_id or command, what it is doing, \
whether it holds the checkout, or \"(none)\" — its result will arrive as a notification, and \
the reader must neither redo its scope nor wait on it]

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
- The summary is a handover, not a sign-off: its reader picks the task straight back up. \
Never tell them to stop, wait, or seek confirmation the conversation did not ask for — if \
work remains, Next Move is what they do first.
- Nothing here is lost, only out of sight: the whole conversation stays searchable with the \
history tool, which also lists every instruction the user gave and reads around any event. \
The todo tool, called with no arguments, returns the current plan, and the service tool's \
status action, called with no name, lists what is still running. Summarize with that in \
mind — record what matters and where to look, rather than trying to preserve everything.
- Terse bullets, not prose.
- Do not mention summarizing, compaction, or context limits.";

/// Appended when the conversation already carries a summary. ilar keeps
/// only the newest one in view, so anything this summary leaves out
/// drops out of sight — searchable, but no longer read — and the model
/// deserves to know that before it decides what to drop.
const SUMMARY_CARRY_FORWARD: &str = "

The conversation opens with a <compaction-summary> covering everything before it. That \
summary is replaced by yours: anything you do not carry forward drops out of sight, where only \
a history search finds it. Keep its \
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
fn summarizer_messages(transcript: &[ChatMessage], services: &[String]) -> Vec<ChatMessage> {
    let mut instruction = String::from(SUMMARIZATION_INSTRUCTION);
    if carries_prior_summary(transcript) {
        instruction.push_str(SUMMARY_CARRY_FORWARD);
    }
    if !services.is_empty() {
        instruction.push_str(SERVICES_RUNNING_NOW);
        for service in services {
            instruction.push_str("\n- ");
            instruction.push_str(service);
        }
    }
    let mut messages = transcript.to_vec();
    messages.push(ChatMessage::user_text(instruction));
    messages
}

/// Appended when the session's service manager reports running
/// services. The conversation says what was *started*; only the manager
/// knows what is still up, and a summary that forgets a dev server is
/// how the next context starts a second one on the same port.
const SERVICES_RUNNING_NOW: &str = "

Services running right now, from the session's service manager (this is live; the \
conversation may be older). Every one of these goes in the Services section, with what it \
is for:";

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
    /// The services still running, one `name · command` line each —
    /// `ToolRegistry::running_services`. Injected into the summarizer's
    /// instruction so the handover names them.
    pub services: &'a [String],
    pub cancel: &'a CancellationToken,
    /// Let the model write this memory before the summary: for a
    /// session nobody reviews afterwards. See [`flush_memory`].
    pub memory: Option<&'a std::sync::Arc<crate::memory::MemoryStore>>,
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

/// What the next request will cost, in tokens.
///
/// The provider already priced everything up to its last reply — system
/// prompt and tool schemas included — and said so exactly. Only what has
/// been appended since is unpriced: the tool results that reply asked
/// for, and any message typed after it. So the exact number carries the
/// weight and the guess is confined to that tail, instead of a
/// whole-transcript heuristic competing with a number we were handed.
/// Before the first reply there is nothing to build on and the whole
/// request is estimated.
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
    // The *last* reply, and only if it priced itself: an older number
    // would leave the replies in between uncounted, which is the one
    // way this can undercount rather than overcount.
    let reported = match events[active_from..]
        .iter()
        .rev()
        .find(|event| matches!(event, SessionEvent::AssistantMessage { .. }))
    {
        Some(SessionEvent::AssistantMessage { usage, .. })
            if usage.input_token_accounting.is_some() =>
        {
            Some(usage.context_tokens())
        }
        _ => None,
    };

    if let Some(reported) = reported
        && let Some(last_reply) = transcript
            .iter()
            .rposition(|message| message.role == Role::Assistant)
    {
        let untold: usize = transcript[last_reply + 1..].iter().map(message_chars).sum();
        return reported.saturating_add(untold as u64 / 4);
    }

    let chars: usize = transcript.iter().map(message_chars).sum::<usize>()
        + system_prompt
            .map(str::chars)
            .map(Iterator::count)
            .unwrap_or(0)
        + serde_json::to_string(tools)
            .map(|tools| tools.chars().count())
            .unwrap_or(0);
    chars as u64 / 4
}

/// One rendered message in characters, plus a small allowance for the
/// envelope every provider wraps it in.
fn message_chars(message: &ChatMessage) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            crate::session::ContentBlock::Text { text } => text.chars().count(),
            // This sum is characters, divided by four by the caller; an
            // image's cost is already tokens, so it enters multiplied.
            // Its base64 length says nothing about what a model bills.
            crate::session::ContentBlock::Image { image } => {
                crate::image::estimated_tokens(image) as usize * 4
            }
            crate::session::ContentBlock::Thinking { text, .. } => text.chars().count(),
            crate::session::ContentBlock::ReasoningSummary { .. } => 0,
            crate::session::ContentBlock::Reasoning { item } => item.to_string().chars().count(),
            crate::session::ContentBlock::Diagnostic { .. } => 0,
            crate::session::ContentBlock::ToolCall { input, .. } => {
                input.to_string().chars().count()
            }
            crate::session::ContentBlock::ToolResult {
                content, images, ..
            } => {
                content.chars().count()
                    + images
                        .iter()
                        .map(|image| crate::image::estimated_tokens(image) as usize * 4)
                        .sum::<usize>()
            }
        })
        .sum::<usize>()
        + 8
}

/// Immediately replace the complete active provider transcript with one
/// handover summary. Canonical audit history remains append-only.
pub async fn compact_session(
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    system_prompt: Option<&str>,
    tools: &[ToolDefinition],
    services: &[String],
    cancel: &CancellationToken,
) -> Result<ManualCompactionOutcome> {
    if cancel.is_cancelled() {
        return Ok(ManualCompactionOutcome::Aborted);
    }
    let mut session = store.acquire_writer(session_id)?.load()?;
    if session.transcript().is_empty() {
        return Ok(ManualCompactionOutcome::NothingToCompact);
    }
    // The summary ends in an append, and an append between a tool call
    // and its result is refused. Ask before summarizing rather than
    // after: the refusal is safe either way, but the request is not
    // free. In practice this is a parked question, the one call a
    // restore leaves open on purpose.
    if session.has_unanswered_calls() {
        anyhow::bail!(
            "this session is waiting on a question — answer or abort it before compacting"
        );
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
            services,
            cancel,
            // Asked for by the person, who is right there.
            memory: None,
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

/// One side response — a summary or a flush round — read to its end:
/// its text, its thinking (which a chat-wire model needs back beside
/// its tool calls), and the calls it made.
#[derive(Default)]
struct SideResponse {
    text: String,
    thinking: String,
    field: Option<crate::session::ReasoningField>,
    calls: Vec<(String, String, serde_json::Value)>,
    stop: Option<StopReason>,
}

/// A side request's response, or `None` when cancelled. Errors are the
/// provider's; judging what came back is the caller's job.
async fn respond_once(
    provider: &dyn Provider,
    request: Request,
    cancel: &CancellationToken,
) -> Result<Option<SideResponse>> {
    let mut stream = provider.stream(request)?;
    let mut response = SideResponse::default();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(None),
            next = stream.next() => next,
        };
        let Some(event) = next else {
            anyhow::bail!("stream ended before completion");
        };
        match event {
            ProviderEvent::TextDelta(t) => response.text.push_str(&t),
            ProviderEvent::ThinkingDelta(t) => response.thinking.push_str(&t),
            ProviderEvent::ThinkingField(field) => response.field = Some(field),
            ProviderEvent::ToolCallCompleted { id, name, input } => {
                response.calls.push((id, name, input));
            }
            ProviderEvent::TurnComplete { stop_reason, .. } => {
                response.stop = Some(stop_reason);
                return Ok(Some(response));
            }
            ProviderEvent::Error(e)
            | ProviderEvent::RetryableError(e)
            | ProviderEvent::RateLimited { message: e, .. } => anyhow::bail!("call failed: {e}"),
            _ => {}
        }
    }
}

/// One summarization call: its text, or empty when cancelled.
async fn summarize_once(
    provider: &dyn Provider,
    request: Request,
    cancel: &CancellationToken,
) -> Result<String> {
    let response = respond_once(provider, request, cancel)
        .await
        .map_err(|error| anyhow::anyhow!("compaction {error:#}"))?;
    match response {
        None => Ok(String::new()),
        Some(response) if response.stop == Some(StopReason::EndTurn) => Ok(response.text),
        Some(response) => anyhow::bail!(
            "compaction ended with invalid stop reason {:?}",
            response.stop
        ),
    }
}

/// What the flush asks before a summary.
const FLUSH_INSTRUCTION: &str = "Stop working on the task — for this one turn only: the \
conversation above is about to be summarized, and its detail will be out of sight. First write \
to memory what a later session would need and could not find in the repository, the log or a \
search: a correction or preference from the person, a decision and its reason, a convention no \
file states. Use only the memory, memory_search and memory_get tools, and amend a note that is \
already about the same fact rather than file a second. If there is nothing to keep, answer with \
the word nothing. Do not continue the task.";

/// Rounds of tool calls the flush may take: a search, a write, a check.
const FLUSH_ROUNDS: usize = 3;

/// OpenClaw's flush, for a session nobody reviews: before the summary,
/// one side request over the same conversation, with the same system
/// prompt and tools, so the provider serves it from the prompt cache.
/// Only the memory tools run; the turn is untouched and nothing of the
/// exchange enters the log. Terminal sessions otherwise wrote little —
/// aiko nothing in thirty prompts and five compactions. Best effort: a
/// failure costs the flush, never the compaction. Returns what the
/// memory tool wrote.
async fn flush_memory(
    provider: &dyn Provider,
    summarizer: &Request,
    transcript: &[ChatMessage],
    store: &std::sync::Arc<crate::memory::MemoryStore>,
    cancel: &CancellationToken,
) -> Vec<String> {
    use crate::memory::{MemoryGetTool, MemorySearchTool, MemoryTool};
    use crate::tools::{Tool, ToolContext, ToolOutput};
    let tools: Vec<std::sync::Arc<dyn Tool>> = vec![
        MemoryTool::new(store.clone()),
        MemorySearchTool::new(store.clone()),
        MemoryGetTool::new(store.clone()),
    ];
    let base = Request {
        messages: Vec::new(),
        ..summarizer.clone()
    };
    let mut messages = transcript.to_vec();
    messages.push(ChatMessage::user_text(FLUSH_INSTRUCTION));
    let mut kept = Vec::new();
    for _ in 0..FLUSH_ROUNDS {
        let request = Request {
            messages: messages.clone(),
            ..base.clone()
        };
        let response = match respond_once(provider, request, cancel).await {
            Ok(Some(response)) if !response.calls.is_empty() => response,
            _ => break,
        };
        let mut results = Vec::new();
        for (id, name, input) in &response.calls {
            let output = match tools.iter().find(|tool| tool.name() == name) {
                // The memory tools read no working directory; a context
                // only has to have one that exists.
                Some(tool) => {
                    tool.run(input.clone(), ToolContext::root(std::env::temp_dir()))
                        .await
                }
                None => ToolOutput::error("only the memory tools run before a summary"),
            };
            if name == "memory" && !output.is_error && crate::memory::was_a_write(&output.content) {
                kept.push(kept_line(&output.content, input));
            }
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: output.content,
                is_error: output.is_error,
                images: Vec::new(),
            });
        }
        // The thinking goes back beside the calls: a chat-wire model in
        // thinking mode (Kimi, DeepSeek) refuses a tool call without it.
        let mut content = Vec::new();
        if !response.thinking.is_empty() {
            content.push(ContentBlock::Thinking {
                text: response.thinking,
                field: response.field,
            });
        }
        content.extend(response.calls.into_iter().map(|(id, name, input)| {
            ContentBlock::ToolCall {
                id,
                name,
                input,
                item_id: None,
            }
        }));
        messages.push(ChatMessage {
            role: Role::Assistant,
            content,
        });
        messages.push(ChatMessage {
            role: Role::User,
            content: results,
        });
    }
    kept
}

/// A flush write as the handover lists it: a note by its result, which
/// names it; a core write with the file and the entry, which "added"
/// alone does not.
fn kept_line(result: &str, input: &serde_json::Value) -> String {
    if !(result.starts_with("added")
        || result.starts_with("replaced")
        || result.starts_with("removed"))
    {
        return result.to_string();
    }
    let field = |name: &str| input.get(name).and_then(serde_json::Value::as_str);
    let file = match field("file") {
        Some("user") => "USER.md",
        _ => "MEMORY.md",
    };
    let entry = field("text")
        .or(field("new"))
        .or(field("old"))
        .unwrap_or_default();
    format!(
        "{} in {file}: {entry}",
        result.split_whitespace().next().unwrap_or(result)
    )
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
        messages: summarizer_messages(&transcript, options.services),
        tools: options.tools.to_vec(),
        cache_key: Some(session.session_id().to_string()),
        options: crate::model::variant_options(model, session.effective_variant().as_deref())?,
    };
    let kept = match options.memory {
        Some(store) => flush_memory(provider, &request, &transcript, store, options.cancel).await,
        None => Vec::new(),
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
    // What the flush kept goes in the handover: the next context knows
    // it is written down, and where to look.
    let summary = if kept.is_empty() {
        summary
    } else {
        format!(
            "{}\n\n## Remembered\n{}",
            summary.trim_end(),
            kept.iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

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
                images: Vec::new(),
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

    /// The provider prices each request exactly. Everything up to its
    /// last reply is therefore known, and only what came after — the
    /// tool results it asked for — has to be guessed at. A long, already
    /// priced history must not be re-estimated on top of the number that
    /// already covers it.
    #[test]
    fn priced_history_is_taken_from_the_provider_not_guessed_again() {
        let huge = "x".repeat(400_000);
        let events = vec![SessionEvent::AssistantMessage {
            id: "a1".into(),
            model: "zai/glm-4.7".into(),
            content: vec![ContentBlock::Text { text: huge.clone() }],
            usage: crate::session::Usage {
                input_tokens: 20_000,
                output_tokens: 500,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                input_token_accounting: Some(crate::session::InputTokenAccounting::ExcludesCached),
            },
            stop_reason: "tool_use".into(),
            ts: Utc::now(),
        }];
        let transcript = vec![
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text { text: huge.clone() }],
            },
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: huge }],
            },
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "t".repeat(4_000),
                }],
            },
        ];

        let estimated = estimate_tokens_from(&events, &transcript, Some("system"), &[]);

        // 20,500 priced + about 1,000 for the unpriced tool result.
        assert!(
            (21_000..22_000).contains(&estimated),
            "the priced history was guessed at again: {estimated}"
        );
    }

    /// A session one exchange old must not compact just because the
    /// user attached screenshots. The estimate is
    /// `max(reported, chars/4)`, so an image counted by its base64
    /// length dominated everything the provider actually reported.
    #[test]
    fn attached_screenshots_do_not_trip_the_threshold_on_their_own() {
        // A real screenshot's payload is megabytes of base64 behind a
        // readable header. Padding a small PNG reproduces that shape
        // without encoding noise: the estimator reads dimensions from
        // the header, and the old one read the length.
        let png = crate::image::encode_png(800, 600, &vec![0u8; 800 * 600 * 4]).expect("encodes");
        let shots: Vec<_> = (0..6)
            .map(|_| {
                let mut image = crate::session::ImageContent::png(&png);
                image.data.push_str(&"A".repeat(2_600_000));
                image
            })
            .collect();
        let base64_chars: usize = shots.iter().map(|image| image.data.len()).sum();
        assert!(
            base64_chars / 4 > 1_000_000,
            "the old estimate has to be the huge one for this to prove anything"
        );

        let mut content = vec![ContentBlock::Text {
            text: "make a web version of this".into(),
        }];
        content.extend(shots.into_iter().map(|image| ContentBlock::Image { image }));
        let transcript = vec![ChatMessage {
            role: Role::User,
            content,
        }];

        let estimated = super::estimate_tokens_from(&[], &transcript, None, &[]);

        assert!(
            estimated < trigger_tokens(272_000, 0.85),
            "a first message with screenshots compacted itself: {estimated}"
        );
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

        let messages = summarizer_messages(&transcript, &[]);

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
        assert!(!instruction.contains("replaced by yours"), "{instruction}");
    }

    /// A summarizer once carried "stop working" into the handover and
    /// the next turn obeyed it. The stop must be scoped to the
    /// checkpoint turn, and the summary must never retire its reader.
    #[test]
    fn the_stop_is_scoped_to_the_checkpoint_not_the_task() {
        let transcript = vec![user_text("build the thing")];

        let instruction = text_of(summarizer_messages(&transcript, &[]).last().unwrap());

        assert!(
            instruction.contains("the task itself resumes immediately"),
            "{instruction}"
        );
        assert!(
            instruction.contains("Never tell them to stop"),
            "{instruction}"
        );
    }

    #[test]
    fn a_prior_summary_adds_the_carry_forward_clause() {
        let transcript = vec![
            user_text("<compaction-summary>\nearlier work\n</compaction-summary>"),
            user_text("now do the next thing"),
        ];

        let instruction = text_of(summarizer_messages(&transcript, &[]).last().unwrap());

        assert!(instruction.contains("replaced by yours"), "{instruction}");
        // The same promise as the template's: out of sight, not gone.
        assert!(
            !instruction.contains("carry forward is lost"),
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
