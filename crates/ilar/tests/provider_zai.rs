use futures::StreamExt;
use ilar::provider::zai::{Flavor, ZaiProvider};
use ilar::provider::{Provider, ProviderEvent, Request, StopReason, ToolDefinition};
use ilar::session::{ChatMessage, ContentBlock, InputTokenAccounting, Role, Usage};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("tests/fixtures/{name}"))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn request() -> Request {
    Request {
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        ..Request::with_model("zai/glm-4.7")
    }
}

fn http_server(sse_body: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
    let listener = futures::executor::block_on(async {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
    });
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut req = Vec::new();
        loop {
            let mut buf = [0u8; 65536];
            let n = socket.read(&mut buf).await.expect("read request");
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&req);
            if let Some(head_end) = text.find("\r\n\r\n") {
                let content_length = text
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if req.len() >= head_end + 4 + content_length {
                    break;
                }
            }
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        for chunk in sse_body.chunks(37) {
            socket.write_all(chunk).await.unwrap();
            socket.flush().await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        String::from_utf8_lossy(&req).into_owned()
    });
    (format!("http://{addr}"), handle)
}

fn http_error_server(body: String) -> String {
    let listener = futures::executor::block_on(async {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
    });
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let _ = socket.read(&mut request).await;
        let response = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    format!("http://{addr}")
}

fn anthropic_provider(base_url: String) -> ZaiProvider {
    ZaiProvider::new("test-key".into(), Some(base_url), Flavor::Anthropic)
}

async fn drain(mut stream: ilar::provider::EventStream) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn assert_terminated_blank(fixtures: &[&str]) {
    for name in fixtures {
        let path = format!("tests/fixtures/{name}");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.ends_with("\n\n"),
            "fixture {name} must end with a blank line"
        );
    }
}

#[tokio::test]
async fn text_fixture_maps_to_neutral_events() {
    assert_terminated_blank(&["zai_text.sse"]);
    let (base, _server) = http_server(fixture("zai_text.sse"));
    let events = drain(anthropic_provider(base).stream(request()).unwrap()).await;

    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::TextDelta(", world".into()),
            ProviderEvent::ResponseContent {
                provider: "zai-anthropic".into(),
                content: serde_json::json!([{"type": "text", "text": "Hello, world"}]),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 6,
                    input_token_accounting: Some(InputTokenAccounting::ExcludesCached),
                    ..Default::default()
                },
            },
        ]
    );
}

#[tokio::test]
async fn tool_call_fixture_maps_thinking_and_tool_use() {
    assert_terminated_blank(&["zai_toolcall.sse"]);
    let (base, _server) = http_server(fixture("zai_toolcall.sse"));
    let events = drain(anthropic_provider(base).stream(request()).unwrap()).await;

    assert!(matches!(
        &events[0],
        ProviderEvent::ThinkingDelta(t) if t == "User wants the config"
    ));
    assert_eq!(
        events[1],
        ProviderEvent::ThinkingCompleted {
            signature: Some("sig_abc".into())
        }
    );
    assert_eq!(
        events[2],
        ProviderEvent::ToolCallStarted {
            id: "toolu_01".into(),
            name: "read".into(),
        }
    );
    assert_eq!(
        events[3],
        ProviderEvent::ToolCallInputDelta {
            id: "toolu_01".into(),
            delta: "{\"path\":".into(),
        }
    );
    assert!(matches!(
        &events[5],
        ProviderEvent::ToolCallCompleted { id, name, input }
            if id == "toolu_01" && name == "read" && input == &serde_json::json!({"path": "Cargo.toml"})
    ));
    assert_eq!(events[6], ProviderEvent::TextDelta("Reading it.".into()));
    let complete = events.last().unwrap();
    assert_eq!(complete.clone().stop_reason(), Some(StopReason::ToolUse));
    assert!(matches!(
        complete,
        ProviderEvent::TurnComplete { usage, .. }
            if usage.cache_read_input_tokens == 128 && usage.output_tokens == 30
    ));
}

