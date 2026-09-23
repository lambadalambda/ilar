//! Rebuilding transcript state from a persisted session.
//!
//! Replays session events into `Line_`s, and parses the notification
//! envelopes background work writes into the transcript so they render
//! as task/job rows rather than as things the user said.

use ilar::session::SessionStore;

use crate::diff;
use crate::transcript::{Line_, ToolKind, ToolState};

#[derive(Default)]
pub(crate) struct RestoredSessionView {
    pub(crate) lines: Vec<Line_>,
    pub(crate) latest_usage: Option<ilar::session::Usage>,
    pub(crate) total_usage: ilar::session::Usage,
    /// `None` once any step lacked pricing (custom or plan-only model).
    pub(crate) total_cost: Option<f64>,
    /// What this session's subagents spent, summed by the with-store
    /// restore. Zero from the plain invocation view, which reads one
    /// log.
    pub(crate) task_usage: ilar::session::Usage,
    pub(crate) task_cost: Option<f64>,
    /// The session's last turn ended in a recorded error and nothing
    /// has happened since: the resume Ctrl-R offers is still on the
    /// table, and the restore is the only place that can say so.
    pub(crate) resume_offer: bool,
    /// Events of the whole log these lines do not cover — what the
    /// newest compaction folded away, as an index into
    /// `SessionStore::whole_events`. Zero unless the session was
    /// compacted before it was opened, and always zero for a child
    /// slice, which is a window onto a timeline rather than onto a log.
    ///
    /// An export reads these back and splices them in front; the screen
    /// does not want them, which is what the cut is for.
    pub(crate) history_before: usize,
}

/// Whether the log ends on a turn that never finished: one that
/// recorded a failure, one whose tool call nobody answered, or one
/// whose results the provider was never told about.
///
/// Walked from the end. A typed user message means the session moved
/// on and nothing of that turn is left to continue — a task result
/// arriving is not that, and is stepped over; an assistant message
/// carrying a `TurnError` is a failure outright, and one carrying a
/// tool call the log stops after is a turn cut while the tool ran; a
/// tool result at the end is a turn cut between the result and the
/// provider call it was owed to.
///
/// Those last two are how an abort looks: the user stopping a turn is
/// not an error, so nothing writes a `TurnError` and the severed chain
/// is the only trace left. Without them the live offer — "turn aborted
/// — Ctrl-R resumes it" — died on reopen, and the same session said
/// there was nothing to resume. A turn that hit `MaxIterations` stops
/// in exactly the same shape.
/// Whether a user message is one the delivery machinery wrote rather
/// than one a person typed. Both envelopes, because a task result and
/// a background job's ending travel the same channel and land the
/// same way.
fn is_an_arrival(text: &str) -> bool {
    task_notification_display(text).is_some() || tool_notification_display(text).is_some()
}

pub(crate) fn ends_mid_turn(events: &[ilar::session::SessionEvent]) -> bool {
    use ilar::session::{ContentBlock, DiagnosticKind, SessionEvent};
    events.iter().rev().find_map(|event| match event {
        // An arrival is the one user message nobody typed — a task
        // result or a background job's ending, which ride the same
        // delivery. It starts a turn or, salvaged, no turn at all;
        // either way it did not end the turn it landed behind, and
        // reading it as the session moving on took the offer away
        // from a turn still every bit as resumable.
        SessionEvent::UserMessage { text, .. } if is_an_arrival(text) => None,
        SessionEvent::UserMessage { .. } => Some(false),
        SessionEvent::ToolResult { .. } => Some(true),
        SessionEvent::AssistantMessage { content, .. } => Some(content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Diagnostic {
                    kind: DiagnosticKind::TurnError,
                    ..
                } | ContentBlock::ToolCall { .. }
            )
        })),
        _ => None,
    }) == Some(true)
}

/// Two cost totals into one; a `None` on either side (an unpriced
/// step somewhere) poisons the sum, the same rule as [`accrue_usage`].
pub(crate) fn add_costs(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        _ => None,
    }
}

/// Fold one step's usage into session totals; unknown pricing poisons the
/// dollar total (tokens keep accumulating).
/// Field-wise saturating add of one usage into a total.
pub(crate) fn add_usage(total: &mut ilar::session::Usage, usage: &ilar::session::Usage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.cache_read_input_tokens = total
        .cache_read_input_tokens
        .saturating_add(usage.cache_read_input_tokens);
    total.cache_creation_input_tokens = total
        .cache_creation_input_tokens
        .saturating_add(usage.cache_creation_input_tokens);
}

pub(crate) fn accrue_usage(
    total: &mut ilar::session::Usage,
    cost: &mut Option<f64>,
    model: &str,
    usage: &ilar::session::Usage,
) {
    add_usage(total, usage);
    if let Some(current) = cost.as_mut() {
        match ilar::model::pricing_for(model) {
            Some(pricing) => *current += pricing.cost(usage),
            None => *cost = None,
        }
    }
}

/// The line standing for a memory recall: how many notes the model was
/// handed after the prompt.
pub(crate) fn memory_recall_display(count: usize) -> String {
    match count {
        1 => "memory: 1 note recalled for this prompt".to_string(),
        n => format!("memory: {n} notes recalled for this prompt"),
    }
}

pub(crate) fn task_notification_display(text: &str) -> Option<String> {
    notification_display(text, "task-notification", normalize_task_notification)
}

/// The row a task result wears: what finished and how, in the words
/// the producer used, minus the wrapper and the id. A row's first
/// line is the one thing a reader scans, so it gets `Fix tests
/// completed` — not `Task "Fix tests" completed (task_id: 7c1e…)`.
/// The id is still worth having (it is what the `tasks` tool and a
/// follow-up `task_message` take), so it moves to the body.
///
/// Producer strings (subagent.rs): `Task "{d}" completed (task_id:
/// {id}).`, `Task "{d}" failed: …`, `Task "{d}" was cancelled.`,
/// `Task "{d}" was aborted.`, `Task "{d}" stalled: …`, `Task "{d}"
/// ended abnormally (task_id: …) …`, and the
/// propagated `Nested task "{d}" completed.` / `failed …`. Anything
/// else is shown as written.
fn normalize_task_notification(first: &str) -> Headline {
    let (nested, rest) = match first.strip_prefix("Nested task \"") {
        Some(rest) => (true, rest),
        None => match first.strip_prefix("Task \"") {
            Some(rest) => (false, rest),
            None => return Headline::plain(first),
        },
    };
    // Description and outcome may both contain quotes, so the split is
    // the first closing quote followed by one of the producer's verbs.
    const VERBS: [&str; 6] = [
        "\" completed",
        "\" failed:",
        "\" was cancelled",
        "\" was aborted",
        "\" stalled:",
        "\" ended abnormally",
    ];
    let Some(split) = VERBS.iter().filter_map(|verb| rest.find(verb)).min() else {
        return Headline::plain(first);
    };
    let description = &rest[..split];
    let outcome = &rest[split + 2..];
    let (outcome, id) = split_task_id(outcome);
    let prefix = if nested { "nested: " } else { "" };
    Headline {
        first: format!("{prefix}{description} {outcome}"),
        detail: id.map(|id| format!("task_id: {id}")),
    }
}

/// `completed (task_id: X).` → (`completed.`, `X`); anything without
/// the parenthetical is returned as is.
fn split_task_id(outcome: &str) -> (String, Option<String>) {
    let Some(open) = outcome.find(" (task_id: ") else {
        return (outcome.to_string(), None);
    };
    let after = &outcome[open + " (task_id: ".len()..];
    let Some(close) = after.find(')') else {
        return (outcome.to_string(), None);
    };
    let id = after[..close].to_string();
    let rest = &after[close + 1..];
    (format!("{}{rest}", &outcome[..open]), Some(id))
}

pub(crate) fn tool_notification_display(text: &str) -> Option<String> {
    notification_display(text, "tool-notification", normalize_tool_notification)
}

/// `Background job job-1 ("Run checks") completed.` → `Run checks
/// completed.`, with the job id in the body: the description is what
/// the reader recognises, the id is what a cancel takes.
fn normalize_tool_notification(first: &str) -> Headline {
    let Some(rest) = first.strip_prefix("Background job ") else {
        return Headline::plain(first);
    };
    let Some((id, tail)) = rest.split_once(" (\"") else {
        return Headline::plain(rest);
    };
    let Some(split) = tail.rfind("\") ") else {
        return Headline::plain(rest);
    };
    Headline {
        first: format!("{} {}", &tail[..split], &tail[split + 3..]),
        detail: Some(format!("job: {id}")),
    }
}

/// A notification's first line as the row shows it, plus a detail line
/// the normalizer moved out of it (an id) that the body keeps.
struct Headline {
    first: String,
    detail: Option<String>,
}

impl Headline {
    fn plain(first: &str) -> Self {
        Self {
            first: first.to_string(),
            detail: None,
        }
    }
}

fn notification_display(
    text: &str,
    tag: &str,
    normalize_first: impl FnOnce(&str) -> Headline,
) -> Option<String> {
    let opening = format!("<{tag}>\n");
    let closing = format!("\n</{tag}>");
    let inner = text.strip_prefix(&opening)?.strip_suffix(&closing)?;
    let (first, body) = inner.split_once('\n').unwrap_or((inner, ""));
    let body = body
        .strip_prefix("<result>\n")
        .and_then(|body| body.strip_suffix("\n</result>"))
        .unwrap_or(body);
    let Headline { first, detail } = normalize_first(first);
    let mut display = first;
    if !body.is_empty() {
        display.push('\n');
        display.push_str(body);
    }
    if let Some(detail) = detail {
        display.push('\n');
        display.push_str(&detail);
    }
    Some(display)
}

