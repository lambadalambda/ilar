use futures::StreamExt;
use ilar::provider::{Provider, ProviderEvent, Request, StopReason, ToolDefinition};
use ilar::session::{ContentBlock, Role};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("tests/fixtures/{name}"))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn request_with_tool() -> Request {
    Request {
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        ..Request::with_model("openai/gpt-5.2")
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
        // Read until we have headers + full body per Content-Length.
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

fn provider(base_url: String) -> ilar::provider::openai::OpenAIProvider {
    // base_url convention includes the version segment (like the default).
    ilar::provider::openai::OpenAIProvider::new("test-key".into(), Some(format!("{base_url}/v1")))
}

async fn drain(mut stream: ilar::provider::EventStream) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn text_fixture_maps_to_neutral_events() {
    let base = http_server(fixture("openai_text.sse")).0;
    let events = drain(provider(base).stream(request_with_tool()).unwrap()).await;

    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::TextDelta(", world".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: ilar::session::Usage {
                    input_tokens: 18,
                    output_tokens: 5,
                    input_token_accounting: Some(
                        ilar::session::InputTokenAccounting::ExcludesCached,
                    ),
                    ..Default::default()
                },
            },
        ]
    );
}

#[tokio::test]
async fn tool_call_fixture_maps_to_neutral_events() {
    let base = http_server(fixture("openai_toolcall.sse")).0;
    let events = drain(provider(base).stream(request_with_tool()).unwrap()).await;

    assert_eq!(events.len(), 6);
    assert_eq!(
        events[0],
        ProviderEvent::ToolCallStarted {
            id: "call_1".into(),
            name: "read".into(),
        }
    );
    assert_eq!(
        events[1],
        ProviderEvent::ToolCallInputDelta {
            id: "call_1".into(),
            delta: "{\"path\":".into(),
        }
    );
    assert!(matches!(
        &events[3],
        ProviderEvent::ToolCallCompleted { id, name, input }
            if id == "call_1" && name == "read" && input == &serde_json::json!({"path": "Cargo.toml"})
    ));
    assert_eq!(events[4], ProviderEvent::TextDelta("Reading it.".into()));
    assert_eq!(events[5].clone().stop_reason(), Some(StopReason::ToolUse));
}

#[tokio::test]
async fn error_fixture_maps_to_error_event() {
    let base = http_server(fixture("openai_error.sse")).0;
    let events = drain(provider(base).stream(request_with_tool()).unwrap()).await;
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0],
        ProviderEvent::Error(msg) if msg.contains("overloaded")));
}

#[tokio::test]
async fn top_level_nested_stream_error_exposes_its_message() {
    let body = b"data: {\"type\":\"error\",\"error\":{\"code\":\"rate_limit\",\"message\":\"Please retry shortly\"}}\n\n";
    let events = drain(
        provider(http_server(body.to_vec()).0)
            .stream(request_with_tool())
            .unwrap(),
    )
    .await;

    assert!(matches!(
        &events[0],
        ProviderEvent::Error(error) if error.contains("Please retry shortly")
    ));
}

#[tokio::test]
async fn message_less_stream_error_has_a_sanitized_bounded_fallback() {
    let body = format!(
        "data: {{\"type\":\"error\",\"error\":{{\"code\":\"backend_failure\",\"api_key\":\"super-secret\",\"detail\":\"Authorization: Bearer sk-live {}\"}}}}\n\n",
        "padding".repeat(2_000)
    );
    let events = drain(
        provider(http_server(body.into_bytes()).0)
            .stream(request_with_tool())
            .unwrap(),
    )
    .await;

    let ProviderEvent::Error(error) = &events[0] else {
        panic!("expected provider error: {events:?}");
    };
    assert!(error.contains("backend_failure"), "{error}");
    assert!(!error.contains("super-secret"), "{error}");
    assert!(!error.contains("sk-live"), "{error}");
    assert!(
        error.len() <= 4096,
        "unbounded stream error: {}",
        error.len()
    );
}

