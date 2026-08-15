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
