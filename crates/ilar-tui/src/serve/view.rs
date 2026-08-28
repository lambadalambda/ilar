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
//!  "model":"zai/glm-4.7","cwd":null,"context_limit":73728,
//!  "ts":"2026-08-26T12:00:00+00:00"}
//! {"type":"user_message","id":"…","text":"look\n[image attached: png · 12.3 KiB]",
//!  "images":[{"n":0,"media_type":"image/png","bytes":12600}],
//!  "notification":null,"ts":"…"}
//! {"type":"subagent_invocation","id":"…","parent_tool_call_id":"task-1","ts":"…"}
//! {"type":"assistant_message","id":"…","model":"zai/glm-4.7","stop_reason":"tool_use",
//!  "usage":{"input":1,"output":2,"cache_read":3,"cache_creation":4},
//!  "content":[{"type":"text","text":"on it"},
//!             {"type":"reasoning_summary","text":"**Planning**"},
//!             {"type":"diagnostic","text":"turn error: provider exploded"},
//!             {"type":"tool_call","id":"read-1","name":"read","summary":"src/lib.rs:10",
//!              "detail":"{\n  \"path\": \"src/lib.rs\"\n}","agent":null,
//!              "state":"running","diff":null}],"ts":"…"}
//! {"type":"tool_result","id":"…","tool_use_id":"read-1","is_error":false,
//!  "text":"…","truncated":false,"images":[],"child_session_id":null,"ts":"…"}
//! {"type":"checkpoint","id":"…","ts":"…"}
//! {"type":"model_change","id":"…","model":"openai/gpt-5.6-sol","variant":"high","ts":"…"}
//! {"type":"compaction","id":"…","summary":"…","kept_from":3,"ts":"…"}
//! {"type":"topic","id":"…","text":"serve","ts":"…"}
//! {"type":"rewind","id":"…","to":1,"ts":"…"}
//! ```
//!
//! `context_limit` on the `meta` line is the one field here that is not
//! in the log: it is looked up per model in this binary's catalog
//! ([`context_limit`]), so a context meter has an honest denominator
//! without the client shipping a copy of the catalog. The session
//! listing carries it too, because the panel needs it before any page
//! has reached the `meta` line. `null` means the model is unknown here.
//!
//! A `tool_call` for the `task` tool carries `agent: {name, model}`;
//! every other tool carries `agent: null`. Three more fields on it are
//! answers the client cannot work out from one event on its own:
//!
//! - **`state`**: `"running"` while a call is unanswered and something is
//!   still running the session, `"failed"` for one nothing will ever
//!   answer — the sweep both TUI paths do on load (`session_view`'s
//!   restore, `transcript`'s live rebuild), because a killed process
//!   leaves its `bash` unanswered forever — and `null` once a
//!   `tool_result` answers it, which speaks for itself. A single event
//!   projected on its own ([`project_event`], the SSE path) is always
//!   `"running"`: the call just landed and its result cannot have.
//!   [`project_page`] is what settles it against a whole session, so a
//!   client that also *follows* a session folds in the liveness the
//!   listing reports — a page open while the process is killed holds
//!   rows the page load never saw.
//! - **`diff`**: the ± lines the TUI draws for an `edit` (and, for a
//!   `write`, the file it is about to create as pure additions), as
//!   `[{"kind":"add"|"del"|"ctx","text":"…"}]`, or `null` for every other
//!   tool and for input too large to diff. It is computed from the *raw*
//!   input, not from `detail` — `detail` is bounded to 16 KiB and stops
//!   being valid JSON there, which is exactly where a big edit needs the
//!   diff most.
//! - a `user_message` carries **`notification`**: the parsed
//!   `<task-notification>` / `<tool-notification>` envelope background
//!   work writes into the transcript, as
//!   `{"kind":"task"|"job","headline":"…","body":"…"}`, or `null` for an
//!   ordinary message. `text` still carries the envelope verbatim; the
//!   parse is the one the TUI renders with
//!   ([`crate::session_view::task_notification_display`]), so a surface
//!   shows a headline with the report behind a click rather than XML
//!   attributed to the user.
//!
//! Assistant content keeps only
//! what a surface shows: raw thinking, opaque reasoning state and
//! half-streamed summaries never leave the process *in a committed
//! event*. The one diagnostic that does is `{"type":"diagnostic"}`,
//! carrying why a turn stopped — a reader without it sees a transcript
//! that simply ends.
//!
//! The live scratch is the one exception, and a deliberate one: a turn
//! streaming its reasoning shows it as it arrives ([`project_live_delta`]),
//! exactly as the TUI does, and then the committed message drops it
//! again. Nothing is retained — the frame is ephemeral, unresumable and
//! gone the moment the step commits — but it *is* a surface that shows
//! reasoning text where the transcript would not. See
//! meta/issues/the-live-turn-lives-in-the-store.md.

