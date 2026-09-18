//! Live smoke tests against the real OpenCode gateways. Ignored by default.
//!
//! Run with:
//!   ILAR_OPENCODE_API_KEY=... cargo test -p ilar --test smoke_opencode -- --ignored --nocapture
//!
//! Each test asks for one tool call so both wires are exercised end to
//! end through ilar's own mappers, not just reached.

use futures::StreamExt;
use ilar::provider::opencode::OpenCodeProvider;
use ilar::provider::{Provider, ProviderEvent, Request, StopReason, ToolDefinition};

fn key() -> String {
    std::env::var("ILAR_OPENCODE_API_KEY").expect("ILAR_OPENCODE_API_KEY")
}

fn tool_request(model: &str) -> Request {
    Request {
        system_prompt: Some("You are a terse coding agent. Use the tool when asked.".into()),
        tools: vec![ToolDefinition {
            name: "write".into(),
            description: "Write a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"]
            }),
        }],
        messages: vec![ilar::session::ChatMessage::user_text(
            "Call the write tool once to write ok to /tmp/probe.txt, then stop.",
        )],
        ..Request::with_model(model)
    }
}

/// Drives one turn and returns the tool calls made and the stop reason.
async fn turn(provider: &OpenCodeProvider, model: &str) -> (Vec<String>, Option<StopReason>) {
    let mut stream = provider.stream(tool_request(model)).unwrap();
    let mut calls = Vec::new();
    let mut text = String::new();
    let mut thinking = 0usize;
    let mut terminal = None;
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::ToolCallStarted { name, .. } => calls.push(name),
            ProviderEvent::TextDelta(t) => text.push_str(&t),
            ProviderEvent::ThinkingDelta(t) => thinking += t.len(),
            ProviderEvent::TurnComplete { stop_reason, usage } => {
                println!(
                    "{model}: stop={stop_reason:?} usage={usage:?} thinking_bytes={thinking} text={text:?}"
                );
                terminal = Some(stop_reason);
                break;
            }
            ProviderEvent::Error(e) => panic!("{model}: provider error: {e}"),
            ProviderEvent::RetryableError(e) => panic!("{model}: retryable error: {e}"),
            _ => {}
        }
    }
    (calls, terminal)
}

#[tokio::test]
#[ignore]
async fn live_go_chat_wire_calls_a_tool() {
    let provider = OpenCodeProvider::go(key(), None);
    let (calls, stop) = turn(&provider, "opencode-go/glm-5.3").await;
    assert_eq!(calls, ["write"]);
    assert_eq!(stop, Some(StopReason::ToolUse));
}

#[tokio::test]
#[ignore]
async fn live_go_responses_wire_calls_a_tool() {
    let provider = OpenCodeProvider::go(key(), None);
    let (calls, stop) = turn(&provider, "opencode-go/gpt-5.6-luna").await;
    assert_eq!(calls, ["write"]);
    assert_eq!(stop, Some(StopReason::ToolUse));
}

#[tokio::test]
#[ignore]
async fn live_zen_kimi_reasons_then_calls_a_tool() {
    let provider = OpenCodeProvider::zen(key(), None);
    let (calls, stop) = turn(&provider, "opencode/kimi-k3").await;
    assert_eq!(calls, ["write"]);
    assert_eq!(stop, Some(StopReason::ToolUse));
}

#[tokio::test]
#[ignore]
async fn live_zen_grok_on_responses_calls_a_tool() {
    let provider = OpenCodeProvider::zen(key(), None);
    let (calls, stop) = turn(&provider, "opencode/grok-4.6").await;
    assert_eq!(calls, ["write"]);
    assert_eq!(stop, Some(StopReason::ToolUse));
}

#[tokio::test]
#[ignore]
async fn live_go_qwen_on_the_chat_wire_calls_a_tool() {
    let provider = OpenCodeProvider::go(key(), None);
    let (calls, stop) = turn(&provider, "opencode-go/qwen3.8-flash").await;
    assert_eq!(calls, ["write"]);
    assert_eq!(stop, Some(StopReason::ToolUse));
}

#[tokio::test]
#[ignore]
async fn live_go_minimax_on_the_chat_wire_calls_a_tool() {
    let provider = OpenCodeProvider::go(key(), None);
    let (calls, stop) = turn(&provider, "opencode-go/minimax-m3").await;
    assert_eq!(calls, ["write"]);
    assert_eq!(stop, Some(StopReason::ToolUse));
}

