//! Session events → wire JSON.
//!
//! Pure projection: no IO, no HTTP, no store. The shapes below are the
//! `ilar serve` API, so treat them as a contract — add fields, never
//! repurpose one. Every projected object carries a `type` tag spelled
//! exactly like the session event's own serde tag, and an `ts` in RFC
//! 3339. Optional values are present as `null`, never omitted, so a
//! client can index a key without probing for it.
//!
//! One event in, one object out, in file order: `Rewind.to` indexes the
//! canonical stream, so the projection has to stay index-parallel with it
//! for the client's two-line fold (`rewind` → truncate, else push) to
//! land on the right event. Nothing is filtered here; the compaction cut
//! is a render-time decision the client makes from the `compaction`
//! object it receives.
//!
//! What the projection removes is bulk and secrets, using the same core
//! helpers the TUI renders with, so both surfaces cut in the same place:
//!
//! - **images**: base64 never crosses the wire. The text carries the
//!   marker lines [`ilar::image::markers`] (tool results) and
//!   [`ilar::image::attachment_markers`] (user messages) produce, and an
//!   `images` array of `{n, media_type, bytes}` descriptors names what a
//!   later image route can fetch by index.
//! - **tool result text**: [`ilar::text::bounded_detail`], with
//!   `truncated: true` when the cap bit — the full text is its own route.
//! - **tool inputs**: [`ilar::agent::summarize_tool_input`],
//!   [`ilar::agent::tool_argument_detail`] and
//!   [`ilar::agent::summarize_task_input`], which redact secrets.
//!
//! ```json
//! {"type":"meta","session_id":"…","parent_id":null,"agent":"build",
//!  "model":"zai/glm-4.7","cwd":null,"ts":"2026-08-26T12:00:00+00:00"}
//! {"type":"user_message","id":"…","text":"look\n[image attached: png · 12.3 KiB]",
//!  "images":[{"n":0,"media_type":"image/png","bytes":12600}],"ts":"…"}
//! {"type":"subagent_invocation","id":"…","parent_tool_call_id":"task-1","ts":"…"}
//! {"type":"assistant_message","id":"…","model":"zai/glm-4.7","stop_reason":"tool_use",
//!  "usage":{"input":1,"output":2,"cache_read":3,"cache_creation":4},
//!  "content":[{"type":"text","text":"on it"},
//!             {"type":"reasoning_summary","text":"**Planning**"},
//!             {"type":"tool_call","id":"read-1","name":"read","summary":"src/lib.rs:10",
//!              "detail":"{\n  \"path\": \"src/lib.rs\"\n}","agent":null}],"ts":"…"}
//! {"type":"tool_result","id":"…","tool_use_id":"read-1","is_error":false,
//!  "text":"…","truncated":false,"images":[],"child_session_id":null,"ts":"…"}
//! {"type":"checkpoint","id":"…","ts":"…"}
//! {"type":"model_change","id":"…","model":"openai/gpt-5.6-sol","variant":"high","ts":"…"}
//! {"type":"compaction","id":"…","summary":"…","kept_from":3,"ts":"…"}
//! {"type":"topic","id":"…","text":"serve","ts":"…"}
//! {"type":"rewind","id":"…","to":1,"ts":"…"}
//! ```
//!
//! A `tool_call` for the `task` tool carries `agent: {name, model}`;
//! every other tool carries `agent: null`. Assistant content keeps only
//! what a surface shows: raw thinking, opaque reasoning state,
//! diagnostics and half-streamed summaries never leave the process *in
//! a committed event*.
//!
//! The live scratch is the one exception, and a deliberate one: a turn
//! streaming its reasoning shows it as it arrives ([`project_live_delta`]),
//! exactly as the TUI does, and then the committed message drops it
//! again. Nothing is retained — the frame is ephemeral, unresumable and
//! gone the moment the step commits — but it *is* a surface that shows
//! reasoning text where the transcript would not. See
//! meta/issues/the-live-turn-lives-in-the-store.md.

use serde_json::{Value, json};