#[tokio::test]
async fn anthropic_signature_deltas_concatenate() {
    let sse = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"thought\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"tail\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let (base, _server) = http_server(sse.as_bytes().to_vec());
    let events = drain(anthropic_provider(base).stream(request()).unwrap()).await;

    assert_eq!(
        events[1],
        ProviderEvent::ThinkingCompleted {
            signature: Some("sig-tail".into())
        }
    );
}

#[tokio::test]
async fn anthropic_pause_content_is_replayed_unchanged() {
    let sse = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Searching.\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_1\",\"name\":\"web_search\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"query\\\":\\\"news\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"pause_turn\"},\"usage\":{}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = drain(
        anthropic_provider(http_server(sse.as_bytes().to_vec()).0)
            .stream(request())
            .unwrap(),
    )
    .await;
    let continuation = events
        .iter()
        .find_map(|event| match event {
            ProviderEvent::ResponseContent { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("paused continuation");
    assert_eq!(continuation[0]["text"], "Searching.");
    assert_eq!(
        continuation[1]["input"],
        serde_json::json!({"query": "news"})
    );
    assert_eq!(
        events.last().and_then(ProviderEvent::stop_reason),
        Some(StopReason::Paused)
    );

    let mut replay = request();
    replay.messages = vec![ChatMessage::user_text("search")];
    replay.continuations = vec![continuation.clone()];
    let body = ZaiProvider::new("k".into(), None, Flavor::Anthropic).wire_body_for_test(&replay);
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"], continuation);
}

#[test]
fn unsigned_thinking_is_diagnostic_text_on_anthropic_wire() {
    let provider = ZaiProvider::new("k".into(), None, Flavor::Anthropic);
    let mut req = request();
    req.messages = vec![ChatMessage {
        role: Role::Assistant,
        content: vec![ContentBlock::Thinking {
            text: "unfinished".into(),
            signature: None,
        }],
    }];

    let body = provider.wire_body_for_test(&req);
    assert!(body["messages"].as_array().unwrap().is_empty());
}

#[test]
fn incomplete_tool_input_is_normalized_for_anthropic_replay() {
    let provider = ZaiProvider::new("k".into(), None, Flavor::Anthropic);
    let mut req = request();
    req.messages = vec![ChatMessage {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolCall {
            id: "incomplete".into(),
            name: "read".into(),
            input: serde_json::Value::Null,
        }],
    }];

    let body = provider.wire_body_for_test(&req);
    assert_eq!(
        body["messages"][0]["content"][0]["input"],
        serde_json::json!({})
    );
}

#[test]
fn malformed_anthropic_replay_is_rejected_preflight() {
    let provider = ZaiProvider::new("k".into(), None, Flavor::Anthropic);
    let mut request = request();
    request.messages = vec![ChatMessage {
        role: Role::Assistant,
        content: vec![ContentBlock::ProviderReplay {
            provider: "zai-anthropic".into(),
            content: serde_json::json!([{
                "type": "tool_use",
                "id": "hidden",
                "name": "read",
                "input": {}
            }]),
        }],
    }];

    let error = provider.stream(request).err().expect("malformed replay");
    assert!(
        error.to_string().contains("does not match neutral"),
        "{error:#}"
    );
}

#[tokio::test]
async fn openai_reasoning_runs_close_at_content_and_tool_boundaries() {
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think-1\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"text-1\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think-2\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{}}\n\n",
    );
    let (base, _server) = http_server(sse.as_bytes().to_vec());
    let provider = ZaiProvider::new("k".into(), Some(base), Flavor::OpenAI);
    let events = drain(provider.stream(request()).unwrap()).await;

    assert_eq!(events[0], ProviderEvent::ThinkingDelta("think-1".into()));
    assert_eq!(
        events[1],
        ProviderEvent::ThinkingCompleted { signature: None }
    );
    assert_eq!(events[2], ProviderEvent::TextDelta("text-1".into()));
    assert_eq!(events[3], ProviderEvent::ThinkingDelta("think-2".into()));
    assert_eq!(
        events[4],
        ProviderEvent::ThinkingCompleted { signature: None }
    );
    assert!(matches!(events[5], ProviderEvent::ToolCallStarted { .. }));
}

