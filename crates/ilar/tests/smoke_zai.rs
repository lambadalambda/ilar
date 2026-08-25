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
            item_id: None,
        }],
    });
    messages.push(ilar::session::ChatMessage {
        role: ilar::session::Role::User,
        content: vec![ilar::session::ContentBlock::ToolResult {
            tool_use_id: call_id,
            content: "22C, clear skies, wind 3m/s".into(),
            is_error: false,
            images: Vec::new(),
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

/// Big system prompt (>1024 tokens — the cacheable minimum) so prompt
/// caching engages, then a growing conversation: turn 1 should write the
/// cache, turns 2+ should READ it instead of re-ingesting the prefix.
#[tokio::test]
#[ignore]
async fn live_prompt_caching_anthropic() {
    let key = std::env::var("ILAR_ZAI_API_KEY").expect("ILAR_ZAI_API_KEY");
    let provider = ZaiProvider::new(key, None, Flavor::Anthropic);
    let filler = "You are a meticulous code assistant. Treat every task with \
care and precision, verify assumptions against the actual source, prefer \
minimal diffs, and explain reasoning briefly. "
        .repeat(60);
    let base = Request {
        system_prompt: Some(filler),
        tools: vec![ilar::provider::ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        ..Request::with_model("zai/glm-4.7")
    };

    let drain = |provider: ZaiProvider, req: Request| async move {
        let mut stream = provider.stream(req).unwrap();
        let mut text = String::new();
        let mut usage = None;
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TextDelta(t) => text.push_str(&t),
                ProviderEvent::TurnComplete { usage: u, .. } => {
                    usage = Some(u);
                    break;
                }
                ProviderEvent::Error(e) => panic!("provider error: {e}"),
                _ => {}
            }
        }
        (text, usage.expect("no terminal event"))
    };

    let turn1 = Request {
        messages: vec![ilar::session::ChatMessage::user_text(
            "Remember the word PINEAPPLE. Just say ok.",
        )],
        ..base.clone()
    };
    let (text1, usage1) = drain(provider.clone(), turn1).await;
    println!("turn 1: {text1:?} usage: {usage1:?}");
    assert!(!text1.is_empty());

    let turn2 = Request {
        messages: vec![
            ilar::session::ChatMessage::user_text("Remember the word PINEAPPLE. Just say ok."),
            ilar::session::ChatMessage {
                role: ilar::session::Role::Assistant,
                content: vec![ilar::session::ContentBlock::Text {
                    text: text1.clone(),
                }],
            },
            ilar::session::ChatMessage::user_text("What word did I ask you to remember?"),
        ],
        ..base.clone()
    };
    let (text2, usage2) = drain(provider.clone(), turn2).await;
    println!("turn 2: {text2:?} usage: {usage2:?}");
    assert!(
        text2.to_uppercase().contains("PINEAPPLE"),
        "model lost context: {text2}"
    );

    let turn3 = Request {
        messages: vec![
            ilar::session::ChatMessage::user_text("Remember the word PINEAPPLE. Just say ok."),
            ilar::session::ChatMessage {
                role: ilar::session::Role::Assistant,
                content: vec![ilar::session::ContentBlock::Text {
                    text: text1.clone(),
                }],
            },
            ilar::session::ChatMessage::user_text("What word did I ask you to remember?"),
            ilar::session::ChatMessage {
                role: ilar::session::Role::Assistant,
                content: vec![ilar::session::ContentBlock::Text {
                    text: text2.clone(),
                }],
            },
            ilar::session::ChatMessage::user_text("And say it backwards now."),
        ],
        ..base
    };
    let (text3, usage3) = drain(provider, turn3).await;
    println!("turn 3: {text3:?} usage: {usage3:?}");

    // The assertions that matter: the bulk of the prefix is served from
    // cache on every follow-up turn, and only the new tail is processed
    // fresh. (z.ai accounting notes: cache_creation_input_tokens is never
    // reported — the turn-1 write shows up as plain input_tokens — and
    // reads are reported at their entry granularity.)
    assert!(
        usage2.cache_read_input_tokens >= 1500,
        "turn 2 did not serve the bulk from cache: {usage2:?}"
    );
    assert!(
        usage2.input_tokens <= 500,
        "turn 2 re-ingested the whole prompt: {usage2:?}"
    );
    assert!(
        usage3.cache_read_input_tokens >= usage2.cache_read_input_tokens,
        "turn 3 cache read regressed: {usage2:?} -> {usage3:?}"
    );
    assert!(
        usage3.input_tokens <= 500,
        "turn 3 re-ingested the whole prompt: {usage3:?}"
    );
}

/// Regression smoke for the glm-5.3 buffered-stream incident: with tools
/// present, the OpenAI-compatible endpoint must stream tool arguments
/// incrementally (tool_stream), not buffer the whole turn server-side.
#[tokio::test]
#[ignore]
async fn live_openai_flavor_glm53_tool_call_streams_incrementally() {
    let key = std::env::var("ILAR_ZAI_API_KEY").expect("ILAR_ZAI_API_KEY");
    let provider = ZaiProvider::new(key, None, Flavor::OpenAI);
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
