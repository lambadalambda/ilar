//! Rebuilding transcript state from a persisted session.
//!
//! Replays session events into `Line_`s, and parses the notification
//! envelopes background work writes into the transcript so they render
//! as task/job rows rather than as things the user said.

use ilar::session::SessionStore;

use crate::diff;
use crate::text::bounded_detail;
use crate::transcript::{Line_, ToolKind, ToolProgress, ToolState};

pub(crate) struct RestoredSessionView {
    pub(crate) lines: Vec<Line_>,
    pub(crate) latest_usage: Option<ilar::session::Usage>,
    pub(crate) total_usage: ilar::session::Usage,
    /// `None` once any step lacked pricing (custom or plan-only model).
    pub(crate) total_cost: Option<f64>,
}

/// Fold one step's usage into session totals; unknown pricing poisons the
/// dollar total (tokens keep accumulating).
pub(crate) fn accrue_usage(
    total: &mut ilar::session::Usage,
    cost: &mut Option<f64>,
    model: &str,
    usage: &ilar::session::Usage,
) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.cache_read_input_tokens = total
        .cache_read_input_tokens
        .saturating_add(usage.cache_read_input_tokens);
    total.cache_creation_input_tokens = total
        .cache_creation_input_tokens
        .saturating_add(usage.cache_creation_input_tokens);
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

pub(crate) fn restored_session_view(session: &ilar::session::SessionReader) -> RestoredSessionView {
    restored_session_invocation_view(session, None)
}

fn restored_session_invocation_view(
    session: &ilar::session::SessionReader,
    parent_tool_call_id: Option<&str>,
) -> RestoredSessionView {
    let all_events = session.events();
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
                    lines: Vec::new(),
                    latest_usage: None,
                    total_usage: ilar::session::Usage::default(),
                    total_cost: Some(0.0),
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
            &all_events[start + 1..end]
        }
        None => all_events,
    };
    let mut cut = 0usize;
    let mut summary = None;
    for (index, event) in events.iter().enumerate() {
        if parent_tool_call_id.is_some() {
            continue;
        }
        if let ilar::session::SessionEvent::Compaction {
            kept_from,
            summary: current,
            ..
        } = event
        {
            cut = (*kept_from).min(index).max(cut);
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
                        id: format!("note:restored:{}", lines.len()),
                        text,
                        expanded: false,
                    }),
                    None => match tool_notification_display(text) {
                        Some(text) => lines.push(Line_::Job {
                            id: format!("note:restored:{}", lines.len()),
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
                                id: format!("thought:restored:{}", lines.len()),
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
                        ilar::session::ContentBlock::Thinking { .. }
                        | ilar::session::ContentBlock::Reasoning { .. }
                        | ilar::session::ContentBlock::ProviderReplay { .. }
                        | ilar::session::ContentBlock::Diagnostic { .. }
                        | ilar::session::ContentBlock::ToolResult { .. } => {}
                    }
                }
            }
            ilar::session::SessionEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
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
                    *result = Some(bounded_detail(content));
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
    for line in &mut lines {
        if let Line_::Tool { id, state, .. } = line
            && *state == ToolState::Running
            && pending_question_id != Some(id.as_str())
        {
            *state = ToolState::Failed;
        }
    }
    RestoredSessionView {
        lines,
        latest_usage,
        total_usage,
        total_cost,
    }
}

pub(crate) fn restored_session_view_with_store(
    session: &ilar::session::SessionReader,
    store: &SessionStore,
) -> RestoredSessionView {
    let mut view = restored_session_view(session);
    let owner_session_id = session
        .meta()
        .map(|meta| meta.session_id.as_str())
        .unwrap_or_default();
    restore_child_activity(&mut view.lines, store, owner_session_id, 0);
    view
}

fn restore_child_activity(
    lines: &mut [Line_],
    store: &SessionStore,
    owner_session_id: &str,
    depth: usize,
) {
    if depth >= 8 {
        return;
    }
    for line in lines {
        let Line_::Tool {
            id: parent_tool_call_id,
            child_session_id: Some(session_id),
            child_lines,
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
        let mut restored =
            restored_session_invocation_view(&session, Some(parent_tool_call_id)).lines;
        if matches!(restored.first(), Some(Line_::User(_))) {
            restored.remove(0);
        }
        restore_child_activity(&mut restored, store, session_id, depth + 1);
        *child_lines = restored;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ilar::session::{SessionMeta, new_id};

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
                        signature: None,
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
                child_session_id: Some(child_id),
                state: None,
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(parent);

        let restored = restored_session_view_with_store(&store.load(&parent_id).unwrap(), &store);
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
}