#[tokio::test]
async fn error_fixture_maps_to_error_event() {
    assert_terminated_blank(&["zai_error.sse"]);
    let (base, _server) = http_server(fixture("zai_error.sse"));
    let events = drain(anthropic_provider(base).stream(request()).unwrap()).await;
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], ProviderEvent::RetryableError(m) if m.contains("Overloaded")));
}

#[tokio::test]
async fn http_error_body_is_bounded_and_redacted() {
    let body = format!("api_key=super-secret {}", "padding".repeat(20_000));
    let base = http_error_server(body);
    let provider = ZaiProvider::new("super-secret".into(), Some(base), Flavor::Anthropic);

    let events = drain(provider.stream(request()).unwrap()).await;

    let ProviderEvent::RetryableError(error) = &events[0] else {
        panic!("expected HTTP error: {events:?}");
    };
    assert!(
        error.len() < 66_000,
        "error was not bounded: {}",
        error.len()
    );
    assert!(!error.contains("super-secret"), "{error}");
    assert!(error.contains("[truncated]"), "{error}");
}

#[tokio::test]
async fn truncated_tool_call_synthesizes_null_completion() {
    assert_terminated_blank(&["zai_truncated.sse"]);
    let (base, _server) = http_server(fixture("zai_truncated.sse"));
    let events = drain(anthropic_provider(base).stream(request()).unwrap()).await;

    assert!(matches!(
        &events[0],
        ProviderEvent::ToolCallStarted { id, name } if id == "toolu_02" && name == "edit"
    ));
    assert!(matches!(
        &events[2],
        ProviderEvent::ToolCallCompleted { id, input, .. }
            if id == "toolu_02" && input == &serde_json::Value::Null
    ));
    assert_eq!(events[3].clone().stop_reason(), Some(StopReason::MaxTokens));
}

#[tokio::test]
async fn malformed_anthropic_payload_cannot_complete_as_truncated() {
    let mut body = b"data: {not-json}\n\n".to_vec();
    body.extend(fixture("zai_truncated.sse"));
    let base = http_server(body).0;

    let events = drain(anthropic_provider(base).stream(request()).unwrap()).await;

    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(&events[0], ProviderEvent::Error(error) if error.contains("JSON")));
}

#[tokio::test]
async fn malformed_anthropic_arguments_are_terminal() {
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{bad\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let base = http_server(body.as_bytes().to_vec()).0;

    let events = drain(anthropic_provider(base).stream(request()).unwrap()).await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("arguments"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::TurnComplete { .. }))
    );
}

#[tokio::test]
async fn anthropic_empty_initial_argument_delta_is_valid() {
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"read\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = drain(
        anthropic_provider(http_server(body.as_bytes().to_vec()).0)
            .stream(request())
            .unwrap(),
    )
    .await;

    assert!(matches!(
        &events[1],
        ProviderEvent::ToolCallCompleted { input, .. } if input == &serde_json::json!({})
    ));
    assert_eq!(
        events.last().and_then(ProviderEvent::stop_reason),
        Some(StopReason::ToolUse)
    );
}

#[tokio::test]
async fn malformed_openai_compatible_arguments_are_terminal() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{bad\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{}}\n\n",
    );
    let base = http_server(body.as_bytes().to_vec()).0;
    let provider = ZaiProvider::new("k".into(), Some(base), Flavor::OpenAI);

    let events = drain(provider.stream(request()).unwrap()).await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("arguments"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::TurnComplete { .. }))
    );
}

#[tokio::test]
async fn openai_compatible_arguments_require_a_started_call() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{}}\n\n",
    );
    let provider = ZaiProvider::new(
        "k".into(),
        Some(http_server(body.as_bytes().to_vec()).0),
        Flavor::OpenAI,
    );
    let events = drain(provider.stream(request()).unwrap()).await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("before tool start")),
        "{events:?}"
    );
}