use ilar::session::{ContentBlock, ImageContent, LiveDelta, SessionEvent, Usage};

/// One canonical event as the wire sees it. Total: every event projects,
/// so the result array indexes the same way `Rewind.to` does.
pub(crate) fn project_event(event: &SessionEvent) -> Value {
    let ts = event.ts().to_rfc3339();
    match event {
        SessionEvent::Meta { meta, .. } => json!({
            "type": "meta",
            "session_id": meta.session_id,
            "parent_id": meta.parent_id,
            "agent": meta.agent,
            "model": meta.model,
            "cwd": meta.cwd.as_ref().map(|cwd| cwd.display().to_string()),
            "ts": ts,
        }),
        SessionEvent::UserMessage {
            id, text, images, ..
        } => json!({
            "type": "user_message",
            "id": id,
            "text": format!("{text}{}", ilar::image::attachment_markers(images)),
            "images": image_descriptors(images),
            "ts": ts,
        }),
        SessionEvent::SubagentInvocation {
            id,
            parent_tool_call_id,
            ..
        } => json!({
            "type": "subagent_invocation",
            "id": id,
            "parent_tool_call_id": parent_tool_call_id,
            "ts": ts,
        }),
        SessionEvent::AssistantMessage {
            id,
            model,
            content,
            usage,
            stop_reason,
            ..
        } => json!({
            "type": "assistant_message",
            "id": id,
            "model": model,
            "stop_reason": stop_reason,
            "usage": project_usage(usage),
            "content": content.iter().filter_map(project_block).collect::<Vec<_>>(),
            "ts": ts,
        }),
        SessionEvent::ToolResult {
            id,
            tool_use_id,
            content,
            is_error,
            images,
            child_session_id,
            ..
        } => {
            // The markers ride on the string the cap applies to, exactly
            // as the restored TUI row builds it: bounding only the text
            // would keep markers a truncated row drops.
            let text =
                ilar::text::bounded_detail(&format!("{content}{}", ilar::image::markers(images)));
            json!({
                "type": "tool_result",
                "id": id,
                "tool_use_id": tool_use_id,
                "is_error": is_error,
                "truncated": was_truncated(&text),
                "text": text,
                "images": image_descriptors(images),
                "child_session_id": child_session_id,
                "ts": ts,
            })
        }
        // Renders nothing; present so indices line up.
        SessionEvent::Checkpoint { id, .. } => json!({
            "type": "checkpoint",
            "id": id,
            "ts": ts,
        }),
        SessionEvent::ModelChange {
            id, model, variant, ..
        } => json!({
            "type": "model_change",
            "id": id,
            "model": model,
            "variant": variant,
            "ts": ts,
        }),
        SessionEvent::Compaction {
            id,
            summary,
            kept_from,
            ..
        } => json!({
            "type": "compaction",
            "id": id,
            "summary": summary,
            "kept_from": kept_from,
            "ts": ts,
        }),
        SessionEvent::Topic { id, text, .. } => json!({
            "type": "topic",
            "id": id,
            "text": text,
            "ts": ts,
        }),
        SessionEvent::Rewind { id, to, .. } => json!({
            "type": "rewind",
            "id": id,
            "to": to,
            "ts": ts,
        }),
    }
}

pub(crate) fn project_events(events: &[SessionEvent]) -> Vec<Value> {
    events.iter().map(project_event).collect()
}

