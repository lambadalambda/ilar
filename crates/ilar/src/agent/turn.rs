//! One user turn: provider call(s) + tool execution until the model
//! stops calling tools. Pure state machine — persists via the session
//! store, publishes to the event channel, never touches a UI.

use anyhow::Result;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent::event::{LoopEvent, publish};
use crate::provider::{ProviderEvent, ProviderResolver, Request, StopReason};
use crate::session::{ContentBlock, SessionEvent, SessionStore, Usage, new_id};
use crate::tools::ToolRegistry;
use crate::tools::executor::{CallOutcome, ToolCall, execute_calls};
use chrono::Utc;

/// Loop tuning knobs.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// Max provider calls per user turn (tool-loop guard).
    pub max_iterations: usize,
    /// Context window in tokens; compaction triggers above
    /// `context_limit * compaction_threshold`. None uses the resolver's
    /// model-specific default, or disables compaction if it has none.
    pub context_limit: Option<u64>,
    pub compaction_threshold: f64,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            context_limit: None,
            compaction_threshold: 0.85,
        }
    }
}

/// How a user turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Model finished with no tool calls.
    Completed,
    /// User aborted (Esc): stream cancelled, running tools cancelled,
    /// partial transcript persisted.
    Aborted,
    /// Hit the max-iterations guard.
    MaxIterations,
}

/// Accumulated blocks from one provider call.
#[derive(Default)]
struct StepAccumulator {
    content: Vec<ContentBlock>,
    thinking_open: Option<usize>,
    tool_indices: std::collections::HashMap<String, usize>,
    completed_calls: std::collections::HashSet<String>,
    /// Tool-call ids that already got a ToolStarted announcement.
    announced_calls: std::collections::HashMap<String, String>,
    usage: Usage,
    stop_reason: Option<StopReason>,
}

impl StepAccumulator {
    fn content_blocks(&self) -> Vec<ContentBlock> {
        self.content
            .iter()
            .map(|block| match block {
                ContentBlock::Thinking {
                    text,
                    signature: None,
                } => ContentBlock::Diagnostic { text: text.clone() },
                block => block.clone(),
            })
            .collect()
    }

    fn push_text(&mut self, delta: String) {
        self.thinking_open = None;
        match self.content.last_mut() {
            Some(ContentBlock::Text { text }) => text.push_str(&delta),
            _ => self.content.push(ContentBlock::Text { text: delta }),
        }
    }

    fn push_thinking(&mut self, delta: String) {
        let index = match self.thinking_open {
            Some(index) => index,
            None => {
                self.content.push(ContentBlock::Thinking {
                    text: String::new(),
                    signature: None,
                });
                let index = self.content.len() - 1;
                self.thinking_open = Some(index);
                index
            }
        };
        if let ContentBlock::Thinking { text, .. } = &mut self.content[index] {
            text.push_str(&delta);
        }
    }

    fn complete_thinking(&mut self, signature: Option<String>) {
        if let Some(index) = self.thinking_open.take()
            && let ContentBlock::Thinking {
                signature: stored, ..
            } = &mut self.content[index]
        {
            *stored = signature;
        }
    }

    fn push_reasoning(&mut self, item: serde_json::Value) {
        self.thinking_open = None;
        self.content.push(ContentBlock::Reasoning { item });
    }

    fn start_tool_call(&mut self, id: String, name: String) {
        self.thinking_open = None;
        if self.tool_indices.contains_key(&id) {
            return;
        }
        self.content.push(ContentBlock::ToolCall {
            id: id.clone(),
            name,
            input: serde_json::Value::Null,
        });
        self.tool_indices.insert(id, self.content.len() - 1);
    }

    fn complete_tool_call(&mut self, id: String, name: String, input: serde_json::Value) {
        self.start_tool_call(id.clone(), name.clone());
        if let Some(index) = self.tool_indices.get(&id).copied() {
            let completed_id = id.clone();
            self.content[index] = ContentBlock::ToolCall { id, name, input };
            self.completed_calls.insert(completed_id);
        }
    }