/// Whether the session being restored is finished or still working.
///
/// It changes exactly one thing, and it matters: a restore's last act
/// is to mark every still-open tool row failed, which is the truth for
/// a session nobody is driving and a lie for one mid-`cargo test`.
/// Worse than a lie — `finish_tool_row` refuses to settle a Failed row,
/// so the result that finally arrives is dropped and the row keeps
/// lying until the view is opened again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Liveness {
    /// Nothing is driving this session: what was running died.
    Settled,
    /// A turn is in flight: open rows stay open and settle from the
    /// live stream.
    Running,
}

/// A store-less replay of a session nobody is driving. Every caller
/// outside the tests wants the store too, for its children's history.
#[cfg(test)]
pub(crate) fn restored_session_view(session: &ilar::session::SessionReader) -> RestoredSessionView {
    let mut view = restored_session_invocation_view(
        session.events(),
        pending_question_id(session),
        None,
        Liveness::Settled,
    );
    view.history_before = session.event_base();
    view
}

/// The tool call a session is waiting on an answer for, as the replay
/// wants it: an id to leave running while every other open row settles.
fn pending_question_id(session: &ilar::session::SessionReader) -> Option<&str> {
    session
        .pending_question()
        .map(|pending| pending.tool_call_id.as_str())
}

/// The lines a session's events render to, and nothing else: no store,
/// no children, no totals. What a preview of somebody's last session
/// needs — see [`crate::app::Ghost`] — over whatever slice of events it
/// was given, which for a ghost is a bounded tail rather than a log.
pub(crate) fn replayed_lines(events: &[ilar::session::SessionEvent]) -> Vec<Line_> {
    restored_session_invocation_view(events, None, None, Liveness::Settled).lines
}

/// Bytes of a session's log a ghost reads, from the end. Two
/// screenfuls of conversation are a few kilobytes; the rest of an 80 MB
/// log is never touched, which is what makes the offer free to make.
/// Sized like the store's own head scan, and like the cap on a kept
/// tool result, so one big result at the end of a log still leaves
/// whole records in the window.
const GHOST_TAIL_BYTES: u64 = 256 * 1024;
/// Events the tail read keeps, before rendering.
const GHOST_TAIL_EVENTS: usize = 60;
/// A log this size or smaller is read whole when the window came back
/// with nothing — a last record bigger than the window, or a window
/// that was all rewind. Above it the offer is dropped instead: no offer
/// is better than a pause on startup.
///
/// Measured, release build, 2026-09-21: a 13 MB log of ten thousand
/// events loads cold in 21 ms. This ceiling is a few times that — the
/// biggest logs a session ever grows — and still not a pause. It was
/// 4 MiB, which dropped the offer on exactly the long sessions whose
/// last act was a rewind.
const GHOST_WHOLE_READ_BYTES: u64 = 32 * 1024 * 1024;
/// Transcript lines the ghost keeps, from the end: a couple of
/// screenfuls, so a chatty tail cannot push the prompt off the screen.
const GHOST_LINES: usize = 40;
/// Columns the offered session's name may take in the header.
const GHOST_TITLE_CHARS: usize = 56;

/// The offer's one header line: which session, and how long since it
/// was used. The keys that answer it are stated under the cursor, in
/// the empty prompt, where they are obeyed.
pub(crate) fn ghost_header(
    title: &str,
    modified: std::time::SystemTime,
    now: std::time::SystemTime,
) -> String {
    format!(
        "previous session here: {title} · {}",
        crate::modals::last_used(modified, now),
    )
}

/// The end of a session, for a preview: the bounded tail read, and
/// failing that a whole read of a log small enough to afford one.
///
/// The window comes back empty on a log whose last record is bigger
/// than it and on one whose window held nothing but a rewind — both
/// perfectly ordinary sessions, and both would otherwise be offered
/// with no ghost at all, which is to say not offered.
fn ghost_events(store: &SessionStore, id: &str) -> Vec<ilar::session::SessionEvent> {
    let bounded = ilar::session::tail_events(store, id, GHOST_TAIL_BYTES, GHOST_TAIL_EVENTS)
        .unwrap_or_default();
    if !bounded.is_empty() {
        return bounded;
    }
    let small = store
        .session_path(id)
        .and_then(std::fs::metadata)
        .is_ok_and(|metadata| metadata.len() <= GHOST_WHOLE_READ_BYTES);
    if !small {
        return Vec::new();
    }
    let mut events = store
        .load(id)
        .map(|session| session.events().to_vec())
        .unwrap_or_default();
    events.drain(..events.len().saturating_sub(GHOST_TAIL_EVENTS));
    events
}

/// The offer a bare launch makes in this directory, or `None` when
/// there is nothing to offer.
///
/// The session is the one `--continue` would take here — the same
/// resolution, so the offer and the flag cannot disagree about what
/// "here" means. A directory nothing was ever launched in, a session
/// with no title (nobody typed in it) and a tail that renders to
/// nothing are all the same answer.
pub(crate) fn ghost_offer(
    store: &SessionStore,
    cwd: &std::path::Path,
    now: std::time::SystemTime,
) -> Option<crate::app::Ghost> {
    let session = ilar::runtime::last_session_here(store, cwd)?;
    // One name for the session in the header and in the status line,
    // bounded so neither has to wrap.
    let title = crate::text::truncate_display(
        &session.title?,
        GHOST_TITLE_CHARS,
        crate::text::Truncation::Right,
    );
    let mut lines = replayed_lines(&ghost_events(store, &session.id));
    // Bounded from the end: what the session was last doing is what
    // makes the choice, and the head of the window is the part the tail
    // read already cut arbitrarily.
    lines.drain(..lines.len().saturating_sub(GHOST_LINES));
    if lines.is_empty() {
        return None;
    }
    Some(crate::app::Ghost::new(
        session.id,
        title.clone(),
        ghost_header(&title, session.modified, now),
        lines,
    ))
}

/// Click-target id for a restored thought or note. Nested subagent lines
/// get none: like the live path, they are previews, not expandable — and
/// the click handler only ever scans top-level lines, so an id down here
/// would toggle an unrelated line that happens to share it.
fn restored_line_id(nested: bool, prefix: &str, index: usize) -> String {
    if nested {
        String::new()
    } else {
        format!("{prefix}:restored:{index}")
    }
}

/// Events rather than a reader: the only thing this replay wants from a
/// session beyond its events is the question it is waiting on, and a
/// caller with a bounded slice of somebody's log (a ghost) has no reader
/// to offer at all.
/// How much of a log a render covers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Window {
    /// From the newest compaction. What the model still carries, and
    /// so what the screen shows, with the handover folded at the top.
    Compacted,
    /// Every event, compacted-away turns included, and no handover
    /// note — an export wants the conversation that happened, not the
    /// window the model kept of it.
    Whole,
}

fn restored_session_invocation_view(
    all_events: &[ilar::session::SessionEvent],
    pending_question_id: Option<&str>,
    parent_tool_call_id: Option<&str>,
    liveness: Liveness,
) -> RestoredSessionView {
    restored_session_invocation_view_in(
        all_events,
        pending_question_id,
        parent_tool_call_id,
        liveness,
        Window::Compacted,
    )
}