use std::collections::HashSet;

use serde_json::{Value, json};

use ilar::session::{ContentBlock, ImageContent, LiveDelta, SessionEvent, Usage};

use crate::diff::{DiffKind, DiffLine};

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
            "context_limit": context_limit(&meta.model),
            "ts": ts,
        }),
        SessionEvent::UserMessage {
            id, text, images, ..
        } => json!({
            "type": "user_message",
            "id": id,
            "text": format!("{text}{}", ilar::image::attachment_markers(images)),
            "images": image_descriptors(images),
            "notification": notification(text),
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

/// One page of `view`, with every tool call's `state` settled against the
/// **whole** session rather than against the page it happens to sit in.
///
/// `page` is a sub-slice of `view`; `live` says whether something is
/// running this session right now (its live-turn scratch exists). The
/// distinction is the whole point: an unanswered call in a session nobody
/// is running is a call that was killed mid-flight, which is what
/// `session_view`'s restore sweep and the live rebuild both decide on
/// load. An unanswered call in a *working* session is a tool running this
/// second and still reads as running.
///
/// The answered set is taken over `view` and not over `page` because a
/// page is a window: the call is in the window a reader paged back to and
/// its result may well be in the next one.
pub(crate) fn project_page(view: &[SessionEvent], page: &[SessionEvent], live: bool) -> Vec<Value> {
    let unanswered = unanswered_calls(view);
    // The one call an idle session may still be legitimately holding.
    let waiting = sole_pending_question(view, &unanswered);
    page.iter()
        .map(|event| {
            let mut projected = project_event(event);
            settle_tool_states(&mut projected, &unanswered, waiting, live);
            projected
        })
        .collect()
}

/// Tool calls no `tool_result` in this view ever answers.
///
/// Ids are taken as unique, which is the store's own invariant
/// (`validate_replay` refuses a log that reuses one): a repeat would read
/// as answered here the moment either occurrence was.
fn unanswered_calls(events: &[SessionEvent]) -> HashSet<&str> {
    let mut calls: Vec<&str> = Vec::new();
    let mut answered: HashSet<&str> = HashSet::new();
    for event in events {
        match event {
            SessionEvent::AssistantMessage { content, .. } => {
                calls.extend(content.iter().filter_map(|block| match block {
                    ContentBlock::ToolCall { id, .. } => Some(id.as_str()),
                    _ => None,
                }));
            }
            SessionEvent::ToolResult { tool_use_id, .. } => {
                answered.insert(tool_use_id.as_str());
            }
            _ => {}
        }
    }
    calls
        .into_iter()
        .filter(|id| !answered.contains(id))
        .collect()
}

/// The one unanswered call that is a `question` waiting on an answer —
/// the exception the TUI's own sweep makes, since a session suspended on
/// the user is waiting, not dead. `None` unless it is the *only* thing
/// outstanding, which is the rule
/// [`ilar::session::Session::pending_question`] applies.
fn sole_pending_question<'a>(
    events: &'a [SessionEvent],
    unanswered: &HashSet<&'a str>,
) -> Option<&'a str> {
    let [only] = unanswered.iter().copied().collect::<Vec<_>>()[..] else {
        return None;
    };
    events.iter().rev().find_map(|event| match event {
        SessionEvent::AssistantMessage { content, .. } => {
            content.iter().find_map(|block| match block {
                ContentBlock::ToolCall { id, name, .. }
                    if id == only && name == ilar::question::QUESTION_TOOL_NAME =>
                {
                    Some(only)
                }
                _ => None,
            })
        }
        _ => None,
    })
}

