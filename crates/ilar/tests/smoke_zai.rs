//! Live smoke test against the real z.ai API. Ignored by default.
//!
//! Run with:
//!   cargo test -p ilar --test smoke_zai -- --ignored --nocapture
//!
//! Requires ILAR_ZAI_API_KEY in the environment (fish it out of
//! ~/.local/share/opencode/auth.json -> ["zai-coding-plan"].key).

use futures::StreamExt;
use ilar::provider::zai::ZaiProvider;
use ilar::provider::{Provider, ProviderEvent, Request, StopReason};

#[tokio::test]
#[ignore]
async fn live_text_turn() {
    let key = std::env::var("ILAR_ZAI_API_KEY").expect("ILAR_ZAI_API_KEY");
    let provider = ZaiProvider::new(key, None);
    let mut stream = provider
        .stream(Request {
            messages: vec![ilar::session::ChatMessage::user_text(
                "Reply with exactly: hello",
            )],
            ..Request::with_model("zai/glm-4.7")
        })
        .unwrap();
    let mut text = String::new();
    let mut terminal = None;
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(t) => text.push_str(&t),
            ProviderEvent::TurnComplete { stop_reason, .. } => {
                terminal = Some(stop_reason);
                break;
            }
            ProviderEvent::Error(e) => panic!("provider error: {e}"),
            _ => {}
        }
    }
    println!("text: {text}");
    assert!(!text.is_empty());
    assert_eq!(terminal, Some(StopReason::EndTurn));
}

/// Regression smoke for the glm-5.3 buffered-stream incident: with tools
/// present, the OpenAI-compatible endpoint must stream tool arguments
/// incrementally (tool_stream), not buffer the whole turn server-side.
#[tokio::test]
#[ignore]
async fn live_glm53_tool_call_streams_incrementally() {
    let key = std::env::var("ILAR_ZAI_API_KEY").expect("ILAR_ZAI_API_KEY");
    let provider = ZaiProvider::new(key, None);
    let request = Request {
        system_prompt: Some("You are a terse coding agent.".into()),
        tools: vec![ilar::provider::ToolDefinition {
            name: "write".into(),
            description: "Write a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"]
            }),
        }],
        messages: vec![ilar::session::ChatMessage::user_text(
            "Write hello.txt containing a 4-line haiku about streams. Use the write tool.",
        )],
        ..Request::with_model("zai/glm-5.3")
    };
    let mut stream = provider.stream(request).unwrap();
    let mut argument_deltas = 0usize;
    let mut completed = None;
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::ToolCallInputDelta { .. } => argument_deltas += 1,
            ProviderEvent::ToolCallCompleted { name, input, .. } => {
                completed = Some((name, input));
            }
            ProviderEvent::Error(e) => panic!("provider error: {e}"),
            _ => {}
        }
    }
    let (name, input) = completed.expect("tool call completed");
    assert_eq!(name, "write");
    assert!(input["content"].is_string(), "{input}");
    // Buffered responses deliver arguments as one blob; incremental
    // streaming produces many small deltas.
    assert!(
        argument_deltas > 1,
        "expected incremental argument deltas, got {argument_deltas}"
    );
}