fn restored_session_invocation_view_in(
    all_events: &[ilar::session::SessionEvent],
    pending_question_id: Option<&str>,
    parent_tool_call_id: Option<&str>,
    liveness: Liveness,
    window: Window,
) -> RestoredSessionView {
    let nested = parent_tool_call_id.is_some();
    // Where this view's slice begins in the event list. A child view
    // starts partway in and `Compaction.kept_from` indexes the whole
    // list, so the two have to be rebased against each other.
    let mut slice_start = 0usize;
    let events = match parent_tool_call_id {
        Some(parent_tool_call_id) => {
            let start = all_events.iter().position(|event| {
                matches!(
                    event,
                    ilar::session::SessionEvent::SubagentInvocation {
                        parent_tool_call_id: current,
                        ..
                    } if current == parent_tool_call_id
                )
            });
            let Some(start) = start else {
                return RestoredSessionView {
                    total_cost: Some(0.0),
                    task_cost: Some(0.0),
                    ..RestoredSessionView::default()
                };
            };
            let end = all_events[start + 1..]
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        ilar::session::SessionEvent::SubagentInvocation { .. }
                    )
                })
                .map(|offset| start + 1 + offset)
                .unwrap_or(all_events.len());
            slice_start = start + 1;
            &all_events[start + 1..end]
        }
        None => all_events,
    };
    let mut cut = 0usize;
    let mut summary = None;
    // Under `Whole` there is no cut to find and no handover to fold:
    // the caller is rendering the half a cut left behind, and a
    // "transcript compacted" note above it would claim this stretch
    // was the thing folded.
    for (index, event) in events.iter().enumerate().take(match window {
        Window::Compacted => events.len(),
        Window::Whole => 0,
    }) {
        if let ilar::session::SessionEvent::Compaction {
            kept_from,
            summary: current,
            ..
        } = event
        {
            // `kept_from` indexes the event list the reader hands out
            // (the store rebases it onto the active window on load), so
            // a nested slice has to subtract its own start before
            // clamping — otherwise a child's compaction would cut its
            // timeline at an index belonging to the whole session.
            cut = kept_from.saturating_sub(slice_start).min(index).max(cut);
            summary = Some(current.as_str());
        }
    }
    let latest_usage = events.iter().rev().find_map(|event| match event {
        ilar::session::SessionEvent::AssistantMessage { usage, .. }
            if usage.context_tokens() > 0 =>
        {
            Some(*usage)
        }
        _ => None,
    });
    // Session totals span the whole log, including compacted-away turns.
    let mut total_usage = ilar::session::Usage::default();
    let mut total_cost = Some(0.0);
    for event in events {
        if let ilar::session::SessionEvent::AssistantMessage { model, usage, .. } = event {
            accrue_usage(&mut total_usage, &mut total_cost, model, usage);
        }
    }
    // Folded. Every restored session that has ever been compacted
    // opens with this, and as plain system rows the handover — which
    // can run to a screenful — pushed the conversation out of sight
    // before it began. The headline says it happened; the body is one
    // keystroke away.
    let mut lines = summary
        .map(|summary| {
            vec![Line_::Note {
                id: restored_line_id(nested, "note", 0),
                text: format!("transcript compacted\n{summary}"),
                expanded: false,
            }]
        })
        .unwrap_or_default();
    // Each call's raw arguments, kept until its result arrives: the
    // result redaction needs to know which values the arguments hid.
    let mut call_inputs: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for event in &events[cut..] {
        match event {
            ilar::session::SessionEvent::Meta { .. } => {}
            ilar::session::SessionEvent::SubagentInvocation { .. } => {}
            ilar::session::SessionEvent::Checkpoint { .. } => {}
            // Session state, shown in the header and the picker, never
            // in the transcript.
            ilar::session::SessionEvent::Topic { .. } => {}
            // Folded out of replay before the view ever sees one; kept
            // total so a raw event stream renders as nothing.
            ilar::session::SessionEvent::Rewind { .. } => {}
            // The pictures stay in the log and in this view; only the
            // provider stops seeing them.
            ilar::session::SessionEvent::ImageCutoff { .. } => {}
            ilar::session::SessionEvent::UserMessage { text, images, .. } => {
                match task_notification_display(text) {
                    Some(text) => lines.push(Line_::Task {
                        id: restored_line_id(nested, "note", lines.len()),
                        text,
                        expanded: false,
                    }),
                    None => match tool_notification_display(text) {
                        Some(text) => lines.push(Line_::Job {
                            id: restored_line_id(nested, "note", lines.len()),
                            text,
                            expanded: false,
                        }),
                        // Inside a subagent's timeline a user message
                        // is the parent's `task_message`, not anything
                        // the person typed — labelling it `you` there
                        // claimed they had written something they
                        // never saw.
                        None => {
                            let said = crate::transcript::user_text_with_images(text, images);
                            lines.push(if nested {
                                Line_::Incoming(said)
                            } else {
                                Line_::User(said)
                            });
                        }
                    },
                }
            }
            ilar::session::SessionEvent::AssistantMessage {
                id: message_id,
                content,
                ..
            } => {
                let mut tool_run = 0usize;
                let mut in_tool_run = false;
                for block in content {
                    if matches!(block, ilar::session::ContentBlock::ToolCall { .. }) {
                        if !in_tool_run {
                            tool_run += 1;
                            in_tool_run = true;
                        }
                    } else {
                        in_tool_run = false;
                    }
                    match block {
                        // Never appears in assistant content.
                        ilar::session::ContentBlock::Image { .. } => {}
                        ilar::session::ContentBlock::Text { text } => match lines.last_mut() {
                            Some(Line_::Assistant(current)) => current.push_str(text),
                            _ => lines.push(Line_::Assistant(text.clone())),
                        },
                        ilar::session::ContentBlock::ReasoningSummary {
                            text,
                            completed: true,
                        } => {
                            lines.push(Line_::Thought {
                                id: restored_line_id(nested, "thought", lines.len()),
                                text: text.clone(),
                                complete: true,
                                expanded: false,
                            });
                        }
                        ilar::session::ContentBlock::ReasoningSummary {
                            completed: false, ..
                        } => {}
                        ilar::session::ContentBlock::ToolCall {
                            id, name, input, ..
                        } => {
                            call_inputs.insert(id.clone(), input.clone());
                            let (kind, arguments) = if name == "task" {
                                match ilar::agent::summarize_task_input(input) {
                                    Some((description, agent, model)) => {
                                        (ToolKind::Agent { name: agent, model }, description)
                                    }
                                    None => (
                                        ToolKind::Agent {
                                            name: "subagent".into(),
                                            model: None,
                                        },
                                        ilar::agent::summarize_tool_input(name, input),
                                    ),
                                }
                            } else {
                                (
                                    ToolKind::Tool,
                                    ilar::agent::summarize_tool_input(name, input),
                                )
                            };
                            // The live path's constructor, seeded with
                            // what only a replay knows at birth: the
                            // live row learns its arguments and its
                            // diff from later events. Spelling the
                            // variant out here is how the two drifted
                            // over what a fresh row is.
                            lines.push(crate::transcript::new_seeded_tool_row(
                                id,
                                format!("{message_id}:{tool_run}"),
                                name,
                                crate::transcript::ToolSeed {
                                    kind,
                                    arguments,
                                    argument_detail: ilar::agent::tool_argument_detail(name, input),
                                    diff: diff::tool_diff_value(name, input),
                                },
                            ));
                        }
                        // Why the turn stopped. Without it a resumed
                        // session that died mid-turn just ends, and the
                        // reader is left guessing at the silence.
                        ilar::session::ContentBlock::Diagnostic {
                            text,
                            kind: ilar::session::DiagnosticKind::TurnError,
                        } => lines.push(Line_::System(text.clone())),
                        // Raw thinking: a local diagnostic where the
                        // provider will not take it back, `Thinking`
                        // where it does (the chat-wire families). Either
                        // way the only
                        // account of why the turn did what it did. It
                        // was dropped here once, so a session showed
                        // thoughts while it ran and none once it was
                        // reread; `--view` is always a reread, which
                        // made the assistant look like it never thought
                        // at all. Same collapsed row the summary two
                        // arms up gets.
                        ilar::session::ContentBlock::Diagnostic {
                            text,
                            kind: ilar::session::DiagnosticKind::Local,
                        }
                        | ilar::session::ContentBlock::Thinking { text, .. } => {
                            lines.push(Line_::Thought {
                                id: restored_line_id(nested, "thought", lines.len()),
                                text: text.clone(),
                                complete: true,
                                expanded: false,
                            });
                        }
                        ilar::session::ContentBlock::Reasoning { .. }
                        | ilar::session::ContentBlock::ToolResult { .. } => {}
                    }
                }
            }
            ilar::session::SessionEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
                images,
                child_session_id,
                ..
            } => {
                // Redacted like the live row: replay is a display too,
                // and the persisted body keeps raw values by design —
                // showing them here would undo the live redaction at
                // the first reopen.
                let content = ilar::agent::redact_tool_result(
                    call_inputs
                        .get(tool_use_id)
                        .unwrap_or(&serde_json::Value::Null),
                    content,
                );
                // The same markers the live ToolFinished row appends,
                // handed over whole: the live path gives the entire
                // description, markers included, to `kept_result_detail`,
                // so bounding only the text here would keep a trailing
                // blank line the live row folds away. The stored content
                // is the full result, so this is where anything past the
                // publish site's 16 KiB streaming cut becomes readable
                // again (up to the 256 KiB keep-cap).
                //
                // And the settling itself is the live path's, rather
                // than these rules written out a second time: it also
                // clears the progress and refuses a row that has
                // already finished, which this one did not.
                crate::transcript::finish_tool_row(
                    &mut lines,
                    tool_use_id,
                    *is_error,
                    &format!("{content}{}", ilar::image::markers(images)),
                    child_session_id,
                );
            }
            ilar::session::SessionEvent::ModelChange { model, variant, .. } => {
                let selection = variant
                    .as_deref()
                    .map(|variant| format!("{model}@{variant}"))
                    .unwrap_or_else(|| model.clone());
                lines.push(Line_::System(format!("switched to {selection}")));
            }
            // The one line that keeps a killed child's transcript from
            // simply stopping.
            ilar::session::SessionEvent::TurnEnded { detail, .. } => {
                lines.push(Line_::System(detail.clone()));
            }
            // What the model was handed from memory, by count; the
            // lines themselves are in the log.
            ilar::session::SessionEvent::MemoryRecall { ids, .. } => {
                lines.push(Line_::System(memory_recall_display(ids.len())));
            }
            ilar::session::SessionEvent::Compaction { .. } => {}
            ilar::session::SessionEvent::TurnFinished { .. } => {}
        }
    }
    if liveness == Liveness::Settled {
        for line in &mut lines {
            if let Line_::Tool { id, state, .. } = line
                && *state == ToolState::Running
                && pending_question_id != Some(id.as_str())
            {
                *state = ToolState::Failed;
            }
        }
    }
    RestoredSessionView {
        lines,
        latest_usage,
        total_usage,
        total_cost,
        task_usage: ilar::session::Usage::default(),
        task_cost: Some(0.0),
        // Only the whole-session restore asks for it; a child
        // invocation's slice has no turn of its own to resume.
        resume_offer: false,
        // A slice cannot know where its log's window begins; the two
        // entry points that hold a reader fill this in.
        history_before: 0,
    }
}

/// The log rendered whole: every turn, compacted-away ones included,
/// and no handover note. An export wants the conversation that
/// happened; the screen wants the window the model kept of it, which
/// is what every other entry point here gives.
pub(crate) fn whole_log_lines(events: &[ilar::session::SessionEvent]) -> Vec<Line_> {
    restored_session_invocation_view_in(events, None, None, Liveness::Settled, Window::Whole).lines
}