/// One line of a running turn's scratch, for the ephemeral `delta`
/// frame. Same contract as above and the same `type` spelling as the
/// core enum's serde tag — but nothing here is a canonical event, so it
/// carries no id, no timestamp and no line number: it is a hint that the
/// committed event will replace.
///
/// `thinking_delta` is reasoning text, which the committed projection
/// above deliberately drops — see the module doc for why the live view
/// is the exception.
///
/// ```json
/// {"type":"text_delta","text":"on it"}
/// {"type":"thinking_delta","text":"weighing the two"}
/// {"type":"tool_started","id":"bash-1","name":"bash","summary":"cargo test"}
/// {"type":"tool_finished","id":"bash-1","ok":true}
/// {"type":"reset"}
/// ```
pub(crate) fn project_live_delta(delta: &LiveDelta) -> Value {
    match delta {
        // Never sent: the tailer consumes the generation marker itself.
        // Projected anyway, because a total projection cannot surprise a
        // caller later.
        LiveDelta::TurnStarted { turn, step } => {
            json!({ "type": "turn_started", "turn": turn, "step": step })
        }
        LiveDelta::TextDelta { text } => json!({ "type": "text_delta", "text": text }),
        LiveDelta::ThinkingDelta { text } => json!({ "type": "thinking_delta", "text": text }),
        LiveDelta::ToolStarted { id, name, summary } => json!({
            "type": "tool_started",
            "id": id,
            "name": name,
            "summary": summary,
        }),
        LiveDelta::ToolFinished { id, ok } => {
            json!({ "type": "tool_finished", "id": id, "ok": ok })
        }
    }
}

/// The frame that retires a client's streaming row: the step committed,
/// or the turn ended.
pub(crate) fn live_reset() -> Value {
    json!({ "type": "reset" })
}

/// The `?invocation=` slice: one child-session turn, from the
/// [`SessionEvent::SubagentInvocation`] naming `parent_tool_call_id`
/// (exclusive) up to the next invocation or the end of the log. Empty
/// when no invocation carries that id — the same rule `session_view`'s
/// nested timeline applies.
pub(crate) fn invocation_slice<'a>(
    events: &'a [SessionEvent],
    parent_tool_call_id: &str,
) -> &'a [SessionEvent] {
    let Some(start) = events.iter().position(|event| {
        matches!(
            event,
            SessionEvent::SubagentInvocation { parent_tool_call_id: current, .. }
                if current == parent_tool_call_id
        )
    }) else {
        return &[];
    };
    let end = events[start + 1..]
        .iter()
        .position(|event| matches!(event, SessionEvent::SubagentInvocation { .. }))
        .map_or(events.len(), |offset| start + 1 + offset);
    &events[start + 1..end]
}

/// Session totals for a header: tokens always, dollars when every turn's
/// model has a published price. An unpriced model poisons the dollar
/// figure (the fold lives in `session_view`, shared with the TUI status
/// line); a coding-plan model says `plan` instead, and an unknown one
/// says neither.
pub(crate) fn usage_totals(events: &[SessionEvent]) -> Value {
    let mut total = Usage::default();
    let mut cost = Some(0.0);
    for event in events {
        if let SessionEvent::AssistantMessage { model, usage, .. } = event {
            crate::session_view::accrue_usage(&mut total, &mut cost, model, usage);
        }
    }
    let mut totals = project_usage(&total);
    match cost {
        Some(cost) => totals["cost_dollars"] = json!(cost),
        None if ilar::model::plan_billed(effective_model(events)) => totals["plan"] = json!(true),
        None => {}
    }
    totals
}

/// The model a header bills against: the last runtime switch, else the
/// one the session opened with.
fn effective_model(events: &[SessionEvent]) -> &str {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::ModelChange { model, .. } => Some(model.as_str()),
            _ => None,
        })
        .or_else(|| {
            events.iter().find_map(|event| match event {
                SessionEvent::Meta { meta, .. } => Some(meta.model.as_str()),
                _ => None,
            })
        })
        .unwrap_or_default()
}

fn project_usage(usage: &Usage) -> Value {
    json!({
        "input": usage.input_tokens,
        "output": usage.output_tokens,
        "cache_read": usage.cache_read_input_tokens,
        "cache_creation": usage.cache_creation_input_tokens,
    })
}

/// What an image route needs to serve one: its index in the event's own
/// list, its type and its decoded size. Never the payload.
fn image_descriptors(images: &[ImageContent]) -> Vec<Value> {
    images
        .iter()
        .enumerate()
        .map(|(n, image)| {
            json!({ "n": n, "media_type": image.media_type, "bytes": image.byte_len() })
        })
        .collect()
}