#[tokio::test]
async fn anthropic_duplicate_ids_and_stop_reasons_are_terminal() {
    let duplicate_id = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"read\"}}\n\n",
    );
    let events = drain(
        anthropic_provider(http_server(duplicate_id.as_bytes().to_vec()).0)
            .stream(request())
            .unwrap(),
    )
    .await;
    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("duplicate Anthropic tool id"))
    );

    let duplicate_stop = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = drain(
        anthropic_provider(http_server(duplicate_stop.as_bytes().to_vec()).0)
            .stream(request())
            .unwrap(),
    )
    .await;
    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("duplicate Anthropic stop reason"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::TurnComplete { .. }))
    );
}

#[tokio::test]
async fn anthropic_completed_block_index_cannot_be_reused() {
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"first\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"replacement\"}}\n\n",
    );
    let events = drain(
        anthropic_provider(http_server(body.as_bytes().to_vec()).0)
            .stream(request())
            .unwrap(),
    )
    .await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("duplicate Anthropic content block index")),
        "{events:?}"
    );
}

#[tokio::test]
async fn anthropic_stop_reason_must_match_tool_calls() {
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = drain(
        anthropic_provider(http_server(body.as_bytes().to_vec()).0)
            .stream(request())
            .unwrap(),
    )
    .await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("contradicts")),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::TurnComplete { .. }))
    );
}

#[tokio::test]
async fn anthropic_content_after_stop_reason_is_terminal() {
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"late\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let events = drain(
        anthropic_provider(http_server(body.as_bytes().to_vec()).0)
            .stream(request())
            .unwrap(),
    )
    .await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("after stop reason")),
        "{events:?}"
    );
}

#[tokio::test]
async fn openai_compatible_done_completes_a_finished_stream_without_usage() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let provider = ZaiProvider::new(
        "k".into(),
        Some(http_server(body.as_bytes().to_vec()).0),
        Flavor::OpenAI,
    );
    let events = drain(provider.stream(request()).unwrap()).await;

    assert_eq!(
        events.last().and_then(ProviderEvent::stop_reason),
        Some(StopReason::EndTurn),
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ProviderEvent::TurnComplete { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn openai_compatible_rejects_content_after_finish_reason() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"late\"},\"finish_reason\":null}]}\n\n",
    );
    let provider = ZaiProvider::new(
        "k".into(),
        Some(http_server(body.as_bytes().to_vec()).0),
        Flavor::OpenAI,
    );
    let events = drain(provider.stream(request()).unwrap()).await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("after finish_reason")),
        "{events:?}"
    );
}

#[tokio::test]
async fn openai_compatible_duplicate_tool_ids_are_terminal() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"same\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"same\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{}}\n\n",
    );
    let base = http_server(body.as_bytes().to_vec()).0;
    let provider = ZaiProvider::new("k".into(), Some(base), Flavor::OpenAI);

    let events = drain(provider.stream(request()).unwrap()).await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("duplicate OpenAI-compatible tool id"))
    );
}

#[tokio::test]
async fn wire_format_anthropic_flavor() {
    assert_terminated_blank(&["zai_text.sse"]);
    let (base, server) = http_server(fixture("zai_text.sse"));
    let mut req = request();
    req.system_prompt = Some("be terse".into());
    req.messages = vec![
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "read it".into(),
            }],
        },
        ChatMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "hmm".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::ToolCall {
                    id: "toolu_01".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "Cargo.toml"}),
                },
            ],
        },
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_01".into(),
                content: "1: model".into(),
                is_error: false,
            }],
        },
    ];
    let stream = anthropic_provider(base).stream(req).unwrap();
    let wire = server.await.unwrap();
    drop(stream);

    assert!(wire.starts_with("POST /v1/messages"));
    assert!(wire.contains("x-api-key: test-key"));
    assert!(wire.contains("anthropic-version:"));
    let body_start = wire.find("\r\n\r\n").unwrap() + 4;
    let body: serde_json::Value = serde_json::from_str(wire[body_start..].trim()).unwrap();
    assert_eq!(body["model"], "glm-4.7");
    // System is a block array with a cache breakpoint.
    assert_eq!(body["system"][0]["text"], "be terse");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["stream"], true);
    assert!(body["max_tokens"].as_u64().unwrap() > 0);
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    // Thinking block round-trips (Anthropic requires it with tool use).
    assert_eq!(messages[1]["content"][0]["type"], "thinking");
    assert_eq!(messages[1]["content"][0]["signature"], "sig");
    assert_eq!(messages[1]["content"][1]["type"], "tool_use");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_01");
}