    fn tool_calls(&self) -> Vec<(&String, &String, &serde_json::Value, bool)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, name, input } => {
                    Some((id, name, input, self.completed_calls.contains(id)))
                }
                _ => None,
            })
            .collect()
    }
}

const MAX_TOOL_ARGUMENT_SUMMARY_CHARS: usize = 512;

fn summarize_tool_input(name: &str, input: &serde_json::Value) -> String {
    let string = |key: &str| {
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(collapse_whitespace)
    };
    let summary = match name {
        "bash" => string("command").map(|command| redact_command(&command)),
        "read" => string("path").map(|path| {
            let offset = input.get("offset").and_then(serde_json::Value::as_u64);
            let limit = input.get("limit").and_then(serde_json::Value::as_u64);
            match (offset, limit) {
                (Some(offset), Some(limit)) => format!("{path}:{offset}+{limit}"),
                (Some(offset), None) => format!("{path}:{offset}"),
                _ => path,
            }
        }),
        "write" | "edit" => string("path"),
        "grep" => string("pattern").map(|pattern| match string("path") {
            Some(path) => format!("/{pattern}/ · {path}"),
            None => format!("/{pattern}/"),
        }),
        "glob" => string("pattern"),
        "task" => string("description").map(|description| match string("subagent_type") {
            Some(agent) => format!("{description} · {agent}"),
            None => description,
        }),
        _ => input.as_object().map(|values| {
            values
                .iter()
                .filter_map(|(key, value)| {
                    if sensitive_key(key) {
                        return Some(format!("{key}=<redacted>"));
                    }
                    match value {
                        serde_json::Value::String(value) => {
                            Some(format!("{key}={}", collapse_whitespace(value)))
                        }
                        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
                            Some(format!("{key}={value}"))
                        }
                        _ => None,
                    }
                })
                .take(3)
                .collect::<Vec<_>>()
                .join(" · ")
        }),
    }
    .unwrap_or_default();
    summary
        .chars()
        .take(MAX_TOOL_ARGUMENT_SUMMARY_CHARS)
        .collect()
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "token",
        "secret",
        "password",
        "authorization",
        "apikey",
        "privatekey",
        "credential",
        "cookie",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn redact_command(command: &str) -> String {
    let mut redact_next = false;
    let mut allow_authorization_scheme = false;
    command
        .split_whitespace()
        .map(|token| {
            if redact_next {
                let normalized = token.trim_matches(['\'', '"', ',']);
                if allow_authorization_scheme
                    && (normalized.eq_ignore_ascii_case("bearer")
                        || normalized.eq_ignore_ascii_case("basic"))
                {
                    allow_authorization_scheme = false;
                    return token.to_string();
                }
                redact_next = false;
                allow_authorization_scheme = false;
                return "<redacted>".to_string();
            }
            let normalized = token.trim_matches(['\'', '"', ',']);
            if normalized.starts_with("sk-")
                || normalized.starts_with("ghp_")
                || normalized.starts_with("github_pat_")
            {
                return "<redacted>".to_string();
            }
            let lower = normalized.to_ascii_lowercase();
            if let Some(position) = lower.find("authorization:") {
                let value = lower[position + "authorization:".len()..].trim();
                if value.is_empty() {
                    redact_next = true;
                    allow_authorization_scheme = true;
                    return token.to_string();
                }
                if value == "bearer" || value == "basic" {
                    redact_next = true;
                    return token.to_string();
                }
                return "Authorization:<redacted>".to_string();
            }
            let (key, value) = token.split_once('=').unwrap_or((token, ""));
            let key_name = key.trim_start_matches('-');
            let key_is_label = !value.is_empty() || key.starts_with('-') || key.ends_with(':');
            if key_is_label && sensitive_key(key_name) {
                if value.is_empty() {
                    redact_next = true;
                    return token.to_string();
                }
                return format!("{key}=<redacted>");
            }
            if normalized.eq_ignore_ascii_case("bearer") {
                redact_next = true;
                return token.to_string();
            }
            token.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run one user turn to completion.
///
/// Flow: append the user message, then repeat { call provider, stream
/// events, persist the assistant message, execute tool calls through the
/// barrier executor, persist results } until the model stops calling
/// tools, the abort token fires, or the iteration guard trips.
///
/// Provider errors abort the turn with `Err`; the partial transcript
/// (user message + any completed assistant messages) stays in the
/// session.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    resolver: &dyn ProviderResolver,
    registry: &ToolRegistry,
    store: &SessionStore,
    session_id: &str,
    user_input: &str,
    system_prompt: Option<&str>,
    config: LoopConfig,
    events: tokio::sync::mpsc::UnboundedSender<LoopEvent>,
    cancel: CancellationToken,
    mut tool_ctx: crate::tools::ToolContext,
) -> Result<TurnOutcome> {
    let mut session = store.acquire_writer(session_id)?.load()?;
    let model = session.effective_model();
    let provider = resolver.resolve_provider(&model)?;
    session.append(SessionEvent::UserMessage {
        id: new_id(),
        text: user_input.to_string(),
        ts: Utc::now(),
    })?;
    publish(&events, LoopEvent::TurnStarted);

    // Compaction runs once per user turn, before the provider loop.
    let context_limit = config
        .context_limit
        .or_else(|| resolver.context_limit(&model));
    if let (Some(limit), threshold) = (context_limit, config.compaction_threshold)
        && crate::compaction::compact_if_needed_locked(
            provider.as_provider(),
            &model,
            &mut session,
            limit,
            threshold,
        )
        .await?
    {
        publish(
            &events,
            LoopEvent::Compacted {
                context_tokens: crate::compaction::estimate_tokens(&session),
            },
        );
    }

    let tools = registry.definitions();
    tool_ctx.session_id = session_id.to_string();

    for _ in 0..config.max_iterations {
        if cancel.is_cancelled() {
            publish(
                &events,
                LoopEvent::TurnDone {
                    outcome: TurnOutcome::Aborted,
                },
            );
            return Ok(TurnOutcome::Aborted);
        }

        let request = Request {
            model: model.clone(),
            system_prompt: system_prompt.map(String::from),
            messages: session.transcript(),
            tools: tools.clone(),
            options: serde_json::Value::Null,
        };

        let mut stream = provider.as_provider().stream(request)?;
        let mut acc = StepAccumulator::default();
        let mut aborted = false;
        let mut errored: Option<String> = None;

        loop {
            let next = tokio::select! {
                next = stream.next() => next,
                _ = cancel.cancelled() => {
                    aborted = true;
                    break;
                }
            };
            let Some(event) = next else { break };
            match event {
                ProviderEvent::TextDelta(t) => {
                    publish(&events, LoopEvent::TextDelta(t.clone()));
                    acc.push_text(t);
                }
                ProviderEvent::ThinkingDelta(t) => {
                    publish(&events, LoopEvent::ThinkingDelta(t.clone()));
                    acc.push_thinking(t);
                }
                ProviderEvent::ThinkingCompleted { signature } => {
                    acc.complete_thinking(signature);
                }
                ProviderEvent::ReasoningItem { item } => {
                    acc.push_reasoning(item);
                }
                ProviderEvent::ToolCallStarted { id, name } => {
                    if !acc.announced_calls.contains_key(&id) {
                        acc.announced_calls.insert(id.clone(), name.clone());
                        acc.start_tool_call(id.clone(), name.clone());
                        publish(&events, LoopEvent::ToolStarted { id, name });
                    }
                }
                ProviderEvent::ToolCallInputDelta { .. } => {}
                ProviderEvent::ToolCallCompleted { id, name, input } => {
                    // Some streams (and test scripts) skip Started; announce
                    // lazily so the UI always sees the pair.
                    if !acc.announced_calls.contains_key(&id) {
                        acc.announced_calls.insert(id.clone(), name.clone());
                        publish(
                            &events,
                            LoopEvent::ToolStarted {
                                id: id.clone(),
                                name: name.clone(),
                            },
                        );
                    }
                    publish(
                        &events,
                        LoopEvent::ToolArguments {
                            id: id.clone(),
                            arguments: summarize_tool_input(&name, &input),
                        },
                    );
                    acc.complete_tool_call(id, name, input);
                }
                ProviderEvent::TurnComplete { stop_reason, usage } => {
                    acc.stop_reason = Some(stop_reason.clone());
                    acc.usage = usage;
                    break;
                }
                ProviderEvent::Error(message) => {
                    errored = Some(message);
                    break;
                }
            }
        }
        drop(stream); // abort the underlying request

        // A stream that ended without TurnComplete or Error is a broken
        // provider/connection — treat it as an error, not a clean step.
        let errored = errored.or_else(|| {
            (acc.stop_reason.is_none() && !aborted)
                .then(|| "stream ended before completion".to_string())
        });

        if let Some(message) = errored {
            // Persist the partial step so the UI's already-shown deltas
            // don't evaporate from the transcript.
            let blocks = acc.content_blocks();
            if !blocks.is_empty() {
                session.append(SessionEvent::AssistantMessage {
                    id: new_id(),
                    model: model.clone(),
                    content: blocks,
                    usage: acc.usage,
                    stop_reason: "error".into(),
                    ts: Utc::now(),
                })?;
            }
            let calls = acc.tool_calls();
            let completed_ids: std::collections::HashSet<&str> =
                calls.iter().map(|(id, _, _, _)| id.as_str()).collect();
            for (id, name, _, _) in calls {
                session.append(SessionEvent::ToolResult {
                    id: new_id(),
                    tool_use_id: id.clone(),
                    content: format!("provider error before execution: {message}"),
                    is_error: true,
                    ts: Utc::now(),
                })?;
                publish(
                    &events,
                    LoopEvent::ToolFinished {
                        id: id.clone(),
                        name: name.clone(),
                        is_error: true,
                    },
                );
            }
            for (id, name) in &acc.announced_calls {
                if !completed_ids.contains(id.as_str()) {
                    publish(
                        &events,
                        LoopEvent::ToolFinished {
                            id: id.clone(),
                            name: name.clone(),
                            is_error: true,
                        },
                    );
                }
            }
            anyhow::bail!(message);
        }

        if aborted {
            // Persist the partial assistant message so the session is
            // resumable...
            let blocks = acc.content_blocks();
            if !blocks.is_empty() {
                session.append(SessionEvent::AssistantMessage {
                    id: new_id(),
                    model: model.clone(),
                    content: blocks,
                    usage: acc.usage,
                    stop_reason: "aborted".into(),
                    ts: Utc::now(),
                })?;
            }
            // ...and answer every announced tool call with a synthetic
            // error result: an unanswered tool_use poisons the transcript
            // (providers 400 on tool_use without tool_result).
            for (id, name, _, _) in acc.tool_calls() {
                session.append(SessionEvent::ToolResult {
                    id: new_id(),
                    tool_use_id: id.clone(),
                    content: "aborted before execution".into(),
                    is_error: true,
                    ts: Utc::now(),
                })?;
                publish(
                    &events,
                    LoopEvent::ToolFinished {
                        id: id.clone(),
                        name: name.clone(),
                        is_error: true,
                    },
                );
            }
            publish(
                &events,
                LoopEvent::TurnDone {
                    outcome: TurnOutcome::Aborted,
                },
            );
            return Ok(TurnOutcome::Aborted);
        }

        // Persist the completed assistant message.
        let blocks = acc.content_blocks();
        let had_tool_calls = !acc.tool_indices.is_empty();
        let stop_reason = acc
            .stop_reason
            .clone()
            .map(|r| match r {
                StopReason::EndTurn => "end_turn".to_string(),
                StopReason::ToolUse => "tool_use".to_string(),
                StopReason::MaxTokens => "max_tokens".to_string(),
                StopReason::Refusal => "refusal".to_string(),
                StopReason::Paused => "paused".to_string(),
                StopReason::Stopped => "stopped".to_string(),
            })
            .unwrap_or_else(|| "unknown".into());
        if !blocks.is_empty() {
            session.append(SessionEvent::AssistantMessage {
                id: new_id(),
                model: model.clone(),
                content: blocks,
                usage: acc.usage,
                stop_reason: stop_reason.clone(),
                ts: Utc::now(),
            })?;
        }
        publish(
            &events,
            LoopEvent::StepComplete {
                stop_reason: stop_reason.clone(),
                usage: acc.usage,
            },
        );

        if !had_tool_calls {
            publish(
                &events,
                LoopEvent::TurnDone {
                    outcome: TurnOutcome::Completed,
                },
            );
            return Ok(TurnOutcome::Completed);
        }

        // Never execute incomplete or null-input calls. Keep result order
        // aligned with the streamed call order.
        let ordered_calls = acc.tool_calls();
        let calls: Vec<ToolCall> = ordered_calls
            .iter()
            .filter(|(_, _, input, completed)| *completed && !input.is_null())
            .map(|(id, name, input, _)| ToolCall {
                id: (*id).clone(),
                name: (*name).clone(),
                input: (*input).clone(),
            })
            .collect();
        let mut call_ctx = tool_ctx.clone();
        call_ctx.cancel = cancel.clone();
        let outcomes =
            execute_calls(calls, |name| registry.get(name), call_ctx, cancel.clone()).await;
        let mut outcomes = outcomes.into_iter();
        for (id, name, input, completed) in ordered_calls {
            let outcome = if completed && !input.is_null() {
                outcomes.next().expect("one outcome per executable call")
            } else {
                CallOutcome {
                    id: id.clone(),
                    name: name.clone(),
                    output: crate::tools::ToolOutput::error(
                        "tool call was incomplete or had invalid arguments",
                    ),
                    cancelled: false,
                }
            };
            session.append(SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: outcome.id.clone(),
                content: outcome.output.content,
                is_error: outcome.output.is_error,
                ts: Utc::now(),
            })?;
            publish(
                &events,
                LoopEvent::ToolFinished {
                    id: outcome.id,
                    name: outcome.name,
                    is_error: outcome.output.is_error,
                },
            );
        }
    }

    publish(
        &events,
        LoopEvent::TurnDone {
            outcome: TurnOutcome::MaxIterations,
        },
    );
    Ok(TurnOutcome::MaxIterations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_argument_summaries_are_bounded_and_redacted() {
        let write = summarize_tool_input(
            "write",
            &serde_json::json!({"path": "src/main.rs", "content": "x".repeat(50_000)}),
        );
        assert_eq!(write, "src/main.rs");

        let bash = summarize_tool_input(
            "bash",
            &serde_json::json!({
                "command": "curl -H 'Authorization: Bearer eyJhbGci.opaque.jwt' --header=Authorization:Basic opaque-basic --api-key=also-secret"
            }),
        );
        assert!(!bash.contains("eyJhbGci"), "{bash}");
        assert!(!bash.contains("opaque-basic"), "{bash}");
        assert!(!bash.contains("also-secret"), "{bash}");
        assert!(bash.contains("<redacted>"), "{bash}");

        let custom = summarize_tool_input(
            "custom",
            &serde_json::json!({"apiKey": "secret", "cookie": "session", "query": "safe"}),
        );
        assert!(!custom.contains("secret"), "{custom}");
        assert!(!custom.contains("session"), "{custom}");
        assert!(custom.chars().count() <= MAX_TOOL_ARGUMENT_SUMMARY_CHARS);
    }
}
