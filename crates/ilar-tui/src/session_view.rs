//! Rebuilding transcript state from a persisted session.
//!
//! Replays session events into `Line_`s, and parses the notification
//! envelopes background work writes into the transcript so they render
//! as task/job rows rather than as things the user said.

use ilar::session::SessionStore;

use crate::diff;
use crate::transcript::{Line_, ToolKind, ToolProgress, ToolState, kept_result_detail};

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

pub(crate) fn task_notification_display(text: &str) -> Option<String> {
    notification_display(text, "task-notification", normalize_task_notification)
}

fn normalize_task_notification(first: &str) -> String {
    let Some(first) = first.strip_prefix("Task \"") else {
        return first.to_string();
    };
    for separator in [
        "\" completed.",
        "\" failed:",
        "\" was cancelled.",
        "\" was aborted.",
        "\" stalled:",
    ] {
        if let Some(index) = first.rfind(separator) {
            return format!("{} {}", &first[..index], &first[index + 2..]);
        }
    }
    format!("Task \"{first}")
}

pub(crate) fn tool_notification_display(text: &str) -> Option<String> {
    notification_display(text, "tool-notification", |first| {
        first
            .strip_prefix("Background job ")
            .unwrap_or(first)
            .to_string()
    })
}

fn notification_display(
    text: &str,
    tag: &str,
    normalize_first: impl FnOnce(&str) -> String,
) -> Option<String> {
    let opening = format!("<{tag}>\n");
    let closing = format!("\n</{tag}>");
    let inner = text.strip_prefix(&opening)?.strip_suffix(&closing)?;
    let (first, body) = inner.split_once('\n').unwrap_or((inner, ""));
    let body = body
        .strip_prefix("<result>\n")
        .and_then(|body| body.strip_suffix("\n</result>"))
        .unwrap_or(body);
    let first = normalize_first(first);
    if body.is_empty() {
        Some(first)
    } else {
        Some(format!("{first}\n{body}"))
    }
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
    restored_session_invocation_view(session, None, Liveness::Settled)
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

fn restored_session_invocation_view(
    session: &ilar::session::SessionReader,
    parent_tool_call_id: Option<&str>,
    liveness: Liveness,
) -> RestoredSessionView {
    let nested = parent_tool_call_id.is_some();
    let all_events = session.events();
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
    for (index, event) in events.iter().enumerate() {
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
    let mut lines = summary
        .map(|summary| vec![Line_::System(format!("transcript compacted\n{summary}"))])
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
                        None => lines.push(Line_::User(crate::transcript::user_text_with_images(
                            text, images,
                        ))),
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
                            lines.push(Line_::Tool {
                                id: id.clone(),
                                group_id: format!("{message_id}:{tool_run}"),
                                name: name.clone(),
                                kind,
                                arguments,
                                argument_detail: ilar::agent::tool_argument_detail(name, input),
                                diff: diff::tool_diff_value(name, input),
                                tail: String::new(),
                                result: None,
                                state: ToolState::Running,
                                progress: ToolProgress::None,
                                expanded: false,
                                full: false,
                                child_lines: Vec::new(),
                                child_group: 0,
                                child_running: false,
                                child_session_id: None,
                            });
                        }
                        // Why the turn stopped. Without it a resumed
                        // session that died mid-turn just ends, and the
                        // reader is left guessing at the silence.
                        ilar::session::ContentBlock::Diagnostic {
                            text,
                            kind: ilar::session::DiagnosticKind::TurnError,
                        } => lines.push(Line_::System(text.clone())),
                        ilar::session::ContentBlock::Thinking { .. }
                        | ilar::session::ContentBlock::Reasoning { .. }
                        | ilar::session::ContentBlock::Diagnostic { .. }
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
                if let Some((state, result, stored_child_session)) =
                    lines.iter_mut().rev().find_map(|line| match line {
                        Line_::Tool {
                            id,
                            state,
                            result,
                            child_session_id,
                            ..
                        } if id == tool_use_id => Some((state, result, child_session_id)),
                        _ => None,
                    })
                {
                    *state = if *is_error {
                        ToolState::Failed
                    } else {
                        ToolState::Succeeded
                    };
                    // Redacted like the live row: replay is a display
                    // too, and the persisted body keeps raw values by
                    // design — showing them here would undo the live
                    // redaction at the first reopen.
                    let content = ilar::agent::redact_tool_result(
                        call_inputs
                            .get(tool_use_id)
                            .unwrap_or(&serde_json::Value::Null),
                        content,
                    );
                    // The same markers the live ToolFinished row appended,
                    // from the same helper — and kept the way that row
                    // keeps them: the live path hands the whole
                    // description, markers included, to
                    // `kept_result_detail`, so a restored transcript that
                    // bounded only the text would keep a trailing blank
                    // line the live row folds away. The stored content is
                    // the full result, so this is where anything past the
                    // publish site's 16 KiB streaming cut becomes
                    // readable again (up to the 256 KiB keep-cap).
                    *result = Some(kept_result_detail(&format!(
                        "{content}{}",
                        ilar::image::markers(images)
                    )));
                    *stored_child_session = child_session_id.clone();
                }
            }
            ilar::session::SessionEvent::ModelChange { model, variant, .. } => {
                let selection = variant
                    .as_deref()
                    .map(|variant| format!("{model}@{variant}"))
                    .unwrap_or_else(|| model.clone());
                lines.push(Line_::System(format!("switched to {selection}")));
            }
            ilar::session::SessionEvent::Compaction { .. } => {}
        }
    }
    let pending_question_id = session
        .pending_question()
        .map(|pending| pending.tool_call_id.as_str());
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
    }
}