#[tokio::test]
async fn openai_flavor_uses_chat_completions_endpoint() {
    let listener = futures::executor::block_on(async {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
    });
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut req = Vec::new();
        loop {
            let mut buf = [0u8; 65536];
            let n = socket.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&req);
            if let Some(head_end) = text.find("\r\n\r\n") {
                let content_length = text
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if req.len() >= head_end + 4 + content_length {
                    break;
                }
            }
        }
        let body = "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n";
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        String::from_utf8_lossy(&req).into_owned()
    });

    let provider = ZaiProvider::new(
        "test-key".into(),
        Some(format!("http://{addr}")),
        Flavor::OpenAI,
    );
    let stream = provider.stream(request()).unwrap();
    let wire = handle.await.unwrap();
    drop(stream);

    assert!(wire.starts_with("POST /chat/completions"));
    assert!(wire.contains("Bearer test-key"));
    let body_start = wire.find("\r\n\r\n").unwrap() + 4;
    let body: serde_json::Value = serde_json::from_str(wire[body_start..].trim()).unwrap();
    assert_eq!(body["model"], "glm-4.7");
    assert_eq!(body["stream"], true);
}

#[test]
fn openai_flavor_serializes_system_and_tool_results_in_protocol_order() {
    let provider = ZaiProvider::new("k".into(), None, Flavor::OpenAI);
    let mut req = request();
    req.system_prompt = Some("follow instructions".into());
    req.messages = vec![
        ChatMessage::user_text("read both"),
        ChatMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "one"}),
                },
                ContentBlock::ToolCall {
                    id: "call_2".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "two"}),
                },
            ],
        },
        ChatMessage {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "one result".into(),
                    is_error: false,
                },
                ContentBlock::Text {
                    text: "continue with this context".into(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call_2".into(),
                    content: "two result".into(),
                    is_error: false,
                },
            ],
        },
    ];

    let body = serde_json::to_value(provider.wire_body_for_test(&req)).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 6, "messages: {messages:?}");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "follow instructions");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_1");
    assert_eq!(messages[4]["role"], "tool");
    assert_eq!(messages[4]["tool_call_id"], "call_2");
    assert_eq!(messages[5]["role"], "user");
    assert_eq!(messages[5]["content"], "continue with this context");
    assert_eq!(body["stream_options"]["include_usage"], true);
    // Without tool_stream, z.ai buffers entire tool-call responses
    // server-side (verified against glm-5.3: first byte arrived only when
    // the whole response was done). Must always be requested.
    assert_eq!(body["tool_stream"], true);
    assert!(body.get("system").is_none());
}

#[tokio::test]
async fn invalid_model_id_is_preflight_error() {
    let provider = ZaiProvider::new("k".into(), None, Flavor::Anthropic);
    assert!(provider.stream(Request::with_model("glm-4.7")).is_err());
}

#[tokio::test]
async fn rejects_model_for_another_provider() {
    let provider = ZaiProvider::new("k".into(), None, Flavor::Anthropic);
    let error = provider
        .stream(Request::with_model("openai/gpt-5.2"))
        .err()
        .expect("provider mismatch must fail preflight");
    assert!(error.to_string().contains("expected zai"));
}

#[tokio::test]
async fn reserved_options_are_rejected_before_zai_network_io() {
    for flavor in [Flavor::Anthropic, Flavor::OpenAI] {
        let provider = ZaiProvider::new("k".into(), Some("http://127.0.0.1:1".into()), flavor);
        let keys: &[&str] = match flavor {
            Flavor::Anthropic => &["model", "messages", "tools", "stream"],
            Flavor::OpenAI => &[
                "model",
                "messages",
                "tools",
                "stream",
                "stream_options",
                "tool_stream",
            ],
        };
        for key in keys {
            let mut request = request();
            request.options = serde_json::Value::Object(
                [(key.to_string(), serde_json::json!("override"))]
                    .into_iter()
                    .collect(),
            );
            let error = provider.stream(request).err().expect("reserved option");
            assert!(error.to_string().contains(key), "{error:#}");
        }
        let mut request = request();
        request.options = serde_json::json!(true);
        assert!(provider.stream(request).is_err());
    }
}