/// Rewrite the `state` [`project_tool_call`] guessed — it sees one event
/// and can only say "running" — now that the whole session is in view.
fn settle_tool_states(
    projected: &mut Value,
    unanswered: &HashSet<&str>,
    waiting: Option<&str>,
    live: bool,
) {
    if projected.get("type").and_then(Value::as_str) != Some("assistant_message") {
        return;
    }
    let Some(content) = projected.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in content {
        let Some(id) = block["id"]
            .as_str()
            .filter(|_| block["type"] == "tool_call")
        else {
            continue;
        };
        block["state"] = match (unanswered.contains(id), live || waiting == Some(id)) {
            (false, _) => Value::Null,
            (true, true) => json!("running"),
            (true, false) => json!("failed"),
        };
    }
}

/// The window a context meter must measure against, by full model id;
/// `null` for a model this binary has no catalog row for, because a bar
/// drawn against a guessed denominator is worse than no bar.
///
/// This is [`ilar::model::compaction_limit`] — the provider's *input*
/// cap — and not the whole context window, for exactly the reason the
/// TUI's own meter reads that way: a request is rejected on its input
/// size, so a meter against the window reads as comfortable headroom
/// while the next request is already unsendable. The two surfaces quote
/// the same number.
///
/// The catalog is consulted directly rather than through a
/// `ProviderResolver`: `ilar serve` starts with no provider
/// configuration at all, and a limit is a property of the model, not of
/// who is serving it. A `custom/…` row registered from configuration
/// answers here too, since [`ilar::model::find`] covers both.
pub(crate) fn context_limit(model: &str) -> Value {
    ilar::model::find(model).map_or(Value::Null, |info| {
        json!(ilar::model::compaction_limit(info))
    })
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
/// `thinking_break` closes the thought that was streaming: the deltas
/// carry no boundary of their own, so it is the only thing telling a
/// client where one summary ends and the next begins.
///
/// ```json
/// {"type":"text_delta","text":"on it"}
/// {"type":"thinking_delta","text":"weighing the two"}
/// {"type":"thinking_break"}
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
        LiveDelta::ThinkingBreak => json!({ "type": "thinking_break" }),
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
    let Some(start) = invocation_at(events, parent_tool_call_id) else {
        return &[];
    };
    let end = events[start + 1..]
        .iter()
        .position(|event| matches!(event, SessionEvent::SubagentInvocation { .. }))
        .map_or(events.len(), |offset| start + 1 + offset);
    &events[start + 1..end]
}

/// Whether this log holds the invocation a parent's task call spawned —
/// which is the only link back, and the one that exists *while the
/// subagent runs*: `ToolResult.child_session_id` is written when the task
/// finishes, so a reader waiting on a twenty-minute subagent has nothing
/// else to go on. Distinct from an empty [`invocation_slice`], which is
/// also what an invocation that has not said anything yet returns.
pub(crate) fn has_invocation(events: &[SessionEvent], parent_tool_call_id: &str) -> bool {
    invocation_at(events, parent_tool_call_id).is_some()
}

fn invocation_at(events: &[SessionEvent], parent_tool_call_id: &str) -> Option<usize> {
    events.iter().position(|event| {
        matches!(
            event,
            SessionEvent::SubagentInvocation { parent_tool_call_id: current, .. }
                if current == parent_tool_call_id
        )
    })
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
/// opaque reasoning items, thinking kept as a local diagnostic,
/// half-streamed summaries and the inline blocks that duplicate a
/// `ToolResult` event all stop here. A turn-error diagnostic passes.
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
        // Why a turn stopped, and only that: without it the transcript
        // simply ends and the reader is left guessing. Raw thinking
        // wears the same block and stays local.
        ContentBlock::Diagnostic {
            text,
            kind: ilar::session::DiagnosticKind::TurnError,
        } => Some(json!({ "type": "diagnostic", "text": text })),
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
        // What one event alone can say: nothing has answered this call.
        // `project_page` settles it against the whole session.
        "state": "running",
        "diff": tool_diff(name, input),
    })
}

