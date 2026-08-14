//! Live smoke test against the real z.ai API. Ignored by default.
//!
//! Run with:
//!   cargo test -p ilar --test smoke_zai -- --ignored --nocapture
//!
//! Requires ILAR_ZAI_API_KEY in the environment (fish it out of
//! ~/.local/share/opencode/auth.json -> ["zai-coding-plan"].key).

use futures::StreamExt;
use ilar::provider::zai::{Flavor, ZaiProvider};
use ilar::provider::{Provider, ProviderEvent, Request, StopReason};

#[tokio::test]
#[ignore]
async fn live_anthropic_flavor_text_turn() {
    let key = std::env::var("ILAR_ZAI_API_KEY").expect("ILAR_ZAI_API_KEY");
    let provider = ZaiProvider::new(key, None, Flavor::Anthropic);
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
    println!("stop: {:?}", terminal);
    assert!(!text.is_empty());
    assert_eq!(terminal, Some(StopReason::EndTurn));
}

#[tokio::test]
#[ignore]
async fn live_anthropic_flavor_tool_roundtrip() {
    let key = std::env::var("ILAR_ZAI_API_KEY").expect("ILAR_ZAI_API_KEY");
    let provider = ZaiProvider::new(key, None, Flavor::Anthropic);
    let request = Request {
        system_prompt: Some("You are terse. Use the tool when asked.".into()),
        tools: vec![ilar::provider::ToolDefinition {
            name: "get_weather".into(),
            description: "Get current weather for a city".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        }],
        messages: vec![ilar::session::ChatMessage::user_text(
            "What's the weather in Tokyo? Use the tool.",
        )],
        ..Request::with_model("zai/glm-4.7")
    };

    // Turn 1: expect a tool call.
    let events: Vec<ProviderEvent> = {
        let mut stream = provider.stream(request.clone()).unwrap();
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::Error(e) => panic!("provider error: {e}"),
                other => events.push(other),
            }
        }
        events
    };
    let call = events.iter().find_map(|e| match e {
        ProviderEvent::ToolCallCompleted {
            id, name, input, ..
        } => Some((id.clone(), name.clone(), input.clone())),
        _ => None,
    });
    let Some((call_id, name, input)) = call else {
        panic!("no tool call in turn 1: {events:?}");
    };
    println!("tool call: {name} {input}");
    assert_eq!(name, "get_weather");
    assert_eq!(input["city"], "Tokyo");

    // Turn 2: send the tool result back, expect text mentioning weather.
    let mut messages = request.messages.clone();
    messages.push(ilar::session::ChatMessage {
        role: ilar::session::Role::Assistant,
        content: vec![ilar::session::ContentBlock::ToolCall {
            id: call_id.clone(),
            name: name.clone(),
            input: input.clone(),
        }],
    });
    messages.push(ilar::session::ChatMessage {
        role: ilar::session::Role::User,
        content: vec![ilar::session::ContentBlock::ToolResult {
            tool_use_id: call_id,
            content: "22C, clear skies, wind 3m/s".into(),
            is_error: false,
        }],
    });
    let request2 = Request {
        messages,
        ..request
    };
    let mut stream = provider.stream(request2).unwrap();
    let mut text = String::new();
    let mut terminal = None;
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(t) => text.push_str(&t),
            ProviderEvent::TurnComplete { stop_reason, .. } => {
                terminal = Some(stop_reason);
                break;
            }
            ProviderEvent::Error(e) => panic!("provider error turn 2: {e}"),
            _ => {}
        }
    }
    println!("turn 2 text: {text}");
    assert!(text.to_lowercase().contains("22") || text.to_lowercase().contains("clear"));
    assert_eq!(terminal, Some(StopReason::EndTurn));
}

#[tokio::test]
#[ignore]
async fn live_openai_flavor_text_turn() {
    let key = std::env::var("ILAR_ZAI_API_KEY").expect("ILAR_ZAI_API_KEY");
    let provider = ZaiProvider::new(key, None, Flavor::OpenAI);
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
    println!("openai-flavor text: {text}");
    assert!(!text.is_empty());
    assert_eq!(terminal, Some(StopReason::EndTurn));
}
