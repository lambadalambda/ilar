use futures::StreamExt;
use ilar::provider::{
    MockProvider, Provider, ProviderEvent, Request, StopReason, ToolDefinition, resolve_model,
};
use ilar::session::{ChatMessage, ContentBlock, Role, Usage};

fn text_turn() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta("Hello".into()),
        ProviderEvent::TextDelta(", world".into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
    ]
}

fn tool_turn() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta("Let me check".into()),
        ProviderEvent::ToolCallStarted {
            id: "toolu_1".into(),
            name: "read".into(),
        },
        ProviderEvent::ToolCallInputDelta {
            id: "toolu_1".into(),
            delta: "{\"path\":".into(),
        },
        ProviderEvent::ToolCallInputDelta {
            id: "toolu_1".into(),
            delta: " \"Cargo.toml\"}".into(),
        },
        ProviderEvent::ToolCallCompleted {
            id: "toolu_1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "Cargo.toml"}),
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ]
}

#[tokio::test]
async fn mock_streams_scripted_events_through_trait() {
    let provider = MockProvider::new(vec![text_turn()]);
    let mut stream = provider.stream(Request::with_model("zai/glm-4.7")).unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }

    assert_eq!(events.len(), 3);
    assert_eq!(ProviderEvent::text_of(&events), "Hello, world");
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }
        })
    );
}

#[tokio::test]
async fn mock_serves_scripted_turns_in_order_then_repeats_last() {
    let provider = MockProvider::new(vec![text_turn(), tool_turn()]);
    let drain = |provider: &MockProvider| {
        let mut stream = provider.stream(Request::with_model("m")).unwrap();
        async move {
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                events.push(event);
            }
            events
        }
    };

    let first = drain(&provider).await;
    let second = drain(&provider).await;
    let third = drain(&provider).await;

    assert_eq!(
        first.last().unwrap().clone().stop_reason(),
        Some(StopReason::EndTurn)
    );
    assert_eq!(
        second.last().unwrap().clone().stop_reason(),
        Some(StopReason::ToolUse)
    );
    // Out of script: last turn repeats (keeps loop tests simple).
    assert_eq!(
        third.last().unwrap().clone().stop_reason(),
        Some(StopReason::ToolUse)
    );
}

#[tokio::test]
async fn mock_records_requests_for_assertions() {
    let provider = MockProvider::new(vec![text_turn()]);
    let request = Request {
        system_prompt: Some("be terse".into()),
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        ..Request::with_model("zai/glm-4.7")
    };
    let _ = provider.stream(request.clone()).unwrap();

    let recorded = provider.requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].model, "zai/glm-4.7");
    assert_eq!(recorded[0].system_prompt.as_deref(), Some("be terse"));
    assert_eq!(recorded[0].tools.len(), 1);
    assert_eq!(recorded[0].messages.len(), 1);
}

#[tokio::test]
async fn error_event_terminates_stream() {
    let provider = MockProvider::error("boom");
    let mut stream = provider.stream(Request::with_model("m")).unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    assert_eq!(events, vec![ProviderEvent::Error("boom".into())]);
}

#[tokio::test]
async fn dropping_stream_cancels_without_consuming_rest() {
    let provider = MockProvider::new(vec![text_turn()]);
    let mut stream = provider.stream(Request::with_model("m")).unwrap();
    assert!(matches!(
        stream.next().await,
        Some(ProviderEvent::TextDelta(_))
    ));
    drop(stream); // must not hang, panic, or require draining
    assert_eq!(provider.requests().len(), 1);
}

#[test]
fn resolve_model_splits_provider_and_id() {
    assert_eq!(
        resolve_model("openai/gpt-5.2").unwrap(),
        ("openai", "gpt-5.2")
    );
}

#[tokio::test]
async fn thinking_events_stream_and_complete() {
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ThinkingDelta("hmm".into()),
        ProviderEvent::ThinkingDelta(" hmm".into()),
        ProviderEvent::ThinkingCompleted {
            signature: Some("sig123".into()),
        },
        ProviderEvent::TextDelta("Answer".into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]]);
    let mut stream = provider.stream(Request::with_model("zai/glm-4.7")).unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    assert_eq!(events.len(), 5);
    assert!(matches!(
        &events[2],
        ProviderEvent::ThinkingCompleted { signature: Some(s) } if s == "sig123"
    ));
}