pub(crate) fn restored_session_view_with_store(
    session: &ilar::session::SessionReader,
    store: &SessionStore,
    liveness: Liveness,
) -> RestoredSessionView {
    let mut view = restored_session_invocation_view(
        session.events(),
        pending_question_id(session),
        None,
        liveness,
    );
    view.history_before = session.event_base();
    let owner_session_id = session
        .meta()
        .map(|meta| meta.session_id.as_str())
        .unwrap_or_default();
    // A child of a working session is working too, near enough: its
    // parent is blocked on the call. If it is not, an open row is a
    // spinner that settles on the next result — cheaper than a ✗ that
    // nothing can take back.
    let mut counted = std::collections::HashSet::new();
    let (task_usage, task_cost) = restore_child_activity(
        &mut view.lines,
        store,
        owner_session_id,
        0,
        liveness,
        &mut counted,
    );
    view.task_usage = task_usage;
    view.task_cost = task_cost;
    // A session that stopped mid-turn can be continued from its
    // committed chain — but only the log knows it ended that way, and
    // only a question-free session can be resumed at all. (A question
    // waiting for an answer is itself an unanswered tool call, so the
    // guard is what keeps it from reading as an abort.)
    view.resume_offer = session.pending_question().is_none() && ends_mid_turn(session.events());
    view
}

/// A session's spend across its whole log — every turn, however it
/// was driven. The anchored slices cannot answer this: a turn resumed
/// by a routed notification carries a synthetic call id that anchors
/// to no row, and its spend would be counted by nobody.
fn session_own_spend(
    session: &ilar::session::SessionReader,
) -> (ilar::session::Usage, Option<f64>) {
    let mut usage = ilar::session::Usage::default();
    let mut cost = Some(0.0);
    for event in session.events() {
        if let ilar::session::SessionEvent::AssistantMessage {
            model, usage: step, ..
        } = event
        {
            accrue_usage(&mut usage, &mut cost, model, step);
        }
    }
    (usage, cost)
}

/// Returns the spend of every child it loaded: whole-log totals,
/// counted once per child session however many rows anchor it (`task`
/// plus `task_message` resumes) and whatever the digest keeps of its
/// lines. Known undercounts, deliberate: descendants whose anchor
/// rows sat in a folded digest middle, or beyond the depth cap, are
/// never loaded — for lines or for spend.
fn restore_child_activity(
    lines: &mut [Line_],
    store: &SessionStore,
    owner_session_id: &str,
    depth: usize,
    liveness: Liveness,
    counted: &mut std::collections::HashSet<String>,
) -> (ilar::session::Usage, Option<f64>) {
    let mut task_usage = ilar::session::Usage::default();
    let mut task_cost = Some(0.0);
    if depth >= 8 {
        return (task_usage, task_cost);
    }
    for line in lines {
        let Line_::Tool {
            id: parent_tool_call_id,
            child_session_id: Some(session_id),
            child_lines,
            kind,
            ..
        } = line
        else {
            continue;
        };
        let Ok(session) = store.load(session_id) else {
            continue;
        };
        if session.meta().and_then(|meta| meta.parent_id.as_deref()) != Some(owner_session_id) {
            continue;
        }
        let agent = session
            .meta()
            .map(|meta| meta.agent.clone())
            .unwrap_or_default();
        if counted.insert(session_id.clone()) {
            let (spend, cost) = session_own_spend(&session);
            add_usage(&mut task_usage, &spend);
            task_cost = add_costs(task_cost, cost);
        }
        let mut restored = restored_session_invocation_view(
            session.events(),
            pending_question_id(&session),
            Some(parent_tool_call_id),
            liveness,
        )
        .lines;
        // The agent row already shows the task prompt, so the child's
        // copy of it is dropped. A compacted child leads with its
        // handover summary instead, and the prompt sits behind it.
        let prompt = usize::from(matches!(restored.first(), Some(Line_::System(_))));
        if matches!(restored.get(prompt), Some(Line_::User(_))) {
            restored.remove(prompt);
        }
        // A settled child is a finished child: the same digest the
        // live path applies at its TurnDone — and squashing *before*
        // the recursion means grandchildren of discarded rows are
        // never loaded at all.
        if liveness == Liveness::Settled {
            crate::transcript::squash_finished_child(&mut restored);
        }
        let (grand_usage, grand_cost) = restore_child_activity(
            &mut restored,
            store,
            session_id,
            depth + 1,
            liveness,
            counted,
        );
        add_usage(&mut task_usage, &grand_usage);
        task_cost = add_costs(task_cost, grand_cost);
        // The same rule the live path applies (fc625c6): a call that has
        // a child IS a subagent call, whatever it was named. Only `task`
        // announces its agent in its input, so a restored `task_message`
        // stayed a plain tool — and a plain tool row renders "result"
        // *instead of* its children, hiding the whole second half of a
        // resumed subagent's conversation. The child session knows which
        // agent ran it.
        if matches!(kind, ToolKind::Tool) && !restored.is_empty() && !agent.is_empty() {
            *kind = ToolKind::Agent {
                name: agent,
                model: None,
            };
        }
        *child_lines = restored;
    }
    (task_usage, task_cost)
}

#[cfg(test)]
mod tests {
    /// The producer's own strings, not hand-written ones: every task
    /// outcome and both job outcomes lead with what finished and how,
    /// and the ids move to the body.
    #[test]
    fn a_task_row_leads_with_the_task_not_its_id() {
        let wrap = |first: &str, body: &str| {
            if body.is_empty() {
                format!("<task-notification>\n{first}\n</task-notification>")
            } else {
                format!(
                    "<task-notification>\n{first}\n<result>\n{body}\n</result>\n</task-notification>"
                )
            }
        };
        assert_eq!(
            super::task_notification_display(&wrap(
                "Task \"Fix tests\" completed (task_id: 7c1e2a3b-0000).",
                "all green"
            ))
            .unwrap(),
            "Fix tests completed.\nall green\ntask_id: 7c1e2a3b-0000"
        );
        assert_eq!(
            super::task_notification_display(&wrap("Task \"Fix tests\" failed: it broke", ""))
                .unwrap(),
            "Fix tests failed: it broke"
        );
        assert_eq!(
            super::task_notification_display(&wrap("Task \"Fix tests\" was cancelled.", ""))
                .unwrap(),
            "Fix tests was cancelled."
        );
        assert_eq!(
            super::task_notification_display(&wrap(
                "Task \"say \"hi\"\" stalled: no progress for 600s. It has been stopped.",
                ""
            ))
            .unwrap(),
            "say \"hi\" stalled: no progress for 600s. It has been stopped."
        );
        assert_eq!(
            super::task_notification_display(&wrap(
                "Nested task \"review the diff\" completed.",
                "no findings"
            ))
            .unwrap(),
            "nested: review the diff completed.\nno findings"
        );
        // Unknown shapes are shown as written, never dropped.
        assert_eq!(
            super::task_notification_display(&wrap("Something else entirely.", "")).unwrap(),
            "Something else entirely."
        );
        assert_eq!(
            super::tool_notification_display(
                "<tool-notification>\nBackground job job-1 (\"Run checks\") completed.\n<result>\nok\n</result>\n</tool-notification>"
            )
            .unwrap(),
            "Run checks completed.\nok\njob: job-1"
        );
        assert_eq!(
            super::tool_notification_display(
                "<tool-notification>\nBackground job job-2 (\"Run checks\") timed out after 5000ms and was stopped.\n</tool-notification>"
            )
            .unwrap(),
            "Run checks timed out after 5000ms and was stopped.\njob: job-2"
        );
    }

    use super::*;
    use ilar::session::{SessionMeta, new_id};