#[tokio::test]
async fn http_error_body_is_bounded_and_redacted() {
    let body = format!(
        "Authorization: Bearer super-secret {}",
        "padding".repeat(20_000)
    );
    let base = http_error_server(body);
    let provider = ilar::provider::openai::OpenAIProvider::new(
        "super-secret".into(),
        Some(format!("{base}/v1")),
    );

    let events = drain(provider.stream(request_with_tool()).unwrap()).await;

    let ProviderEvent::Error(error) = &events[0] else {
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
    let base = http_server(fixture("openai_truncated.sse")).0;
    let events = drain(provider(base).stream(request_with_tool()).unwrap()).await;

    // Every Started call must be Completed (event contract), with null
    // input + MaxTokens per the truncation convention.
    assert!(matches!(
        &events[0],
        ProviderEvent::ToolCallStarted { id, name } if id == "call_9" && name == "edit"
    ));
    assert_eq!(
        events[1],
        ProviderEvent::ToolCallInputDelta {
            id: "call_9".into(),
            delta: "{\"path\": \"sr".into(),
        }
    );
    assert!(matches!(
        &events[2],
        ProviderEvent::ToolCallCompleted { id, input, .. }
            if id == "call_9" && input == &serde_json::Value::Null
    ));
    assert_eq!(events[3].clone().stop_reason(), Some(StopReason::MaxTokens));
}

#[tokio::test]
async fn malformed_payload_cannot_be_followed_by_successful_truncation() {
    let mut body = b"data: {not-json}\n\n".to_vec();
    body.extend(fixture("openai_truncated.sse"));
    let base = http_server(body).0;

    let events = drain(provider(base).stream(request_with_tool()).unwrap()).await;

    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(&events[0], ProviderEvent::Error(error) if error.contains("JSON")));
}

#[tokio::test]
async fn malformed_completed_arguments_are_terminal() {
    let body = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{bad\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
    );
    let base = http_server(body.as_bytes().to_vec()).0;

    let events = drain(provider(base).stream(request_with_tool()).unwrap()).await;

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
async fn argument_delta_after_completion_is_terminal() {
    let body = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{}\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{}\"}\n\n",
    );
    let events = drain(
        provider(http_server(body.as_bytes().to_vec()).0)
            .stream(request_with_tool())
            .unwrap(),
    )
    .await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("after completion")),
        "{events:?}"
    );
}

#[tokio::test]
async fn completed_function_item_must_match_started_call() {
    let body = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{}\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"changed\",\"name\":\"read\",\"arguments\":\"{}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
    );
    let events = drain(
        provider(http_server(body.as_bytes().to_vec()).0)
            .stream(request_with_tool())
            .unwrap(),
    )
    .await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("contradicts")),
        "{events:?}"
    );
}

#[tokio::test]
async fn duplicate_completed_function_item_is_terminal() {
    let body = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{}\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"{}\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"{}\"}}\n\n",
    );
    let events = drain(
        provider(http_server(body.as_bytes().to_vec()).0)
            .stream(request_with_tool())
            .unwrap(),
    )
    .await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("duplicate completed")),
        "{events:?}"
    );
}

#[tokio::test]
async fn missing_and_duplicate_openai_tool_fields_are_terminal() {
    let missing = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.incomplete\",\"response\":{\"usage\":{}}}\n\n",
    );
    let events = drain(
        provider(http_server(missing.as_bytes().to_vec()).0)
            .stream(request_with_tool())
            .unwrap(),
    )
    .await;
    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("tool call id"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::TurnComplete { .. }))
    );

    let duplicate = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{}\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{}\"}\n\n",
    );
    let events = drain(
        provider(http_server(duplicate.as_bytes().to_vec()).0)
            .stream(request_with_tool())
            .unwrap(),
    )
    .await;
    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("duplicate completion"))
    );
}

#[tokio::test]
async fn incomplete_reason_must_explicitly_mean_token_truncation() {
    for response in [
        r#"{"usage":{}}"#,
        r#"{"incomplete_details":{"reason":"content_filter"},"usage":{}}"#,
    ] {
        let body =
            format!("data: {{\"type\":\"response.incomplete\",\"response\":{response}}}\n\n");
        let events = drain(
            provider(http_server(body.into_bytes()).0)
                .stream(request_with_tool())
                .unwrap(),
        )
        .await;

        assert!(
            matches!(events.last(), Some(ProviderEvent::Error(_))),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::TurnComplete { .. })),
            "{events:?}"
        );
    }
}

#[tokio::test]
async fn incomplete_refusal_remains_token_truncation() {
    let body = concat!(
        "data: {\"type\":\"response.refusal.delta\",\"delta\":\"I cannot\"}\n\n",
        "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{}}}\n\n",
    );
    let events = drain(
        provider(http_server(body.as_bytes().to_vec()).0)
            .stream(request_with_tool())
            .unwrap(),
    )
    .await;

    assert_eq!(
        events.last().and_then(ProviderEvent::stop_reason),
        Some(StopReason::MaxTokens)
    );
}

#[tokio::test]
async fn terminal_event_stops_pump_before_trailing_payloads() {
    let body = concat!(
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
        "data: {not-json}\n\n",
    );
    let events = drain(
        provider(http_server(body.as_bytes().to_vec()).0)
            .stream(request_with_tool())
            .unwrap(),
    )
    .await;

    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(events[0], ProviderEvent::TurnComplete { .. }));
}

#[tokio::test]
async fn refusal_maps_to_refusal_stop_reason() {
    let base = http_server(fixture("openai_refusal.sse")).0;
    let events = drain(provider(base).stream(request_with_tool()).unwrap()).await;

    assert_eq!(
        events[0],
        ProviderEvent::TextDelta("I can't help with that.".into())
    );
    assert_eq!(events[1].clone().stop_reason(), Some(StopReason::Refusal));
}

