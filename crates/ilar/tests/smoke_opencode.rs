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