/// Whether [`ilar::text::bounded_detail`] cut, read off its own
/// postcondition: it appends the marker and lands exactly on the cap.
/// Text that already ended that way *at* the cap reads as truncated,
/// which is what it is.
fn was_truncated(bounded: &str) -> bool {
    bounded.ends_with(ilar::text::DETAIL_TRUNCATED)
        && bounded.chars().count() == ilar::text::MAX_DETAIL_CHARS
}

/// Assistant content, keeping only what a surface renders. Raw thinking,
/// opaque reasoning items, provider diagnostics, half-streamed summaries
/// and the inline blocks that duplicate a `ToolResult` event all stop
/// here.
fn project_block(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
        ContentBlock::ReasoningSummary {
            text,
            completed: true,
        } => Some(json!({ "type": "reasoning_summary", "text": text })),
        ContentBlock::ToolCall {
            id, name, input, ..
        } => Some(project_tool_call(id, name, input)),
        ContentBlock::ReasoningSummary {
            completed: false, ..
        }
        | ContentBlock::Image { .. }
        | ContentBlock::Thinking { .. }
        | ContentBlock::Reasoning { .. }
        | ContentBlock::Diagnostic { .. }
        | ContentBlock::ToolResult { .. } => None,
    }
}

/// A tool call as a row: the one-line summary, the redacted pretty
/// input, and — for `task` — the agent the call spawned, exactly the
/// split `session_view` makes when it builds a `ToolKind`.
fn project_tool_call(id: &str, name: &str, input: &Value) -> Value {
    let (agent, summary) = match (name, ilar::agent::summarize_task_input(input)) {
        ("task", Some((description, agent, model))) => {
            (json!({ "name": agent, "model": model }), description)
        }
        ("task", None) => (
            json!({ "name": "subagent", "model": null }),
            ilar::agent::summarize_tool_input(name, input),
        ),
        _ => (Value::Null, ilar::agent::summarize_tool_input(name, input)),
    };
    json!({
        "type": "tool_call",
        "id": id,
        "name": name,
        "summary": summary,
        "detail": ilar::agent::tool_argument_detail(name, input),
        "agent": agent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use ilar::session::{
        ContentBlock, ImageContent, InputTokenAccounting, SessionMeta, Usage, new_id,
    };

    fn ts() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap()
    }

    fn meta(model: &str) -> SessionEvent {
        SessionEvent::Meta {
            meta: SessionMeta {
                session_id: "session-1".into(),
                parent_id: None,
                agent: "build".into(),
                model: model.into(),
                workspace: None,
                cwd: None,
            },
            ts: ts(),
        }
    }

    fn user(text: &str, images: Vec<ImageContent>) -> SessionEvent {
        SessionEvent::UserMessage {
            id: new_id(),
            text: text.into(),
            images,
            ts: ts(),
        }
    }

    fn tool_result(tool_use_id: &str, content: &str, images: Vec<ImageContent>) -> SessionEvent {
        SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: false,
            images,
            child_session_id: None,
            state: None,
            ts: ts(),
        }
    }

    fn invocation(parent_tool_call_id: &str) -> SessionEvent {
        SessionEvent::SubagentInvocation {
            id: new_id(),
            parent_tool_call_id: parent_tool_call_id.into(),
            ts: ts(),
        }
    }

    fn assistant(model: &str, content: Vec<ContentBlock>, usage: Usage) -> SessionEvent {
        SessionEvent::AssistantMessage {
            id: "message-1".into(),
            model: model.into(),
            content,
            usage,
            stop_reason: "end_turn".into(),
            ts: ts(),
        }
    }

    fn usage(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_creation,
            input_token_accounting: Some(InputTokenAccounting::ExcludesCached),
        }
    }

    /// The wire carries the marker text the TUI shows and a descriptor per
    /// image — never the base64, which the image route serves lazily.
    #[test]
    fn an_image_bearing_tool_result_projects_the_markers_the_tui_renders() {
        let image = ImageContent::png(&vec![0u8; 12_600]);
        let jpeg = ImageContent::new("image/jpeg", &[0u8; 900]);
        let images = vec![image.clone(), jpeg.clone()];
        let value = project_event(&tool_result("read-1", "shot.png: 640x480", images.clone()));

        assert_eq!(value["type"], "tool_result");
        assert_eq!(value["tool_use_id"], "read-1");
        assert_eq!(
            value["text"],
            json!(format!(
                "shot.png: 640x480{}",
                ilar::image::markers(&images)
            ))
        );
        assert_eq!(value["truncated"], json!(false));
        assert_eq!(
            value["images"],
            json!([
                { "n": 0, "media_type": "image/png", "bytes": 12_600 },
                { "n": 1, "media_type": "image/jpeg", "bytes": 900 },
            ])
        );
        // The payload never rides along.
        assert!(!value.to_string().contains(&image.data));
    }

    #[test]
    fn a_user_message_carries_attachment_markers_and_descriptors() {
        let image = ImageContent::png(&vec![0u8; 12_600]);
        let images = std::slice::from_ref(&image);
        let value = project_event(&user("look at this", images.to_vec()));

        assert_eq!(value["type"], "user_message");
        assert_eq!(
            value["text"],
            json!(format!(
                "look at this{}",
                ilar::image::attachment_markers(images)
            ))
        );
        assert_eq!(
            value["images"],
            json!([{ "n": 0, "media_type": "image/png", "bytes": 12_600 }])
        );
    }

    /// The same cut `bounded_detail` gives the TUI, flagged so the client
    /// knows a full-text route exists.
    #[test]
    fn an_over_cap_tool_result_is_bounded_and_flagged_truncated() {
        let raw = "x".repeat(ilar::text::MAX_DETAIL_CHARS * 2);
        let value = project_event(&tool_result("bash-1", &raw, Vec::new()));

        assert_eq!(value["text"], json!(ilar::text::bounded_detail(&raw)));
        assert_eq!(value["truncated"], json!(true));
        let text = value["text"].as_str().unwrap();
        assert_eq!(text.chars().count(), ilar::text::MAX_DETAIL_CHARS);
        assert!(text.ends_with(ilar::text::DETAIL_TRUNCATED));

        // At the cap exactly, nothing was cut.
        let exact = "y".repeat(ilar::text::MAX_DETAIL_CHARS);
        let value = project_event(&tool_result("bash-2", &exact, Vec::new()));
        assert_eq!(value["text"], json!(exact));
        assert_eq!(value["truncated"], json!(false));
    }

    /// Markers ride on the same string the cap applies to, exactly as
    /// `session_view::restored_session_invocation_view` does it.
    #[test]
    fn markers_are_bounded_with_the_text_they_follow() {
        let image = ImageContent::png(&vec![0u8; 12_600]);
        let raw = "z".repeat(ilar::text::MAX_DETAIL_CHARS * 2);
        let value = project_event(&tool_result("bash-3", &raw, vec![image.clone()]));
        assert_eq!(
            value["text"],
            json!(ilar::text::bounded_detail(&format!(
                "{raw}{}",
                ilar::image::markers(std::slice::from_ref(&image))
            )))
        );
        // Descriptors survive the cut even when the markers did not.
        assert_eq!(value["images"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tool_calls_are_summarized_by_the_core_helpers() {
        let read_input = json!({ "path": "src/lib.rs", "offset": 10 });
        let task_input = json!({
            "description": "Review restored security paths",
            "subagent_type": "build · secure",
        });
        let value = project_event(&assistant(
            "zai/glm-4.7",
            vec![
                ContentBlock::Text {
                    text: "on it".into(),
                },
                ContentBlock::Thinking {
                    text: "hidden thought".into(),
                },
                ContentBlock::ReasoningSummary {
                    text: "**Planning**".into(),
                    completed: true,
                },
                ContentBlock::ReasoningSummary {
                    text: "half a thought".into(),
                    completed: false,
                },
                ContentBlock::ToolCall {
                    id: "read-1".into(),
                    name: "read".into(),
                    input: read_input.clone(),
                    item_id: None,
                },
                ContentBlock::ToolCall {
                    id: "task-1".into(),
                    name: "task".into(),
                    input: task_input.clone(),
                    item_id: None,
                },
            ],
            usage(1, 2, 3, 4),
        ));

        assert_eq!(value["type"], "assistant_message");
        assert_eq!(value["model"], "zai/glm-4.7");
        assert_eq!(
            value["usage"],
            json!({ "input": 1, "output": 2, "cache_read": 3, "cache_creation": 4 })
        );
        let content = value["content"].as_array().unwrap();
        // Thinking and an incomplete summary never reach a surface.
        assert_eq!(content.len(), 4);
        assert_eq!(content[0], json!({ "type": "text", "text": "on it" }));
        assert_eq!(
            content[1],
            json!({ "type": "reasoning_summary", "text": "**Planning**" })
        );
        assert_eq!(
            content[2],
            json!({
                "type": "tool_call",
                "id": "read-1",
                "name": "read",
                "summary": ilar::agent::summarize_tool_input("read", &read_input),
                "detail": ilar::agent::tool_argument_detail("read", &read_input),
                "agent": null,
            })
        );
        assert_eq!(
            content[3],
            json!({
                "type": "tool_call",
                "id": "task-1",
                "name": "task",
                "summary": "Review restored security paths",
                "detail": ilar::agent::tool_argument_detail("task", &task_input),
                "agent": { "name": "build · secure", "model": null },
            })
        );
        assert!(!value.to_string().contains("hidden thought"));
        assert!(!value.to_string().contains("half a thought"));
    }

    /// `Rewind.to` indexes the canonical stream, so the projection has to
    /// be index-parallel with it: every event projects, none is dropped.
    #[test]
    fn every_canonical_event_projects_in_file_order() {
        let events = vec![
            meta("zai/glm-4.7"),
            user("hi", Vec::new()),
            invocation("task-1"),
            assistant("zai/glm-4.7", Vec::new(), Usage::default()),
            tool_result("read-1", "ok", Vec::new()),
            SessionEvent::Checkpoint {
                id: new_id(),
                commit: "abc123".into(),
                head: None,
                ts: ts(),
            },
            SessionEvent::ModelChange {
                id: new_id(),
                model: "openai/gpt-5.6-sol".into(),
                variant: Some("high".into()),
                ts: ts(),
            },
            SessionEvent::Compaction {
                id: new_id(),
                summary: "kept decisions".into(),
                kept_from: 3,
                ts: ts(),
            },
            SessionEvent::Topic {
                id: new_id(),
                text: "serve".into(),
                ts: ts(),
            },
            SessionEvent::Rewind {
                id: new_id(),
                to: 1,
                tree_restored: None,
                tree_saved: None,
                ts: ts(),
            },
        ];
        let projected = project_events(&events);
        assert_eq!(projected.len(), events.len());
        assert_eq!(
            projected
                .iter()
                .map(|value| value["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "meta",
                "user_message",
                "subagent_invocation",
                "assistant_message",
                "tool_result",
                "checkpoint",
                "model_change",
                "compaction",
                "topic",
                "rewind",
            ]
        );
        assert_eq!(projected[0]["model"], "zai/glm-4.7");
        assert_eq!(projected[0]["agent"], "build");
        assert_eq!(projected[2]["parent_tool_call_id"], "task-1");
        assert_eq!(projected[6]["variant"], "high");
        assert_eq!(projected[7]["kept_from"], json!(3));
        assert_eq!(projected[9]["to"], json!(1));
        // Every event carries a timestamp the client can render.
        assert!(projected.iter().all(|value| value["ts"].is_string()));
    }

    #[test]
    fn an_invocation_slice_ends_at_the_next_invocation() {
        let events = vec![
            meta("zai/glm-4.7"),
            invocation("task-1"),
            user("first request", Vec::new()),
            assistant("zai/glm-4.7", Vec::new(), Usage::default()),
            invocation("task-2"),
            user("second request", Vec::new()),
        ];
        let slice = invocation_slice(&events, "task-1");
        assert_eq!(slice.len(), 2);
        assert!(matches!(
            &slice[0],
            SessionEvent::UserMessage { text, .. } if text == "first request"
        ));
        assert!(matches!(&slice[1], SessionEvent::AssistantMessage { .. }));

        // The last invocation runs to the end of the log.
        let tail = invocation_slice(&events, "task-2");
        assert_eq!(tail.len(), 1);
        assert!(matches!(
            &tail[0],
            SessionEvent::UserMessage { text, .. } if text == "second request"
        ));
    }

    #[test]
    fn an_unknown_invocation_yields_nothing() {
        let events = vec![meta("zai/glm-4.7"), invocation("task-1"), user("x", vec![])];
        assert!(invocation_slice(&events, "task-missing").is_empty());
        assert!(invocation_slice(&[], "task-1").is_empty());
    }

    /// zai/glm-4.7 is priced 0.6/2.2/0.11/0.0 USD per million for
    /// input/output/cache-read/cache-write, so the two turns below cost
    /// 0.6 + (1.1 + 0.22 + 0.0) = 1.92 dollars.
    #[test]
    fn usage_totals_accumulate_tokens_and_dollars_for_a_priced_model() {
        let events = vec![
            meta("zai/glm-4.7"),
            assistant("zai/glm-4.7", Vec::new(), usage(1_000_000, 0, 0, 0)),
            assistant(
                "zai/glm-4.7",
                Vec::new(),
                usage(0, 500_000, 2_000_000, 1_000_000),
            ),
        ];
        let totals = usage_totals(&events);
        assert_eq!(totals["input"], json!(1_000_000));
        assert_eq!(totals["output"], json!(500_000));
        assert_eq!(totals["cache_read"], json!(2_000_000));
        assert_eq!(totals["cache_creation"], json!(1_000_000));
        let cost = totals["cost_dollars"].as_f64().unwrap();
        assert!((cost - 1.92).abs() < 1e-9, "{cost}");
        assert!(totals.get("plan").is_none());
    }

    /// A coding-plan model has no per-token price; tokens still add up and
    /// the header says `plan` instead of a dollar figure, like the TUI's
    /// status line.
    #[test]
    fn usage_totals_report_the_plan_for_a_plan_billed_model() {
        let events = vec![
            meta("zai/glm-5.3"),
            assistant("zai/glm-5.3", Vec::new(), usage(120, 30, 40, 0)),
            assistant("zai/glm-5.3", Vec::new(), usage(80, 20, 10, 5)),
        ];
        let totals = usage_totals(&events);
        assert_eq!(totals["input"], json!(200));
        assert_eq!(totals["output"], json!(50));
        assert_eq!(totals["cache_read"], json!(50));
        assert_eq!(totals["cache_creation"], json!(5));
        assert_eq!(totals["plan"], json!(true));
        assert!(totals.get("cost_dollars").is_none());

        // An unpriced model that is not plan-billed gets neither key.
        let unknown = usage_totals(&[
            meta("custom/unknown"),
            assistant("custom/unknown", Vec::new(), usage(1, 1, 0, 0)),
        ]);
        assert_eq!(unknown["input"], json!(1));
        assert!(unknown.get("cost_dollars").is_none());
        assert!(unknown.get("plan").is_none());

        // A runtime switch decides which model the header bills against.
        let switched = usage_totals(&[
            meta("zai/glm-4.7"),
            SessionEvent::ModelChange {
                id: new_id(),
                model: "zai/glm-5.3".into(),
                variant: None,
                ts: ts(),
            },
            assistant("zai/glm-5.3", Vec::new(), usage(10, 10, 0, 0)),
        ]);
        assert_eq!(switched["plan"], json!(true));
    }

    #[test]
    fn an_empty_session_totals_to_zero_dollars() {
        let totals = usage_totals(&[]);
        assert_eq!(totals["input"], json!(0));
        assert_eq!(totals["cost_dollars"], json!(0.0));
    }
}