#[tokio::test]
async fn invalid_model_id_is_preflight_error() {
    let provider = ilar::provider::openai::OpenAIProvider::new("k".into(), None);
    let err = provider
        .stream(Request::with_model("gpt-5.2"))
        .err()
        .expect("should fail pre-flight");
    assert!(err.to_string().contains("provider/model-id"));
}

#[tokio::test]
async fn rejects_model_for_another_provider() {
    let provider = ilar::provider::openai::OpenAIProvider::new("k".into(), None);
    let error = provider
        .stream(Request::with_model("zai/glm-4.7"))
        .err()
        .expect("provider mismatch must fail preflight");
    assert!(error.to_string().contains("expected openai"));
}

#[tokio::test]
async fn reserved_options_are_rejected_before_openai_network_io() {
    let provider = ilar::provider::openai::OpenAIProvider::new(
        "k".into(),
        Some("http://127.0.0.1:1/v1".into()),
    );
    for key in ["model", "input", "tools", "stream"] {
        let mut request = request_with_tool();
        request.options = serde_json::Value::Object(
            [(key.to_string(), serde_json::json!("override"))]
                .into_iter()
                .collect(),
        );
        let error = provider.stream(request).err().expect("reserved option");
        assert!(error.to_string().contains(key), "{error:#}");
    }
    let mut request = request_with_tool();
    request.options = serde_json::json!(["not", "an", "object"]);
    assert!(provider.stream(request).is_err());
}

#[tokio::test]
async fn neutral_request_serializes_to_wire_format() {
    let (base, server) = http_server(fixture("openai_text.sse"));
    let mut request = request_with_tool();
    request.system_prompt = Some("be terse".into());
    request.messages = vec![
        ilar::session::ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "read it".into(),
            }],
        },
        ilar::session::ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "Cargo.toml"}),
            }],
        },
        ilar::session::ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "1: model".into(),
                is_error: false,
            }],
        },
    ];
    // Hold the stream: dropping it aborts the pump task (cancellation
    // contract), which would kill the request before the server sees it.
    let stream = provider(base).stream(request).unwrap();
    let req = server.await.unwrap();
    drop(stream);
    assert!(req.starts_with("POST /v1/responses"));
    assert!(req.contains("Bearer test-key"));
    let body_start = req.find("\r\n\r\n").unwrap() + 4;
    let body: serde_json::Value = serde_json::from_str(req[body_start..].trim()).unwrap();
    assert_eq!(body["model"], "gpt-5.2");
    assert_eq!(body["stream"], true);
    assert_eq!(body["instructions"], "be terse");
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["tools"][0]["type"], "function");
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "call_1");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], "call_1");
    assert!(
        input[2].get("is_error").is_none(),
        "Responses API function_call_output rejects is_error"
    );
}

#[tokio::test]
async fn stateless_tool_continuation_replays_opaque_reasoning_in_order() {
    let first_sse = concat!(
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"encrypted-1\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
    )
    .as_bytes()
    .to_vec();
    let (first_base, first_server) = http_server(first_sse);
    let mut first_request = request_with_tool();
    first_request.options = serde_json::json!({"store": false});
    let events = drain(provider(first_base).stream(first_request).unwrap()).await;
    first_server.await.unwrap();

    let reasoning = events
        .iter()
        .find_map(|event| match event {
            ProviderEvent::ReasoningItem { item } => Some(item.clone()),
            _ => None,
        })
        .expect("reasoning item");
    let call = events
        .iter()
        .find_map(|event| match event {
            ProviderEvent::ToolCallCompleted { id, name, input } => Some(ContentBlock::ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        })
        .expect("tool call");

    let (second_base, second_server) = http_server(fixture("openai_text.sse"));
    let mut second_request = request_with_tool();
    second_request.options = serde_json::json!({
        "store": false,
        "include": ["web_search_call.action.sources"]
    });
    second_request.messages = vec![
        ilar::session::ChatMessage::user_text("read it"),
        ilar::session::ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Reasoning { item: reasoning }, call],
        },
        ilar::session::ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "contents".into(),
                is_error: false,
            }],
        },
    ];
    let stream = provider(second_base).stream(second_request).unwrap();
    let wire = second_server.await.unwrap();
    drop(stream);
    let body_start = wire.find("\r\n\r\n").unwrap() + 4;
    let body: serde_json::Value = serde_json::from_str(wire[body_start..].trim()).unwrap();

    assert_eq!(body["store"], false);
    assert_eq!(body["include"][0], "web_search_call.action.sources");
    assert_eq!(body["include"][1], "reasoning.encrypted_content");
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["encrypted_content"], "encrypted-1");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[3]["type"], "function_call_output");
}