// ---- prompt caching ----

#[tokio::test]
async fn cache_breakpoints_placed() {
    assert_terminated_blank(&["zai_text.sse"]);
    let (base, server) = http_server(fixture("zai_text.sse"));
    let mut req = request();
    req.system_prompt = Some("sys".into());
    req.messages = vec![
        ChatMessage::user_text("one"),
        ChatMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "t".into(),
                    signature: Some("s".into()),
                },
                ContentBlock::ToolCall {
                    id: "toolu_01".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "x"}),
                },
            ],
        },
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_01".into(),
                content: "out".into(),
                is_error: false,
            }],
        },
    ];
    let stream = anthropic_provider(base).stream(req).unwrap();
    let wire = server.await.unwrap();
    drop(stream);
    let body_start = wire.find("\r\n\r\n").unwrap() + 4;
    let body: serde_json::Value = serde_json::from_str(wire[body_start..].trim()).unwrap();

    // System breakpoint.
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    // Last tool gets the breakpoint, earlier tools don't.
    let tools = body["tools"].as_array().unwrap();
    assert!(tools[0].get("cache_control").is_none() || tools.len() == 1);
    assert_eq!(tools.last().unwrap()["cache_control"]["type"], "ephemeral");
    // Last message's last block gets the breakpoint; earlier blocks don't.
    let messages = body["messages"].as_array().unwrap();
    assert!(messages[0]["content"][0].get("cache_control").is_none());
    assert!(messages[1]["content"][0].get("cache_control").is_none());
    let last_blocks = messages[2]["content"].as_array().unwrap();
    assert_eq!(
        last_blocks.last().unwrap()["cache_control"]["type"],
        "ephemeral"
    );
}

#[test]
fn wire_prefix_stable_across_turns() {
    // Same session, growing transcript: the serialized wire messages of
    // turn 2 must begin with identical serialization of turn 1's messages
    // after stripping cache_control markers. (Anthropic documents that
    // breakpoint placement is not part of the cached-content hash; the
    // moving breakpoint is the canonical incremental pattern. The live
    // test below verifies z.ai honors this — if it ever fails, switch to
    // append-only breakpoints.)
    let strip_markers = |value: serde_json::Value| -> serde_json::Value {
        fn walk(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    map.remove("cache_control");
                    for v in map.values_mut() {
                        walk(v);
                    }
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(walk),
                _ => {}
            }
        }
        let mut v = value;
        walk(&mut v);
        v
    };
    let provider = ZaiProvider::new("k".into(), None, Flavor::Anthropic);
    let u1 = ChatMessage::user_text("what is in the config?");
    let a1 = ChatMessage {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                text: "check file".into(),
                signature: Some("sig".into()),
            },
            ContentBlock::ToolCall {
                id: "toolu_01".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "ilar.toml"}),
            },
        ],
    };
    let u1b = ChatMessage {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_01".into(),
            content: "1: model = zai/glm-4.7".into(),
            is_error: false,
        }],
    };

    let turn1 = Request {
        messages: vec![u1.clone()],
        ..request()
    };
    let turn2 = Request {
        messages: vec![u1, a1, u1b],
        ..request()
    };

    let body1 = serde_json::to_value(provider.wire_body_for_test(&turn1)).unwrap();
    let body2 = serde_json::to_value(provider.wire_body_for_test(&turn2)).unwrap();
    let m1 = serde_json::to_string(&strip_markers(body1["messages"][0].clone())).unwrap();
    let m2 = serde_json::to_string(&strip_markers(body2["messages"][0].clone())).unwrap();
    assert_eq!(
        m1, m2,
        "first message content must be identical across turns"
    );
    // System prompt block identical (markers and all — it never moves).
    assert_eq!(
        serde_json::to_string(&body1["system"]).unwrap(),
        serde_json::to_string(&body2["system"]).unwrap()
    );
}
