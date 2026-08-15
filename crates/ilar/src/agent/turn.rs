//! One user turn: provider call(s) + tool execution until the model
//! stops calling tools. Pure state machine — persists via the session
//! store, publishes to the event channel, never touches a UI.

use anyhow::Result;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent::event::{LoopEvent, publish};
use crate::provider::{Provider, ProviderEvent, Request, StopReason};
use crate::session::{ContentBlock, SessionEvent, SessionStore, Usage, new_id};
use crate::tools::ToolRegistry;
use crate::tools::executor::{ToolCall, execute_calls};
use chrono::Utc;

/// Loop tuning knobs.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// Max provider calls per user turn (tool-loop guard).
    pub max_iterations: usize,
    /// Context window in tokens; compaction triggers above
    /// `context_limit * compaction_threshold`. None disables compaction.
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
    text: String,
    thinking: Option<(String, Option<String>)>, // text, signature
    tool_calls: Vec<ContentBlock>,
    /// Tool-call ids that already got a ToolStarted announcement.
    announced_calls: std::collections::HashMap<String, String>,
    usage: Usage,
    stop_reason: Option<StopReason>,
}

impl StepAccumulator {
    fn content_blocks(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::new();
        if let Some((text, signature)) = &self.thinking {
            blocks.push(ContentBlock::Thinking {
                text: text.clone(),
                signature: signature.clone(),
            });
        }
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: self.text.clone(),
            });
        }
        blocks.extend(self.tool_calls.iter().cloned());
        blocks
    }
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
    provider: &dyn Provider,
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
    let mut session = store.load(session_id)?;
    session.append(SessionEvent::UserMessage {
        id: new_id(),
        text: user_input.to_string(),
        ts: Utc::now(),
    })?;
    publish(&events, LoopEvent::TurnStarted);

    // Compaction runs once per user turn, before the provider loop.
    if let (Some(limit), threshold) = (config.context_limit, config.compaction_threshold)
        && crate::compaction::compact_if_needed(provider, store, session_id, limit, threshold)
            .await?
    {
        publish(&events, LoopEvent::Compacted);
        // Reload with the compaction event applied.
        session = store.load(session_id)?;
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
            model: session.effective_model(),
            system_prompt: system_prompt.map(String::from),
            messages: session.transcript(),
            tools: tools.clone(),
            options: serde_json::Value::Null,
        };

        let mut stream = provider.stream(request)?;
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
                    acc.text.push_str(&t);
                }
                ProviderEvent::ThinkingDelta(t) => {
                    publish(&events, LoopEvent::ThinkingDelta(t.clone()));
                    acc.thinking
                        .get_or_insert_with(|| (String::new(), None))
                        .0
                        .push_str(&t);
                }
                ProviderEvent::ThinkingCompleted { signature } => {
                    if let Some(thinking) = acc.thinking.as_mut() {
                        thinking.1 = signature;
                    }
                }
                ProviderEvent::ToolCallStarted { id, name } => {
                    if !acc.announced_calls.contains_key(&id) {
                        acc.announced_calls.insert(id.clone(), name.clone());
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
                    acc.tool_calls
                        .push(ContentBlock::ToolCall { id, name, input });
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
                    model: session.effective_model(),
                    content: blocks,
                    usage: acc.usage,
                    stop_reason: "error".into(),
                    ts: Utc::now(),
                })?;
            }
            let completed_ids: std::collections::HashSet<&str> = acc
                .tool_calls
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect();
            for block in &acc.tool_calls {
                if let ContentBlock::ToolCall { id, name, .. } = block {
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
                    model: session.effective_model(),
                    content: blocks,
                    usage: acc.usage,
                    stop_reason: "aborted".into(),
                    ts: Utc::now(),
                })?;
            }
            // ...and answer every announced tool call with a synthetic
            // error result: an unanswered tool_use poisons the transcript
            // (providers 400 on tool_use without tool_result).
            for block in &acc.tool_calls {
                if let ContentBlock::ToolCall { id, .. } = block {
                    session.append(SessionEvent::ToolResult {
                        id: new_id(),
                        tool_use_id: id.clone(),
                        content: "aborted before execution".into(),
                        is_error: true,
                        ts: Utc::now(),
                    })?;
                }
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
        let had_tool_calls = !acc.tool_calls.is_empty();
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
                model: session.effective_model(),
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

        // Execute the turn's tool calls under the barrier, persist results.
        let calls: Vec<ToolCall> = acc
            .tool_calls
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, name, input } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect();
        let outcomes = execute_calls(
            calls,
            |name| registry.get(name),
            tool_ctx.clone(),
            cancel.clone(),
        )
        .await;
        for outcome in outcomes {
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