pub(crate) fn restored_session_view_with_store(
    session: &ilar::session::SessionReader,
    store: &SessionStore,
    liveness: Liveness,
) -> RestoredSessionView {
    let mut view = restored_session_invocation_view(session, None, liveness);
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
            model,
            usage: step,
            ..
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
        let mut restored =
            restored_session_invocation_view(&session, Some(parent_tool_call_id), liveness).lines;
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
        let (grand_usage, grand_cost) =
            restore_child_activity(&mut restored, store, session_id, depth + 1, liveness, counted);
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

        let restored =
            restored_session_view_with_store(
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
                        text: "hidden thought".into(),
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
        assert!(matches!(
            &view.lines[2],
            Line_::Thought { text, complete: true, .. }
                if text.contains("Reviewing restored state")
        ));
        assert!(matches!(
            &view.lines[3],
            Line_::Tool { id, name, arguments, state: ToolState::Succeeded, .. }
                if id == "read-1" && name == "read" && arguments.is_empty()
        ));
        assert!(matches!(
            &view.lines[4],
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
        let rendered = format!("{:?}", view.lines);
        assert!(!rendered.contains("hidden thought"), "{rendered}");
    }

    /// A session that died mid-turn must say so when it is resumed.
    /// Raw thinking wears the same block — kept because no provider
    /// takes it back — and stays out of the transcript.
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
                        text: "a thought nobody needs to reread".into(),
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

        let view = restored_session_invocation_view(
            &store.load(&child_id).unwrap(),
            Some("task-restore"),
            Liveness::Settled,
        );
        let rendered = format!("{:?}", view.lines);
        assert!(rendered.contains("child decisions retained"), "{rendered}");
        assert!(rendered.contains("child current history"), "{rendered}");
        assert!(!rendered.contains("child obsolete history"), "{rendered}");
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

    /// Over 16 KiB the publish site has already cut what the live row
    /// gets, but restore reads the whole redacted result from the log
    /// and must stop destroying it: the restored row keeps everything
    /// (up to the 256 KiB keep-cap) behind the full toggle, agreeing
    /// with the live row on every character live was allowed to keep —
    /// both still cut the raw representation before expanding tabs.
    #[test]
    fn an_over_long_result_survives_restore_where_live_was_cut() {
        let raw = "\tname\tvalue\n".repeat(4_000);
        assert!(raw.chars().count() > ilar::text::MAX_DETAIL_CHARS);
        let (live, restored) = live_and_restored(&raw, &[]);
        // The live row still ends at the publish-site cut…
        assert!(live.ends_with("… output truncated"), "{live:?}");
        assert!(!live.contains('\t'), "tabs are expanded for display");
        // …while the restored row keeps the whole result…
        assert!(!restored.contains("output truncated"), "{restored:?}");
        assert_eq!(restored.lines().count(), 4_000);
        assert!(!restored.contains('\t'), "tabs are expanded for display");
        // …and the two agree on everything the live row kept.
        let shared = live
            .strip_suffix("… output truncated")
            .unwrap()
            .trim_end_matches('\n');
        assert!(restored.starts_with(shared), "{live:?} vs {restored:?}");

        // Images ride along on the same string; over the cut, both
        // paths now keep their markers past the truncated text.
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
            ilar::text::bounded_detail(raw),
            ilar::image::markers(images)
        );
        let mut live = vec![Line_::Tool {
            id: "call-1".into(),
            group_id: "g".into(),
            name: "bash".into(),
            kind: ToolKind::Tool,
            arguments: String::new(),
            argument_detail: "{}".into(),
            diff: Vec::new(),
            tail: String::new(),
            result: None,
            state: ToolState::Running,
            progress: ToolProgress::None,
            expanded: false,
            full: false,
            child_lines: Vec::new(),
            child_group: 0,
            child_running: false,
            child_session_id: None,
        }];
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
            Line_::Thought { id, .. } | Line_::Task { id, .. } | Line_::Job { id, .. } => {
                Some(id.as_str())
            }
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
}
