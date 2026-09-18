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

/// Drives one turn and keeps everything the next request needs: the
/// thinking as streamed, and the first tool call as the model made it.
async fn first_step(
    provider: &OpenCodeProvider,
    model: &str,
) -> (String, Option<(String, String, serde_json::Value)>) {
    let mut stream = provider.stream(tool_request(model)).unwrap();
    let mut thinking = String::new();
    let mut call = None;
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::ThinkingDelta(t) => thinking.push_str(&t),
            ProviderEvent::ToolCallCompleted { id, name, input } if call.is_none() => {
                call = Some((id, name, input));
            }
            ProviderEvent::TurnComplete { .. } => break,
            ProviderEvent::Error(e) => panic!("{model}: provider error: {e}"),
            ProviderEvent::RetryableError(e) => panic!("{model}: retryable error: {e}"),
            _ => {}
        }
    }
    (thinking, call)
}

/// The chat-wire rows are a proxy to upstreams nobody can read from
/// here: the one way to know they take `reasoning_content` back on the
/// assistant message inside a turn is to send it. Every family that
/// streams thinking gets its own thought and its own tool call echoed
/// with a result, and must answer rather than refuse.
#[tokio::test]
#[ignore]
async fn live_chat_rows_take_their_thinking_back() {
    use ilar::session::{ChatMessage, ContentBlock, Role};
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
        let (thinking, call) = first_step(provider, model).await;
        let Some((id, name, input)) = call else {
            println!("{model}: no tool call on the first step; nothing to echo");
            continue;
        };
        println!("{model}: thinking_bytes={} call={name}", thinking.len());
        let mut request = tool_request(model);
        request.messages.push(ChatMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: if thinking.is_empty() {
                        "(the model streamed no thinking; echoing a stand-in)".into()
                    } else {
                        thinking
                    },
                },
                ContentBlock::ToolCall {
                    id: id.clone(),
                    name,
                    input,
                    item_id: None,
                },
            ],
        });
        request.messages.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id,
                content: "wrote /tmp/probe.txt".into(),
                is_error: false,
                images: Vec::new(),
            }],
        });
        let mut stream = provider.stream(request).unwrap();
        let mut answered = false;
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TurnComplete { stop_reason, .. } => {
                    println!("{model}: second step stop={stop_reason:?}");
                    answered = true;
                    break;
                }
                ProviderEvent::Error(e) => panic!("{model}: refused the echoed thinking: {e}"),
                ProviderEvent::RetryableError(e) => panic!("{model}: retryable error: {e}"),
                _ => {}
            }
        }
        assert!(answered, "{model}: no terminal event on the second step");
    }
}
