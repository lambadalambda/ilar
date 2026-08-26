//! `[models.<name>]` endpoints: the shared chat-completions wire with
//! everything z.ai-specific left out.

use futures::StreamExt;
use ilar::provider::chat::{ChatDialect, ChatProvider};
use ilar::provider::{Provider, ProviderEvent, Request, StopReason, ToolDefinition, chat};
use ilar::session::{ChatMessage, ContentBlock, ImageContent, Role, Usage};

fn request() -> Request {
    Request {
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        ..Request::with_model("custom/qwen")
    }
}

/// One request, captured verbatim, answered with `sse_body`.
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
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
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
        // In small pieces: SSE frames arriving split across reads is the
        // normal case on the wire, not an edge one.
        for chunk in sse_body.chunks(37) {
            socket.write_all(chunk).await.unwrap();
            socket.flush().await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        String::from_utf8_lossy(&req).into_owned()
    });
    (format!("http://{addr}"), handle)
}

/// A keyless local server: no api_key, no vision, the wire id it wants.
fn keyless(base_url: String) -> ChatProvider {
    ChatProvider::new(ChatDialect::custom(
        base_url,
        "llama3.3:70b".into(),
        None,
        false,
    ))
}

async fn drain(mut stream: ilar::provider::EventStream) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

/// The three things a custom endpoint changes about the request: where it
/// goes, what it calls the model, and that a keyless server is not sent
/// an Authorization header. Plus the one thing it must not carry: z.ai's
/// `tool_stream`.
#[tokio::test]
async fn a_keyless_endpoint_gets_the_configured_url_and_wire_id_without_authorization() {
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (base, server) = http_server(sse.as_bytes().to_vec());
    let provider = keyless(base);

    let stream = provider.stream(request()).unwrap();
    let wire = server.await.unwrap();
    drop(stream);

    assert!(wire.starts_with("POST /chat/completions"), "{wire}");
    assert!(
        !wire.to_lowercase().contains("authorization"),
        "a keyless server must not be sent credentials: {wire}"
    );
    let body_start = wire.find("\r\n\r\n").unwrap() + 4;
    let body: serde_json::Value = serde_json::from_str(wire[body_start..].trim()).unwrap();
    // The wire id is the entry's `model`, not the ilar id.
    assert_eq!(body["model"], "llama3.3:70b");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body.get("tool_stream").is_none(), "{body}");
    let mut keys = body
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        ["messages", "model", "stream", "stream_options", "tools"]
    );
}

#[tokio::test]
async fn a_configured_key_is_sent_as_a_bearer_token() {
    let sse = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (base, server) = http_server(sse.as_bytes().to_vec());
    let provider = ChatProvider::new(ChatDialect::custom(
        base,
        "qwen3".into(),
        Some("local-key".into()),
        false,
    ));

    let stream = provider.stream(request()).unwrap();
    let wire = server.await.unwrap();
    drop(stream);

    assert!(wire.contains("Bearer local-key"), "{wire}");
}

/// Configured sampling options ride into the body as typed, through the
/// same merge the wire already had.
#[tokio::test]
async fn configured_options_reach_the_body_verbatim() {
    let sse = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (base, server) = http_server(sse.as_bytes().to_vec());
    let provider = ChatProvider::new(
        ChatDialect::custom(base, "qwen3".into(), None, false).with_options(serde_json::json!({
            "temperature": 0.7,
            "top_p": 0.9,
            "min_p": 0.05,
        })),
    );

    let stream = provider.stream(request()).unwrap();
    let wire = server.await.unwrap();
    drop(stream);

    let body_start = wire.find("\r\n\r\n").unwrap() + 4;
    let body: serde_json::Value = serde_json::from_str(wire[body_start..].trim()).unwrap();
    assert_eq!(body["temperature"], 0.7);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["min_p"], 0.05);
}

/// Text, then a tool call, then a finish reason — and not one usage frame
/// in the whole stream, which many local servers never send. The turn
/// still completes; only its accounting is zero.
#[tokio::test]
async fn a_round_trip_without_usage_frames_yields_text_a_tool_call_and_a_zero_usage_turn() {
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"reading\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"x\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _server) = http_server(sse.as_bytes().to_vec());
    let events = drain(keyless(base).stream(request()).unwrap()).await;

    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("reading".into()),
            ProviderEvent::ToolCallStarted {
                id: "call_1".into(),
                name: "read".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallInputDelta {
                id: "call_1".into(),
                delta: "{\"path\":\"x\"}".into(),
            },
            ProviderEvent::ToolCallCompleted {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "x"}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ]
    );
}

#[tokio::test]
async fn rejects_a_model_for_another_provider() {
    let provider = keyless("http://127.0.0.1:1".into());
    let error = provider
        .stream(Request::with_model("zai/glm-4.7"))
        .err()
        .expect("provider mismatch must fail preflight");
    assert!(error.to_string().contains("expected custom"), "{error}");
}

#[tokio::test]
async fn reserved_options_are_rejected_before_network_io_and_tool_stream_is_not_one() {
    let provider = keyless("http://127.0.0.1:1".into());
    for key in chat::RESERVED_OPTIONS {
        let mut request = request();
        request.options = serde_json::Value::Object(
            [((*key).to_string(), serde_json::json!("override"))]
                .into_iter()
                .collect(),
        );
        let error = provider.stream(request).err().expect("reserved option");
        assert!(error.to_string().contains(key), "{error:#}");
    }
    // z.ai's body field is not part of this dialect, so it is an option
    // like any other here.
    let mut request = request();
    request.options = serde_json::json!({"tool_stream": true});
    let body = provider.wire_body_for_test(&request);
    assert_eq!(body["tool_stream"], true);
}

/// The entry's own flag decides images: there is no catalog row to ask.
#[test]
fn the_configured_vision_flag_decides_image_parts() {
    let seeing = ChatProvider::new(ChatDialect::custom(
        "http://127.0.0.1:1".into(),
        "qwen3-vl".into(),
        None,
        true,
    ));
    let blind = keyless("http://127.0.0.1:1".into());
    let mut req = request();
    req.messages = vec![ChatMessage {
        role: Role::User,
        content: vec![
            ContentBlock::Text {
                text: "what is this?".into(),
            },
            ContentBlock::Image {
                image: ImageContent {
                    media_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                },
            },
        ],
    }];

    assert_eq!(
        seeing.wire_body_for_test(&req)["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aGVsbG8="
    );
    assert_eq!(
        blind.wire_body_for_test(&req)["messages"][0]["content"],
        "what is this?\n[image omitted: this model cannot view images]"
    );
}