/// One provider step over an arbitrary conversation: what it thought,
/// the first tool call it made, and what it said — everything the next
/// request has to carry back.
struct Step {
    thinking: String,
    /// The field the thinking arrived under, when the wire said it was
    /// not the default: it goes back under the same one.
    field: Option<ilar::session::ReasoningField>,
    call: Option<(String, String, serde_json::Value)>,
    text: String,
}

async fn step(provider: &OpenCodeProvider, model: &str, request: Request) -> Step {
    let mut stream = provider.stream(request).unwrap();
    let mut step = Step {
        thinking: String::new(),
        field: None,
        call: None,
        text: String::new(),
    };
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::ThinkingDelta(t) => step.thinking.push_str(&t),
            ProviderEvent::ThinkingField(field) => step.field = Some(field),
            ProviderEvent::TextDelta(t) => step.text.push_str(&t),
            ProviderEvent::ToolCallCompleted { id, name, input } if step.call.is_none() => {
                step.call = Some((id, name, input));
            }
            ProviderEvent::TurnComplete { stop_reason, .. } => {
                println!(
                    "{model}: stop={stop_reason:?} thinking_bytes={} field={:?} call={:?} text={:?}",
                    step.thinking.len(),
                    step.field,
                    step.call.as_ref().map(|(_, name, _)| name.as_str()),
                    step.text.chars().take(60).collect::<String>()
                );
                break;
            }
            ProviderEvent::Error(e) => panic!("{model}: refused: {e}"),
            ProviderEvent::RetryableError(e) => panic!("{model}: retryable error: {e}"),
            _ => {}
        }
    }
    step
}

/// The step's own thought and call, as the wire will carry them back,
/// plus the result the tool "gave". A step that made no call is just
/// its words.
fn echo(step: Step, result: &str) -> Vec<ilar::session::ChatMessage> {
    use ilar::session::{ChatMessage, ContentBlock, Role};
    let mut content = Vec::new();
    if !step.thinking.is_empty() {
        content.push(ContentBlock::Thinking {
            text: step.thinking,
            field: step.field,
        });
    }
    if !step.text.is_empty() {
        content.push(ContentBlock::Text { text: step.text });
    }
    let Some((id, name, input)) = step.call else {
        return vec![ChatMessage {
            role: Role::Assistant,
            content,
        }];
    };
    content.push(ContentBlock::ToolCall {
        id: id.clone(),
        name,
        input,
        item_id: None,
    });
    vec![
        ChatMessage {
            role: Role::Assistant,
            content,
        },
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id,
                content: result.into(),
                is_error: false,
                images: Vec::new(),
            }],
        },
    ]
}

/// The chat-wire rows are a proxy to upstreams nobody can read from
/// here: the one way to know they take their thinking back is to send
/// it. Two turns under the default, `all`, so the second turn's
/// requests carry the first turn's thinking as well as their own — a
/// server that only takes the current turn's would refuse here.
#[tokio::test]
#[ignore]
async fn live_chat_rows_take_their_thinking_back() {
    let go = OpenCodeProvider::go(key(), None);
    let zen = OpenCodeProvider::zen(key(), None);
    let rows: [(&OpenCodeProvider, &str); 5] = [
        (&go, "opencode-go/qwen3.8-flash"),
        (&go, "opencode-go/minimax-m3"),
        (&go, "opencode-go/kimi-k2.6"),
        (&go, "opencode-go/deepseek-v4-flash"),
        (&zen, "opencode/kimi-k3"),
    ];
    for (provider, model) in rows {
        // Turn one: the call, then its thought and call echoed with a
        // result, then the model's answer.
        let mut request = tool_request(model);
        let first = step(provider, model, request.clone()).await;
        assert!(
            first.call.is_some(),
            "{model}: no tool call on the first step"
        );
        request.messages.extend(echo(first, "wrote /tmp/probe.txt"));
        let answer = step(provider, model, request.clone()).await;
        request.messages.extend(echo(answer, ""));

        // Turn two: a new prompt ahead of all of that; under `all`,
        // turn one's thinking rides along with turn two's.
        request.messages.push(ilar::session::ChatMessage::user_text(
            "Now call the write tool once more to write ok2 to /tmp/probe2.txt, then stop.",
        ));
        let second = step(provider, model, request.clone()).await;
        assert!(
            second.call.is_some(),
            "{model}: no tool call on the second turn"
        );
        request
            .messages
            .extend(echo(second, "wrote /tmp/probe2.txt"));
        let done = step(provider, model, request).await;
        assert!(
            done.call.is_none() || !done.text.is_empty() || !done.thinking.is_empty(),
            "{model}: the second turn did not end"
        );
    }
}