    /// The same log, read two ways: what a dead session left running
    /// failed with it, and what a working one left running is still
    /// running. The second is the focus view's case, and marking it ✗
    /// also cost the real result — `finish_tool_row` will not settle a
    /// Failed row.
    #[test]
    fn a_restore_only_fails_open_rows_when_nothing_is_driving_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "bash-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({ "command": "cargo test" }),
                    item_id: None,
                }],
                usage: ilar::session::Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let state = |liveness| {
            restored_session_view_with_store(&store.load(&session_id).unwrap(), &store, liveness)
                .lines
                .iter()
                .find_map(|line| match line {
                    Line_::Tool { id, state, .. } if id == "bash-1" => Some(*state),
                    _ => None,
                })
                .expect("the tool row is restored")
        };
        assert_eq!(state(Liveness::Settled), ToolState::Failed);
        assert_eq!(state(Liveness::Running), ToolState::Running);
    }

    /// A row a replay settles is the row the live path would have
    /// settled. Both used to write the rules out — the restore path's
    /// copy took the newest row with a matching id whatever its state,
    /// and never cleared the progress — so this is the drift that
    /// minted the focus view's settle bug.
    #[test]
    fn a_restored_row_settles_exactly_as_the_live_one_does() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "task-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({ "command": "cargo test" }),
                    item_id: None,
                }],
                usage: ilar::session::Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "task-1".into(),
                content: "ok\n".into(),
                is_error: false,
                images: Vec::new(),
                child_session_id: Some("child-9".into()),
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let view = restored_session_view_with_store(
            &store.load(&session_id).unwrap(),
            &store,
            Liveness::Settled,
        );
        let Some(Line_::Tool {
            state,
            progress,
            result,
            child_session_id,
            ..
        }) = view
            .lines
            .iter()
            .find(|line| matches!(line, Line_::Tool { id, .. } if id == "task-1"))
        else {
            panic!("the tool row is restored: {:?}", view.lines);
        };
        assert_eq!(*state, ToolState::Succeeded);
        // Cleared by `finish_tool_row`, which the restore path now
        // calls instead of setting the three fields it remembered.
        assert_eq!(*progress, crate::transcript::ToolProgress::None);
        assert_eq!(child_session_id.as_deref(), Some("child-9"));
        assert!(result.as_deref().unwrap_or_default().contains("ok"));
    }

    /// Replay is a display too: a secret the arguments hid must not
    /// resurface in the restored result row, though the persisted
    /// event keeps it raw by design.
    #[test]
    fn a_restored_result_is_redacted_like_the_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "svc-1".into(),
                    name: "service".into(),
                    input: serde_json::json!({
                        "name": "api",
                        "command": "run --api-key=sk-verysecretvalue serve"
                    }),
                    item_id: None,
                }],
                usage: ilar::session::Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "svc-1".into(),
                content: "started: run --api-key=sk-verysecretvalue serve".into(),
                is_error: false,
                images: Vec::new(),
                child_session_id: None,
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let restored = restored_session_view_with_store(
            &store.load(&session_id).unwrap(),
            &store,
            Liveness::Settled,
        );
        let result = restored
            .lines
            .iter()
            .find_map(|line| match line {
                Line_::Tool {
                    result: Some(result),
                    ..
                } => Some(result.clone()),
                _ => None,
            })
            .expect("the restored result row");
        assert!(
            !result.contains("sk-verysecretvalue"),
            "the secret resurfaced on replay: {result}"
        );
        assert!(result.contains("<redacted>"), "{result}");
    }

    #[test]
    fn resumed_session_restores_visible_events_and_latest_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "remember this".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        let usage = ilar::session::Usage {
            input_tokens: 120,
            output_tokens: 30,
            cache_read_input_tokens: 40,
            cache_creation_input_tokens: 0,
            input_token_accounting: Some(ilar::session::InputTokenAccounting::ExcludesCached),
        };
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![
                    ilar::session::ContentBlock::Text {
                        text: "restored answer".into(),
                    },
                    ilar::session::ContentBlock::Thinking {
                        text: "a thought from a model that summarises none".into(),
                        field: None,
                    },
                    ilar::session::ContentBlock::ReasoningSummary {
                        text: "**Reviewing restored state**\n\nDetails remain collapsed.".into(),
                        completed: true,
                    },
                    ilar::session::ContentBlock::ToolCall {
                        id: "read-1".into(),
                        name: "read".into(),
                        input: Default::default(),
                        item_id: None,
                    },
                    ilar::session::ContentBlock::ToolCall {
                        id: "task-1".into(),
                        name: "task".into(),
                        input: serde_json::json!({
                            "description": "Review restored security paths",
                            "subagent_type": "build · secure",
                        }),
                        item_id: None,
                    },
                ],
                usage,
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "task-1".into(),
                content: "review complete".into(),
                is_error: false,
                images: Vec::new(),
                child_session_id: None,
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "read-1".into(),
                content: "file contents".into(),
                is_error: false,
                images: Vec::new(),
                child_session_id: None,
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ModelChange {
                id: new_id(),
                model: "openai/gpt-5.6-sol".into(),
                variant: Some("high".into()),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let resumed = store.load(&session_id).unwrap();
        let view = restored_session_view(&resumed);
        assert_eq!(view.latest_usage, Some(usage));
        assert!(matches!(&view.lines[0], Line_::User(text) if text == "remember this"));
        assert!(matches!(&view.lines[1], Line_::Assistant(text) if text == "restored answer"));
        // Both kinds of thinking come back, in the order they were
        // written: a model's own words and a provider's summary of
        // them read the same way.
        assert!(matches!(
            &view.lines[2],
            Line_::Thought { text, complete: true, .. }
                if text.contains("summarises none")
        ));
        assert!(matches!(
            &view.lines[3],
            Line_::Thought { text, complete: true, .. }
                if text.contains("Reviewing restored state")
        ));
        assert!(matches!(
            &view.lines[4],
            Line_::Tool { id, name, arguments, state: ToolState::Succeeded, .. }
                if id == "read-1" && name == "read" && arguments.is_empty()
        ));
        assert!(matches!(
            &view.lines[5],
            Line_::Tool {
                id,
                name,
                kind: ToolKind::Agent { name: agent, .. },
                arguments,
                state: ToolState::Succeeded,
                ..
            } if id == "task-1"
                && name == "task"
                && agent == "build · secure"
                && arguments == "Review restored security paths"
        ));
        assert!(matches!(
            view.lines.last(),
            Some(Line_::System(text)) if text.contains("openai/gpt-5.6-sol")
        ));
    }

    /// A session that died mid-turn must say so when it is resumed.
    /// Raw thinking wears the same block — kept because no provider
    /// takes it back — and comes back as the thought it was, because
    /// a reader who reopens a session wants to know why as much as
    /// one who watched it happen.
    #[test]
    fn a_resumed_session_shows_why_its_turn_died() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![
                    ilar::session::ContentBlock::Diagnostic {
                        text: "first I will check the parser".into(),
                        kind: ilar::session::DiagnosticKind::Local,
                    },
                    ilar::session::ContentBlock::Diagnostic {
                        text: "turn error: provider exploded".into(),
                        kind: ilar::session::DiagnosticKind::TurnError,
                    },
                ],
                usage: ilar::session::Usage::default(),
                stop_reason: "error".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let view = restored_session_view(&store.load(&session_id).unwrap());
        let systems: Vec<&String> = view
            .lines
            .iter()
            .filter_map(|line| match line {
                Line_::System(text) => Some(text),
                _ => None,
            })
            .collect();

        assert_eq!(
            systems,
            vec!["turn error: provider exploded"],
            "{:?}",
            view.lines
        );
        // And the thinking is a thought again, collapsed like the
        // live fold draws it — not a system line, and not gone.
        let thoughts: Vec<&String> = view
            .lines
            .iter()
            .filter_map(|line| match line {
                Line_::Thought {
                    text,
                    complete: true,
                    expanded: false,
                    ..
                } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(thoughts, vec!["first I will check the parser"]);

        // Restored the way the TUI restores it: the failed turn is
        // still there to continue, so the resume is offered.
        let restored = restored_session_view_with_store(
            &store.load(&session_id).unwrap(),
            &store,
            Liveness::Settled,
        );
        assert!(restored.resume_offer, "a dead turn is resumable");
    }

    /// A model that hands back no reasoning item — a local one
    /// thinking in `<think>` tags — is the case this matters for: the
    /// diagnostic is the only account of the turn there is. Logs
    /// written before thinking was stored as a diagnostic carry the
    /// block itself, and mean the same thing.
    #[test]
    fn raw_thinking_restores_as_a_thought_whichever_shape_it_was_written_in() {
        use ilar::session::{ContentBlock, DiagnosticKind, SessionEvent, Usage};

        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "lemonade/a-local-one".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(SessionEvent::AssistantMessage {
                id: new_id(),
                model: "lemonade/a-local-one".into(),
                content: vec![
                    ContentBlock::Diagnostic {
                        text: "the user wants the article read first".into(),
                        kind: DiagnosticKind::Local,
                    },
                    ContentBlock::Thinking {
                        text: "an older log wrote it like this".into(),
                        field: None,
                    },
                    ContentBlock::Text {
                        text: "here is what I think".into(),
                    },
                ],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let view = restored_session_view(&store.load(&session_id).unwrap());
        let thoughts: Vec<&String> = view
            .lines
            .iter()
            .filter_map(|line| match line {
                Line_::Thought { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            thoughts,
            vec![
                "the user wants the article read first",
                "an older log wrote it like this"
            ],
            "{:?}",
            view.lines
        );
        // The answer is still the answer, and it reads after the
        // thinking that produced it.
        assert!(
            view.lines.iter().any(
                |line| matches!(line, Line_::Assistant(text) if text == "here is what I think")
            ),
            "{:?}",
            view.lines
        );
    }

    /// Where the log ends decides whether a resume is still on offer:
    /// a turn that never finished is, however it stopped, and a
    /// session that moved on since is not.
    #[test]
    fn a_resume_is_offered_only_while_the_log_ends_mid_turn() {
        use ilar::session::{ContentBlock, DiagnosticKind, SessionEvent, Usage};

        let assistant = |content: Vec<ContentBlock>| SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content,
            usage: Usage::default(),
            stop_reason: "error".into(),
            ts: chrono::Utc::now(),
        };
        let user = || SessionEvent::UserMessage {
            id: new_id(),
            text: "try again".into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        };
        let died = || {
            assistant(vec![ContentBlock::Diagnostic {
                text: "turn error: provider exploded".into(),
                kind: DiagnosticKind::TurnError,
            }])
        };
        let answered = || {
            assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }])
        };
        let interrupted_tool = || SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: new_id(),
            content: "Tool call interrupted before completion.".into(),
            is_error: true,
            images: Vec::new(),
            child_session_id: None,
            state: None,
            ts: chrono::Utc::now(),
        };
        let called_tool = || {
            assistant(vec![ContentBlock::ToolCall {
                id: new_id(),
                name: "bash".into(),
                input: serde_json::json!({"command": "sleep 600"}),
                item_id: None,
            }])
        };

        assert!(!ends_mid_turn(&[]));
        assert!(!ends_mid_turn(&[user()]));
        assert!(!ends_mid_turn(&[died(), user()]));
        assert!(!ends_mid_turn(&[died(), user(), answered()]));
        assert!(!ends_mid_turn(&[user(), answered()]), "the turn finished");
        assert!(ends_mid_turn(&[user(), died()]));
        // The back-filled results an interrupted turn leaves say the
        // same thing the turn error does: the chain stops here.
        assert!(ends_mid_turn(&[user(), died(), interrupted_tool()]));
        // An abort writes no turn error. Stopped while the tool ran,
        // and stopped after its result with the provider call still
        // owed, are the two shapes it leaves — and the two a
        // `MaxIterations` ending leaves too.
        assert!(
            ends_mid_turn(&[user(), called_tool()]),
            "a tool call nobody answered"
        );
        assert!(
            ends_mid_turn(&[user(), called_tool(), interrupted_tool()]),
            "a result the provider was never told about"
        );

        // A task result that lands after an interrupted turn starts no
        // turn of its own — a salvage writes it with no turn at all —
        // so it cannot be what ended one. Reading it as "the session
        // moved on" took the offer away on reopen while the live one
        // was still showing.
        let arrival = || SessionEvent::UserMessage {
            id: new_id(),
            text: "<task-notification>\nTask \"build\" completed.\n</task-notification>".into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        };
        assert!(
            ends_mid_turn(&[user(), called_tool(), interrupted_tool(), arrival()]),
            "an arrival does not end the turn it landed behind"
        );
        assert!(
            ends_mid_turn(&[user(), died(), arrival()]),
            "nor does it undo a recorded failure"
        );
        // A background job's ending rides the same delivery and is the
        // same non-event here.
        let job_arrival = || {
            SessionEvent::UserMessage {
            id: new_id(),
            text: "<tool-notification>\nBackground job job-1 (\"checks\") completed.\n</tool-notification>".into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        }
        };
        assert!(
            ends_mid_turn(&[user(), called_tool(), job_arrival()]),
            "a job ending does not end the turn it landed behind"
        );
        // A person typing is still the session moving on, which is the
        // rule this must not break.
        assert!(!ends_mid_turn(&[user(), died(), arrival(), user()]));
    }

    #[test]
    fn restored_edit_tools_carry_a_diff() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "edit-1".into(),
                    name: "edit".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "old_string": "keep\nold",
                        "new_string": "keep\nnew",
                    }),
                    item_id: None,
                }],
                usage: ilar::session::Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let view = restored_session_view(&store.load(&session_id).unwrap());
        let Some(Line_::Tool { diff, .. }) = view.lines.first() else {
            panic!("expected restored edit tool: {:?}", view.lines);
        };
        assert_eq!(
            diff.iter().map(|line| line.kind).collect::<Vec<_>>(),
            vec![
                diff::DiffKind::Context,
                diff::DiffKind::Removed,
                diff::DiffKind::Added
            ]
        );
    }

    #[test]
    fn resumed_unfinished_tools_are_marked_failed() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "unfinished".into(),
                    name: "bash".into(),
                    input: Default::default(),
                    item_id: None,
                }],
                usage: ilar::session::Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let view = restored_session_view(&store.load(&session_id).unwrap());
        assert!(matches!(
            view.lines.as_slice(),
            [Line_::Tool {
                state: ToolState::Failed,
                ..
            }]
        ));
    }

    #[test]
    fn resumed_compaction_replaces_old_history_with_the_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "obsolete history".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::Compaction {
                id: new_id(),
                summary: "decisions retained here".into(),
                kept_from: 2,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "current history".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let view = restored_session_view(&store.load(&session_id).unwrap());
        let rendered = format!("{:?}", view.lines);
        assert!(!rendered.contains("obsolete history"), "{rendered}");
        assert!(rendered.contains("decisions retained here"), "{rendered}");
        assert!(rendered.contains("current history"), "{rendered}");

        // Folded, with a click target: the handover can run to a
        // screenful, and unfolded it pushed the conversation off the
        // top of every restored session that had ever been compacted.
        let Some(Line_::Note { id, text, expanded }) = view.lines.first() else {
            panic!("the summary leads, as a note: {:?}", view.lines.first());
        };
        assert!(!expanded, "collapsed until asked");
        assert!(!id.is_empty(), "and clickable, since it has a body");
        assert_eq!(
            text.lines().next(),
            Some("transcript compacted"),
            "the headline says what happened; the summary is the body"
        );

        // The summary is off the screen while it is folded, and on it
        // once it is not.
        let now = std::time::Instant::now();
        let rows = |lines: &[Line_]| {
            crate::transcript::transcript_entry_lines(&lines[0], 80, now, now)
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let folded = rows(&view.lines);
        assert!(!folded.contains("decisions retained here"), "{folded}");
        assert!(folded.contains("click or Enter to expand"), "{folded}");
        let mut opened = view.lines.clone();
        crate::transcript::toggle_note_expansion(&mut opened, id);
        assert!(rows(&opened).contains("decisions retained here"));
    }

    /// The transcript on screen starts at the newest compaction,
    /// because that is what the model still carries. An export is the
    /// conversation the person had, and it used to stop there too —
    /// the file looked complete with its first half missing.
    #[test]
    fn an_export_carries_the_turns_the_compaction_folded() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "the first question".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::Compaction {
                id: new_id(),
                summary: "earlier: a question was asked".into(),
                kept_from: 2,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "the second question".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let reader = store.load(&session_id).unwrap();
        let view = restored_session_view(&reader);
        assert!(
            view.history_before > 0,
            "the compaction folded something away"
        );

        // The screen is unchanged: the window is still the window. The
        // reader does not even carry the folded half — a compacted
        // session loads rebased onto its active window — which is why
        // the export reads the log again rather than these events.
        let on_screen = format!("{:?}", view.lines);
        assert!(!on_screen.contains("the first question"), "{on_screen}");
        assert!(
            !format!("{:?}", reader.events()).contains("the first question"),
            "the reader is the window too"
        );

        // The export splices the folded half in front of the screen.
        let whole = store.whole_events(&session_id).unwrap();
        let mut lines = whole_log_lines(&whole[..view.history_before]);
        lines.extend(view.lines.iter().cloned());
        let exported = crate::transcript::transcript_markdown(&session_id, &lines);
        assert!(exported.contains("the first question"), "{exported}");
        assert!(exported.contains("the second question"), "{exported}");
        // And the seam is marked, once: the window's own handover note
        // says where the fold fell, and the half in front of it — which
        // folded nothing — renders none of its own.
        assert_eq!(
            exported.matches("transcript compacted").count(),
            1,
            "{exported}"
        );
    }

    /// A child session compacts like any other, and `kept_from` indexes
    /// the whole log while the nested view is a slice starting at the
    /// invocation. The guard that skipped compaction for nested views
    /// dropped the child's summary marker entirely and left its
    /// compacted-away turns on screen.
    #[test]
    fn a_child_timeline_honours_its_own_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let child_id = new_id();
        let mut child = store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(new_id()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        let user = |text: &str| ilar::session::SessionEvent::UserMessage {
            id: new_id(),
            text: text.into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        };
        // Compaction cuts at a turn boundary and carries the invocation
        // link with its user message, so the surviving window opens on
        // the invocation this view is keyed to — the one arrangement
        // that puts a Compaction inside a nested slice.
        child.append(user("child obsolete history")).unwrap();
        child
            .append(ilar::session::SessionEvent::SubagentInvocation {
                id: new_id(),
                parent_tool_call_id: "task-restore".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child.append(user("child current history")).unwrap();
        child
            .append(ilar::session::SessionEvent::Compaction {
                id: new_id(),
                summary: "child decisions retained".into(),
                kept_from: 2,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(child);

        let child = store.load(&child_id).unwrap();
        let view = restored_session_invocation_view(
            child.events(),
            pending_question_id(&child),
            Some("task-restore"),
            Liveness::Settled,
        );
        let rendered = format!("{:?}", view.lines);
        assert!(rendered.contains("child decisions retained"), "{rendered}");
        assert!(rendered.contains("child current history"), "{rendered}");
        assert!(!rendered.contains("child obsolete history"), "{rendered}");
    }

    /// Inside a subagent's timeline a user message is the parent's
    /// `task_message`, not anything the person typed. Labelling it
    /// `you` there claimed they had written something they never saw.
    #[test]
    fn a_parents_message_in_a_child_timeline_is_not_labelled_you() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let child_id = new_id();
        let mut child = store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(new_id()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::SubagentInvocation {
                id: new_id(),
                parent_tool_call_id: "task-1".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "also check the picker".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(child);

        let reader = store.load(&child_id).unwrap();
        // The child's own timeline: the message came from whoever
        // delegated the work.
        let nested = restored_session_invocation_view(
            reader.events(),
            None,
            Some("task-1"),
            Liveness::Settled,
        );
        assert!(
            nested.lines.iter().any(
                |line| matches!(line, Line_::Incoming(text) if text == "also check the picker")
            ),
            "{:?}",
            nested.lines
        );

        // The same log read as a session in its own right is the
        // person's: `--view` on a child is still a transcript, and the
        // label belongs to the timeline, not to the event.
        let root = restored_session_invocation_view(reader.events(), None, None, Liveness::Settled);
        assert!(
            root.lines
                .iter()
                .any(|line| matches!(line, Line_::User(text) if text == "also check the picker")),
            "{:?}",
            root.lines
        );
    }

    /// The live rule from fc625c6, on the restore path: a call that has
    /// a child is a subagent call. Only `task` names its agent in its
    /// input, so a restored `task_message` stayed a plain tool — and a
    /// plain tool row draws "result" *instead of* its children. Since a
    /// resumed subagent's `task` slice ends at the resume, the whole
    /// second half of its conversation was loaded and never drawn.
    #[test]
    fn a_restored_task_message_becomes_the_agent_it_resumed_and_draws_its_children() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let parent_id = new_id();
        let child_id = new_id();
        let mut child = store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        let mut turn = |call: &str, prompt: &str, answer: &str| {
            child
                .append(ilar::session::SessionEvent::SubagentInvocation {
                    id: new_id(),
                    parent_tool_call_id: call.into(),
                    ts: chrono::Utc::now(),
                })
                .unwrap();
            child
                .append(ilar::session::SessionEvent::UserMessage {
                    id: new_id(),
                    text: prompt.into(),
                    images: Vec::new(),
                    ts: chrono::Utc::now(),
                })
                .unwrap();
            child
                .append(ilar::session::SessionEvent::AssistantMessage {
                    id: new_id(),
                    model: "zai/glm-4.7".into(),
                    content: vec![ilar::session::ContentBlock::Text {
                        text: answer.into(),
                    }],
                    usage: Default::default(),
                    stop_reason: "end_turn".into(),
                    ts: chrono::Utc::now(),
                })
                .unwrap();
        };
        turn("task-1", "Inspect rendering", "the first half");
        turn("msg-1", "keep going", "the second half");
        drop(child);

        let mut parent = store
            .create(SessionMeta {
                session_id: parent_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        let mut call = |id: &str, name: &str, input: serde_json::Value| {
            parent
                .append(ilar::session::SessionEvent::AssistantMessage {
                    id: new_id(),
                    model: "zai/glm-4.7".into(),
                    content: vec![ilar::session::ContentBlock::ToolCall {
                        id: id.into(),
                        name: name.into(),
                        input,
                        item_id: None,
                    }],
                    usage: Default::default(),
                    stop_reason: "tool_use".into(),
                    ts: chrono::Utc::now(),
                })
                .unwrap();
            parent
                .append(ilar::session::SessionEvent::ToolResult {
                    id: new_id(),
                    tool_use_id: id.into(),
                    content: "done".into(),
                    is_error: false,
                    images: Vec::new(),
                    child_session_id: Some(child_id.clone()),
                    state: None,
                    ts: chrono::Utc::now(),
                })
                .unwrap();
        };
        call(
            "task-1",
            "task",
            serde_json::json!({"description": "Inspect rendering", "subagent_type": "explore"}),
        );
        call(
            "msg-1",
            "task_message",
            serde_json::json!({"task_id": "task-1", "message": "keep going"}),
        );
        drop(parent);

        let restored = restored_session_view_with_store(
            &store.load(&parent_id).unwrap(),
            &store,
            Liveness::Settled,
        );
        let resumed = restored
            .lines
            .iter()
            .find_map(|line| match line {
                Line_::Tool {
                    id,
                    kind,
                    child_lines,
                    ..
                } if id == "msg-1" => Some((kind, child_lines)),
                _ => None,
            })
            .expect("the task_message row");
        assert!(
            matches!(resumed.0, ToolKind::Agent { name, .. } if name == "explore"),
            "{:?}",
            resumed.0
        );
        assert!(
            resumed
                .1
                .iter()
                .any(|line| matches!(line, Line_::Assistant(text) if text == "the second half"))
        );

        // And it reaches the screen once opened: a plain tool row draws
        // its result in place of all of this, however far it is opened.
        let mut lines = restored.lines.clone();
        for line in &mut lines {
            if let Line_::Tool { expanded, .. } = line {
                *expanded = true;
            }
        }
        let groups = std::collections::HashSet::new();
        let now = std::time::Instant::now();
        let rendered: Vec<String> = crate::transcript::transcript_entries(&lines, &groups)
            .iter()
            .flat_map(|entry| {
                crate::transcript::transcript_entry_rows(entry, &groups, 100, now, now, false)
            })
            .map(|row| crate::text::tests::rendered_text(&row.line))
            .collect();
        assert!(
            rendered.iter().any(|line| line.contains("the second half")),
            "{rendered:?}"
        );
    }

    #[test]
    fn restored_agent_loads_its_child_timeline() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let parent_id = new_id();
        let child_id = new_id();
        let mut child = store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::SubagentInvocation {
                id: new_id(),
                parent_tool_call_id: "task-restore".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "Inspect rendering".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::Text {
                    text: "Nested restored answer".into(),
                }],
                usage: Default::default(),
                stop_reason: "end_turn".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::SubagentInvocation {
                id: new_id(),
                parent_tool_call_id: "later-task".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "Later request".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::Text {
                    text: "Later answer".into(),
                }],
                usage: Default::default(),
                stop_reason: "end_turn".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(child);

        let mut parent = store
            .create(SessionMeta {
                session_id: parent_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        parent
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "task-restore".into(),
                    name: "task".into(),
                    input: serde_json::json!({
                        "description": "Inspect rendering",
                        "subagent_type": "explore"
                    }),
                    item_id: None,
                }],
                usage: Default::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        parent
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "task-restore".into(),
                content: "Nested restored answer".into(),
                is_error: false,
                images: Vec::new(),
                child_session_id: Some(child_id),
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(parent);

        let restored = restored_session_view_with_store(
            &store.load(&parent_id).unwrap(),
            &store,
            Liveness::Settled,
        );
        let child_lines = restored.lines.iter().find_map(|line| match line {
            Line_::Tool { child_lines, .. } => Some(child_lines),
            _ => None,
        });

        assert!(child_lines.is_some_and(|lines| {
            lines.iter().any(
                |line| matches!(line, Line_::Assistant(text) if text == "Nested restored answer"),
            ) && !lines
                .iter()
                .any(|line| matches!(line, Line_::Assistant(text) if text == "Later answer"))
        }));
    }

    /// Past 16 KiB the same call used to say two different things
    /// depending on when you looked: the live row ended at the publish
    /// bound and a reopened session read the whole result back from
    /// the log. The bound is the keep cap now, so both keep everything
    /// up to it — and both still cut the raw representation before
    /// expanding tabs.
    #[test]
    fn an_over_long_result_reads_the_same_live_and_restored() {
        let raw = "\tname\tvalue\n".repeat(4_000);
        // The window where the two used to disagree: past the old
        // publish bound, under the keep cap.
        assert!(raw.chars().count() > ilar::text::MAX_DETAIL_CHARS);
        assert!(raw.chars().count() < ilar::text::MAX_RESULT_CHARS);
        let (live, restored) = live_and_restored(&raw, &[]);
        assert_eq!(live, restored);
        assert!(!live.contains("output truncated"), "cut under the cap");
        assert_eq!(live.lines().count(), 4_000);
        assert!(!live.contains('\t'), "tabs are expanded for display");

        // Images ride along on the same string, past the text either
        // path kept.
        let image = ilar::session::ImageContent::png(&[0u8; 128]);
        let markers = ilar::image::markers(std::slice::from_ref(&image));
        let (live, restored) = live_and_restored(&raw, std::slice::from_ref(&image));
        assert!(live.ends_with(&markers), "{live:?}");
        assert!(restored.ends_with(&markers), "{restored:?}");
        // Under the cut, parity is exact. A description that ends in a
        // newline is the common case, and it is where the two used to
        // differ by a blank line.
        let (live, restored) = live_and_restored("one\ttwo\n", std::slice::from_ref(&image));
        assert_eq!(live, restored);
        assert!(live.ends_with(&markers));
    }

    /// The same tool result down both paths: the live row settles what
    /// the agent loop published, the restored one is rebuilt from the
    /// log. Returns (live, restored).
    fn live_and_restored(raw: &str, images: &[ilar::session::ImageContent]) -> (String, String) {
        // What the agent loop publishes and stores is the same string.
        let published = format!(
            "{}{}",
            ilar::text::bounded_result(raw),
            ilar::image::markers(images)
        );
        let mut live = vec![crate::transcript::new_seeded_tool_row(
            "call-1",
            "g".into(),
            "bash",
            crate::transcript::ToolSeed {
                argument_detail: "{}".into(),
                ..Default::default()
            },
        )];
        crate::transcript::finish_tool_row(&mut live, "call-1", false, &published, &None);
        let Some(Line_::Tool {
            result: Some(live_result),
            ..
        }) = live.first()
        else {
            panic!("the live row must have settled");
        };

        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({ "command": "cat table.tsv" }),
                    item_id: None,
                }],
                usage: Default::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "call-1".into(),
                content: raw.to_string(),
                is_error: false,
                images: images.to_vec(),
                child_session_id: None,
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let restored = restored_session_view(&store.load(&session_id).unwrap());
        let Some(Line_::Tool {
            result: Some(restored_result),
            ..
        }) = restored
            .lines
            .iter()
            .find(|line| matches!(line, Line_::Tool { .. }))
        else {
            panic!("expected a restored tool row: {:?}", restored.lines);
        };
        (live_result.clone(), restored_result.clone())
    }

    #[test]
    fn restored_image_bearing_tool_results_show_the_same_markers_the_live_row_did() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.6v".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.6v".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "read-image".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "shot.png" }),
                    item_id: None,
                }],
                usage: Default::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        let image = ilar::session::ImageContent::png(&vec![0u8; 12_600]);
        session
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "read-image".into(),
                content: "shot.png: image/png, 640x480 — the image itself follows".into(),
                is_error: false,
                images: vec![image.clone()],
                child_session_id: None,
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        let restored = restored_session_view(&store.load(&session_id).unwrap());
        let Some(Line_::Tool { result, .. }) = restored
            .lines
            .iter()
            .find(|line| matches!(line, Line_::Tool { .. }))
        else {
            panic!("expected a restored tool row: {:?}", restored.lines);
        };
        let result = result.as_deref().unwrap_or_default();
        assert!(
            result.starts_with("shot.png: image/png, 640x480 — the image itself follows"),
            "the description still leads: {result:?}"
        );
        // Byte-identical to what the live ToolFinished row appended.
        assert!(
            result.ends_with(&ilar::image::markers(std::slice::from_ref(&image))),
            "restored rows must carry the live markers: {result:?}"
        );
        assert!(result.contains("[image: png · 12.3 KiB]"), "{result:?}");
    }

    /// Click-target id of an expandable line, if it is one.
    fn expandable_id(line: &Line_) -> Option<&str> {
        match line {
            Line_::Thought { id, .. }
            | Line_::Task { id, .. }
            | Line_::Job { id, .. }
            | Line_::Note { id, .. } => Some(id.as_str()),
            _ => None,
        }
    }

    #[test]
    fn restored_nested_thoughts_are_not_click_targets() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let parent_id = new_id();
        let child_id = new_id();
        let mut child = store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::SubagentInvocation {
                id: new_id(),
                parent_tool_call_id: "task-nested".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "Inspect rendering".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![
                    ilar::session::ContentBlock::ReasoningSummary {
                        text: "Nested reasoning".into(),
                        completed: true,
                    },
                    ilar::session::ContentBlock::Text {
                        text: "Nested restored answer".into(),
                    },
                ],
                usage: Default::default(),
                stop_reason: "end_turn".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        child
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "<tool-notification>\nBackground job build finished.\n</tool-notification>"
                    .into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(child);

        let mut parent = store
            .create(SessionMeta {
                session_id: parent_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        parent
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![
                    ilar::session::ContentBlock::ReasoningSummary {
                        text: "Top-level reasoning".into(),
                        completed: true,
                    },
                    ilar::session::ContentBlock::ToolCall {
                        id: "task-nested".into(),
                        name: "task".into(),
                        input: serde_json::json!({
                            "description": "Inspect rendering",
                            "subagent_type": "explore"
                        }),
                        item_id: None,
                    },
                ],
                usage: Default::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        parent
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "task-nested".into(),
                content: "Nested restored answer".into(),
                is_error: false,
                images: Vec::new(),
                child_session_id: Some(child_id),
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(parent);

        let restored = restored_session_view_with_store(
            &store.load(&parent_id).unwrap(),
            &store,
            Liveness::Settled,
        );
        let Some(Line_::Tool { child_lines, .. }) = restored
            .lines
            .iter()
            .find(|line| matches!(line, Line_::Tool { .. }))
        else {
            panic!("expected a restored agent tool: {:?}", restored.lines);
        };
        let nested_ids: Vec<&str> = child_lines.iter().filter_map(expandable_id).collect();
        assert_eq!(
            nested_ids.len(),
            2,
            "the child timeline keeps its thought and its job note: {child_lines:?}"
        );
        assert!(
            nested_ids.iter().all(|id| id.is_empty()),
            "nested restored rows are previews, not click targets: {nested_ids:?}"
        );

        // Top level keeps working, unique ids: the click handler scans
        // only these, so each one must match exactly one line.
        let top_ids: Vec<&str> = restored.lines.iter().filter_map(expandable_id).collect();
        assert_eq!(top_ids.len(), 1, "{:?}", restored.lines);
        for id in &top_ids {
            assert!(!id.is_empty());
            assert_eq!(
                restored
                    .lines
                    .iter()
                    .filter(|line| expandable_id(line) == Some(*id))
                    .count(),
                1,
                "id {id} must toggle only itself"
            );
        }

        // Rendered, an expanded agent row offers no nested thought target.
        let mut lines = restored.lines.clone();
        for line in &mut lines {
            if let Line_::Tool { expanded, .. } = line {
                *expanded = true;
            }
        }
        let groups = std::collections::HashSet::new();
        let now = std::time::Instant::now();
        let targets: Vec<String> = crate::transcript::transcript_entries(&lines, &groups)
            .iter()
            .flat_map(|entry| {
                crate::transcript::transcript_entry_rows(entry, &groups, 100, now, now, false)
            })
            .filter_map(|row| match row.target {
                Some(crate::transcript::TranscriptHitTarget::Thought(id)) => Some(id),
                _ => None,
            })
            .collect();
        assert!(
            targets.iter().all(|id| top_ids.contains(&id.as_str())),
            "only top-level thoughts are clickable: {targets:?}"
        );
    }

    /// A session launched in `cwd` with `turns` exchanges in it, left
    /// as this directory's last session — which is what `create` writes
    /// down.
    fn session_here(store: &SessionStore, cwd: &std::path::Path, turns: usize) -> String {
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: Some(cwd.to_path_buf()),
            })
            .unwrap();
        for turn in 0..turns {
            session
                .append(ilar::session::SessionEvent::UserMessage {
                    id: new_id(),
                    text: format!("ask {turn}"),
                    images: Vec::new(),
                    ts: chrono::Utc::now(),
                })
                .unwrap();
            session
                .append(ilar::session::SessionEvent::AssistantMessage {
                    id: new_id(),
                    model: "zai/glm-4.7".into(),
                    content: vec![ilar::session::ContentBlock::Text {
                        text: format!("answer {turn}"),
                    }],
                    usage: ilar::session::Usage::default(),
                    stop_reason: "end_turn".into(),
                    ts: chrono::Utc::now(),
                })
                .unwrap();
        }
        session_id
    }

    /// The offer is the pointer's answer or nothing: a directory that
    /// has a session here shows it, and one that does not shows
    /// nothing rather than paying for a listing to find a stranger's.
    #[test]
    fn an_offer_is_made_where_a_session_was_left_and_nowhere_else() {
        let state = tempfile::tempdir().unwrap();
        let store = SessionStore::new(state.path().to_path_buf());
        let work = tempfile::tempdir().unwrap();
        let here = std::fs::canonicalize(work.path()).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now();

        assert!(
            ghost_offer(&store, &here, now).is_none(),
            "nothing has happened in this directory yet"
        );

        let session_id = session_here(&store, &here, 2);

        let offer = ghost_offer(&store, &here, now).expect("the session here is on offer");
        assert_eq!(offer.session_id, session_id);
        assert_eq!(offer.title, "ask 0", "named by its opening prompt");
        assert!(
            offer.header.starts_with("previous session here: ask 0 · "),
            "{}",
            offer.header
        );
        assert!(
            !offer.header.contains("Enter"),
            "the keys are said in the prompt, not here: {}",
            offer.header
        );
        assert!(
            format!("{:?}", offer.lines).contains("answer 1"),
            "the tail is what it was last doing"
        );

        assert!(
            ghost_offer(&store, elsewhere.path(), now).is_none(),
            "another directory's session is not this directory's offer"
        );

        // Somebody opened `ilar` here and closed it again: the empty
        // session is swept and takes the pointer with it. The offer is
        // still the session before it — through the listing, which
        // repairs the pointer on the way.
        let opened_and_quit = session_here(&store, &here, 0);
        assert!(store.remove_if_empty(&opened_and_quit, &state.path().join("outbox")));
        assert_eq!(
            ghost_offer(&store, &here, now).map(|offer| offer.session_id),
            Some(session_id),
            "an open-and-quit must not cost the directory its offer"
        );
    }

    /// A session nobody typed in has no title and is no offer: it is
    /// the empty session a previous launch left behind, and offering to
    /// resume nothing is worse than offering nothing.
    #[test]
    fn an_untouched_session_is_not_offered() {
        let state = tempfile::tempdir().unwrap();
        let store = SessionStore::new(state.path().to_path_buf());
        let work = tempfile::tempdir().unwrap();
        let here = std::fs::canonicalize(work.path()).unwrap();

        session_here(&store, &here, 0);

        assert!(ghost_offer(&store, &here, std::time::SystemTime::now()).is_none());
    }

    /// A last record bigger than the tail window leaves the window
    /// with no whole record in it — a big tool result, an image-bearing
    /// message. The offer is still made: a log this size is read
    /// properly rather than shown empty.
    #[test]
    fn a_record_bigger_than_the_window_is_still_offered() {
        let state = tempfile::tempdir().unwrap();
        let store = SessionStore::new(state.path().to_path_buf());
        let work = tempfile::tempdir().unwrap();
        let here = std::fs::canonicalize(work.path()).unwrap();

        let session_id = session_here(&store, &here, 1);
        let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: new_id(),
                text: format!(
                    "one enormous message {}",
                    "x".repeat(GHOST_TAIL_BYTES as usize)
                ),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        assert!(
            ilar::session::tail_events(&store, &session_id, GHOST_TAIL_BYTES, GHOST_TAIL_EVENTS)
                .unwrap()
                .is_empty(),
            "the window holds no whole record, which is the case under test"
        );

        let offer = ghost_offer(&store, &here, std::time::SystemTime::now()).expect("on offer");
        assert!(
            format!("{:?}", offer.lines).contains("one enormous message"),
            "the offer shows what the session was last doing"
        );
    }

    /// Bounded from the end, whatever the log weighs: the offer is a
    /// couple of screenfuls of the session's ending, and a long
    /// conversation costs it nothing.
    #[test]
    fn an_offer_is_bounded_however_long_the_session() {
        let state = tempfile::tempdir().unwrap();
        let store = SessionStore::new(state.path().to_path_buf());
        let work = tempfile::tempdir().unwrap();
        let here = std::fs::canonicalize(work.path()).unwrap();

        session_here(&store, &here, 400);

        let offer = ghost_offer(&store, &here, std::time::SystemTime::now()).expect("on offer");
        assert!(
            offer.lines.len() <= GHOST_LINES,
            "{} lines is not a couple of screenfuls",
            offer.lines.len()
        );
        assert!(
            format!("{:?}", offer.lines).contains("answer 399"),
            "and it is the ending that is kept"
        );
        assert!(
            !format!("{:?}", offer.lines).contains("ask 0\""),
            "not the beginning"
        );
    }
}