/// The most of a `write` that is sent as a diff. A new file is pure
/// addition — there is no match to run, so [`crate::diff::diff_lines`]'s
/// line cap (which exists to bound a quadratic LCS) does not apply and a
/// 500-line file would fall back to truncated JSON for no reason. Only
/// the bulk matters, and this is the same order as the byte cap the LCS
/// path uses.
const MAX_WRITE_DIFF_BYTES: usize = 256 * 1024;

/// The ± the TUI draws, as data: [`crate::diff::tool_diff_value`] for an
/// `edit`, and for a `write` the body it is about to put on disk as pure
/// additions. `null` for every other tool, and past the caps, where the
/// client falls back to the pretty input.
///
/// Taken from the raw input rather than from `detail`, which
/// [`ilar::agent::tool_argument_detail`] bounds to 16 KiB: past that cap
/// `detail` is truncated JSON, and the edits big enough to truncate are
/// the ones a reader most needs a diff for.
fn tool_diff(name: &str, input: &Value) -> Value {
    let lines = match name {
        "write" => written_lines(input),
        _ => crate::diff::tool_diff_value(name, input),
    };
    if lines.is_empty() {
        return Value::Null;
    }
    Value::Array(lines.iter().map(diff_line).collect())
}

/// A `write`'s body, every line of it an addition.
fn written_lines(input: &Value) -> Vec<DiffLine> {
    input
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| content.len() <= MAX_WRITE_DIFF_BYTES)
        .map(|content| {
            content
                .lines()
                .map(|text| DiffLine {
                    kind: DiffKind::Added,
                    text: text.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn diff_line(line: &DiffLine) -> Value {
    let kind = match line.kind {
        DiffKind::Added => "add",
        DiffKind::Removed => "del",
        DiffKind::Context => "ctx",
    };
    json!({ "kind": kind, "text": line.text })
}

/// A background task's or job's report, unwrapped from the envelope it
/// was written in — the same parse the TUI's `Task`/`Job` rows use, so
/// the two surfaces show the same headline and hide the same body.
/// `null` for an ordinary user message, which is nearly all of them.
fn notification(text: &str) -> Value {
    let parsed = crate::session_view::task_notification_display(text)
        .map(|display| ("task", display))
        .or_else(|| {
            crate::session_view::tool_notification_display(text).map(|display| ("job", display))
        });
    match parsed {
        Some((kind, display)) => {
            let (headline, body) = display.split_once('\n').unwrap_or((display.as_str(), ""));
            json!({ "kind": kind, "headline": headline, "body": body })
        }
        None => Value::Null,
    }
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

    /// Two unrelated things are stored as diagnostics. Why a turn died
    /// crosses the wire — without it the transcript just stops — while
    /// raw thinking, kept as a diagnostic because no provider takes it
    /// back, stays in the process.
    #[test]
    fn a_turn_error_reaches_the_page_and_raw_thinking_never_does() {
        use ilar::session::DiagnosticKind;
        let event = assistant(
            "zai/glm-4.7",
            vec![
                ContentBlock::Diagnostic {
                    text: "chain of thought nobody asked for".into(),
                    kind: DiagnosticKind::Local,
                },
                ContentBlock::Diagnostic {
                    text: "turn error: provider exploded".into(),
                    kind: DiagnosticKind::TurnError,
                },
            ],
            usage(1, 1, 0, 0),
        );

        let projected = project_event(&event);
        let content = projected["content"].as_array().expect("content is a list");

        assert_eq!(content.len(), 1, "{content:?}");
        assert_eq!(content[0]["type"], "diagnostic");
        assert_eq!(content[0]["text"], "turn error: provider exploded");
        assert!(
            !serde_json::to_string(&projected)
                .unwrap()
                .contains("nobody asked for"),
            "raw thinking left the process: {projected}"
        );
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
                // What one event alone can say; `project_page` settles it.
                "state": "running",
                "diff": null,
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
                "state": "running",
                "diff": null,
            })
        );
        assert!(!value.to_string().contains("hidden thought"));
        assert!(!value.to_string().contains("half a thought"));
    }

    /// The one field on the wire that is not in the log: the window a
    /// context meter measures against, looked up per model.
    #[test]
    fn a_meta_line_carries_the_context_limit_the_catalog_knows() {
        let known = project_event(&meta("zai/glm-4.7"));
        assert_eq!(
            known["context_limit"],
            json!(ilar::model::compaction_limit(
                ilar::model::find("zai/glm-4.7").expect("a cataloged model")
            ))
        );
        // The provider's input cap, not the whole 204 800 window: the
        // meter says what compaction measures, exactly as the TUI's does.
        assert_eq!(known["context_limit"], json!(73_728));

        // A model this binary has never heard of gets no denominator
        // rather than a guessed one — and the key is still there.
        let unknown = project_event(&meta("nobody/nothing"));
        assert_eq!(unknown["context_limit"], Value::Null);
        assert!(unknown.as_object().unwrap().contains_key("context_limit"));
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
        let projected: Vec<Value> = events.iter().map(project_event).collect();
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

    /// The live frames, with the core enum's own serde spelling — a
    /// client switches on `type` across both halves of the wire.
    #[test]
    fn live_deltas_project_with_the_core_spelling() {
        let deltas = [
            LiveDelta::TurnStarted {
                turn: "turn-1".into(),
                step: 2,
            },
            LiveDelta::TextDelta {
                text: "on it".into(),
            },
            LiveDelta::ThinkingDelta {
                text: "**Planning**".into(),
            },
            LiveDelta::ThinkingBreak,
            LiveDelta::ToolStarted {
                id: "bash-1".into(),
                name: "bash".into(),
                summary: "cargo test".into(),
            },
            LiveDelta::ToolFinished {
                id: "bash-1".into(),
                ok: true,
            },
        ];
        assert_eq!(
            deltas.iter().map(project_live_delta).collect::<Vec<_>>(),
            vec![
                json!({ "type": "turn_started", "turn": "turn-1", "step": 2 }),
                json!({ "type": "text_delta", "text": "on it" }),
                json!({ "type": "thinking_delta", "text": "**Planning**" }),
                json!({ "type": "thinking_break" }),
                json!({
                    "type": "tool_started",
                    "id": "bash-1",
                    "name": "bash",
                    "summary": "cargo test",
                }),
                json!({ "type": "tool_finished", "id": "bash-1", "ok": true }),
            ]
        );
        // The projection's tag is the enum's own, not a second spelling
        // of it maintained by hand.
        for delta in &deltas {
            assert_eq!(
                serde_json::to_value(delta).unwrap()["type"],
                project_live_delta(delta)["type"]
            );
        }
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

    fn call(id: &str, name: &str, input: Value) -> ContentBlock {
        ContentBlock::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
            item_id: None,
        }
    }

    fn states(projected: &[Value]) -> Vec<(String, Value)> {
        projected
            .iter()
            .filter(|value| value["type"] == "assistant_message")
            .flat_map(|value| value["content"].as_array().cloned().unwrap_or_default())
            .filter(|block| block["type"] == "tool_call")
            .map(|block| {
                (
                    block["id"].as_str().unwrap().to_string(),
                    block["state"].clone(),
                )
            })
            .collect()
    }

    /// The sweep both TUI paths do on load, on the wire: a call nothing
    /// answered is *failed* in a session nobody is running, and still
    /// running while a turn is. A call with a result carries no state at
    /// all — its result speaks for itself.
    #[test]
    fn an_unanswered_call_is_failed_unless_the_session_is_live() {
        let events = vec![
            meta("zai/glm-4.7"),
            assistant(
                "zai/glm-4.7",
                vec![
                    call("read-1", "read", json!({ "path": "src/lib.rs" })),
                    call("bash-1", "bash", json!({ "command": "sleep 900" })),
                ],
                Usage::default(),
            ),
            tool_result("read-1", "ok", Vec::new()),
        ];

        assert_eq!(
            states(&project_page(&events, &events, false)),
            vec![
                ("read-1".to_string(), Value::Null),
                ("bash-1".to_string(), json!("failed")),
            ]
        );
        assert_eq!(
            states(&project_page(&events, &events, true)),
            vec![
                ("read-1".to_string(), Value::Null),
                ("bash-1".to_string(), json!("running")),
            ],
            "a turn is running it right now"
        );
        // One event on its own cannot know: the SSE path says running,
        // which is what a call that just landed is.
        assert_eq!(
            project_event(&events[1])["content"][1]["state"],
            json!("running")
        );
    }

    /// A page is a window, and a call's answer may be in the next one.
    /// The sweep is taken over the whole session, never over the page.
    #[test]
    fn a_page_settles_its_calls_against_the_whole_session() {
        let events = vec![
            meta("zai/glm-4.7"),
            assistant(
                "zai/glm-4.7",
                vec![call("read-1", "read", json!({ "path": "x" }))],
                Usage::default(),
            ),
            tool_result("read-1", "ok", Vec::new()),
        ];
        // The page holds the call but not the result that follows it.
        let page = project_page(&events, &events[..2], false);
        assert_eq!(states(&page), vec![("read-1".to_string(), Value::Null)]);
    }

    /// The one exception the TUI's sweep makes: a session suspended on a
    /// structured question is waiting, not dead — and only while nothing
    /// else is outstanding.
    #[test]
    fn a_lone_pending_question_keeps_running_in_an_idle_session() {
        let asking = assistant(
            "zai/glm-4.7",
            vec![call(
                "question-1",
                ilar::question::QUESTION_TOOL_NAME,
                json!({
                    "question": "which one?",
                    "options": [{"label": "left"}, {"label": "right"}],
                }),
            )],
            Usage::default(),
        );
        let events = vec![meta("zai/glm-4.7"), asking.clone()];
        assert_eq!(
            states(&project_page(&events, &events, false)),
            vec![("question-1".to_string(), json!("running"))]
        );

        // With another call outstanding beside it, the session did not
        // suspend — it stopped, and both rows say so.
        let events = vec![
            meta("zai/glm-4.7"),
            asking,
            assistant(
                "zai/glm-4.7",
                vec![call("bash-1", "bash", json!({ "command": "sleep 900" }))],
                Usage::default(),
            ),
        ];
        assert_eq!(
            states(&project_page(&events, &events, false)),
            vec![
                ("question-1".to_string(), json!("failed")),
                ("bash-1".to_string(), json!("failed")),
            ]
        );
    }

    /// The ± the TUI draws, as data — and taken from the raw input, so an
    /// edit far past the 16 KiB detail cap still diffs. That is the case
    /// the page needs most: `detail` is not even valid JSON up there.
    #[test]
    fn an_edit_carries_the_diff_the_tui_draws_however_large_it_is() {
        let input = json!({ "path": "f.rs", "old_string": "a\nb\nc", "new_string": "a\nB\nc" });
        let projected = project_tool_call("edit-1", "edit", &input);
        assert_eq!(
            projected["diff"],
            json!([
                { "kind": "ctx", "text": "a" },
                { "kind": "del", "text": "b" },
                { "kind": "add", "text": "B" },
                { "kind": "ctx", "text": "c" },
            ])
        );

        // Long enough that the pretty input is cut, short enough that the
        // diff's own line cap is not: 300 lines, ~13 KiB a side.
        let old = "the quick brown fox jumps over the lazy dog\n".repeat(300);
        let huge = json!({
            "path": "f.rs",
            "old_string": old,
            "new_string": format!("{old}tail\n"),
        });
        let projected = project_tool_call("edit-2", "edit", &huge);
        let detail = projected["detail"].as_str().unwrap();
        assert!(
            detail.ends_with(ilar::text::DETAIL_TRUNCATED),
            "the premise: the pretty input was cut"
        );
        assert!(
            serde_json::from_str::<Value>(detail).is_err(),
            "and is no longer parseable, so the page cannot diff it itself"
        );
        let diff = projected["diff"].as_array().expect("a diff all the same");
        assert_eq!(
            diff.last().unwrap(),
            &json!({ "kind": "add", "text": "tail" })
        );

        // A write is its file, as pure additions; every other tool has no
        // diff and the key is still there.
        let written = project_tool_call(
            "write-1",
            "write",
            &json!({ "path": "f", "content": "a\nb" }),
        );
        assert_eq!(
            written["diff"],
            json!([
                { "kind": "add", "text": "a" },
                { "kind": "add", "text": "b" },
            ])
        );
        // A new file is pure addition, so the LCS line cap — which is
        // there to bound a match that never runs here — must not send a
        // 500-line file back to truncated JSON.
        let long = "one line of a new file\n".repeat(500);
        let big_write = project_tool_call("write-3", "write", &json!({ "content": long }));
        assert_eq!(big_write["diff"].as_array().map(Vec::len), Some(500));

        let read = project_tool_call("read-1", "read", &json!({ "path": "f" }));
        assert_eq!(read["diff"], Value::Null);
        assert!(read.as_object().unwrap().contains_key("diff"));
        // Past the diff caps there is no diff, and the page falls back.
        let enormous = "x".repeat(300 * 1024);
        assert_eq!(
            project_tool_call("write-2", "write", &json!({ "content": enormous }))["diff"],
            Value::Null
        );
    }

    /// A background task's report arrives wrapped in the envelope that
    /// routed it. The projection unwraps it exactly as the TUI's task row
    /// does, and leaves `text` verbatim beside it.
    #[test]
    fn a_task_notification_is_projected_as_a_headline_and_a_body() {
        let envelope = "<task-notification>\nTask \"review\" completed.\n<result>\nAll good.\n\
                        Two files changed.\n</result>\n</task-notification>";
        let projected = project_event(&user(envelope, Vec::new()));
        assert_eq!(
            projected["notification"],
            json!({
                "kind": "task",
                "headline": "review completed.",
                "body": "All good.\nTwo files changed.",
            })
        );
        assert_eq!(
            projected["text"], envelope,
            "the envelope still crosses verbatim: the shape is additive"
        );

        let job = project_event(&user(
            "<tool-notification>\nBackground job 3 finished\n<result>\nexit 0\n</result>\n\
             </tool-notification>",
            Vec::new(),
        ));
        assert_eq!(job["notification"]["kind"], "job");
        assert_eq!(job["notification"]["headline"], "3 finished");
        assert_eq!(job["notification"]["body"], "exit 0");

        // An ordinary message — nearly all of them — carries the key and
        // nothing in it.
        let typed = project_event(&user("<task-notification> is a thing I typed", Vec::new()));
        assert_eq!(typed["notification"], Value::Null);
    }

    /// The link that exists *while* a subagent runs, which
    /// `ToolResult.child_session_id` does not.
    #[test]
    fn an_invocation_is_found_before_it_has_said_anything() {
        let events = vec![meta("zai/glm-4.7"), invocation("task-1")];
        assert!(has_invocation(&events, "task-1"));
        assert!(
            invocation_slice(&events, "task-1").is_empty(),
            "the premise: an empty slice is not the same answer"
        );
        assert!(!has_invocation(&events, "task-2"));
    }

    #[test]
    fn an_empty_session_totals_to_zero_dollars() {
        let totals = usage_totals(&[]);
        assert_eq!(totals["input"], json!(0));
        assert_eq!(totals["cost_dollars"], json!(0.0));
    }
}
