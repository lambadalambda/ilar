use futures::StreamExt;
use ilar::provider::zai::ZaiProvider;
use ilar::provider::{Provider, ProviderEvent, Request, StopReason, ToolDefinition};
use ilar::session::{ChatMessage, ContentBlock, ImageContent, InputTokenAccounting, Role, Usage};

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

fn provider(base_url: String) -> ZaiProvider {
    ZaiProvider::new("k".into(), Some(base_url))
}

async fn drain(mut stream: ilar::provider::EventStream) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
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
    let provider = ZaiProvider::new("k".into(), Some(base));
    let events = drain(provider.stream(request()).unwrap()).await;

    assert_eq!(events[0], ProviderEvent::ThinkingDelta("think-1".into()));
    assert_eq!(events[1], ProviderEvent::ThinkingCompleted);
    assert_eq!(events[2], ProviderEvent::TextDelta("text-1".into()));
    assert_eq!(events[3], ProviderEvent::ThinkingDelta("think-2".into()));
    assert_eq!(events[4], ProviderEvent::ThinkingCompleted);
    assert!(matches!(events[5], ProviderEvent::ToolCallStarted { .. }));
}

/// chat-completions reports mid-stream failures as an error chunk rather
/// than terminating the HTTP response.
#[tokio::test]
async fn mid_stream_error_chunk_maps_to_error_event() {
    let body = "data: {\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    let (base, _server) = http_server(body.as_bytes().to_vec());
    let events = drain(provider(base).stream(request()).unwrap()).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(&events[0], ProviderEvent::RetryableError(m) if m.contains("Overloaded")));
}

#[tokio::test]
async fn http_error_body_is_bounded_and_redacted() {
    let body = format!("api_key=super-secret {}", "padding".repeat(20_000));
    let base = http_error_server(body);
    let provider = ZaiProvider::new("super-secret".into(), Some(base));

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
async fn malformed_openai_compatible_arguments_are_terminal() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{bad\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{}}\n\n",
    );
    let base = http_server(body.as_bytes().to_vec()).0;
    let provider = ZaiProvider::new("k".into(), Some(base));

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
async fn openai_compatible_arguments_without_a_name_are_terminal() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{}}\n\n",
    );
    let provider = ZaiProvider::new("k".into(), Some(http_server(body.as_bytes().to_vec()).0));
    let events = drain(provider.stream(request()).unwrap()).await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("index 0")),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::TurnComplete { .. }))
    );
}

/// A live GLM-4.6V stream opened index 0 with an arguments-only chunk —
/// no id, no name — and named the call one chunk later. The turn has to
/// survive that ordering with its events intact.
#[tokio::test]
async fn openai_compatible_tolerates_arguments_before_the_tool_name() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"type\":\"function\",\"function\":{\"arguments\":\"{\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\\\"path\\\":\\\"x\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{}}\n\n",
    );
    let provider = ZaiProvider::new("k".into(), Some(http_server(body.as_bytes().to_vec()).0));
    let events = drain(provider.stream(request()).unwrap()).await;

    assert_eq!(
        events,
        vec![
            ProviderEvent::ToolCallStarted {
                id: "call_1".into(),
                name: "read".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallInputDelta {
                id: "call_1".into(),
                delta: "{".into(),
            },
            ProviderEvent::ToolCallInputDelta {
                id: "call_1".into(),
                delta: "\"path\":\"x\"}".into(),
            },
            ProviderEvent::ToolCallCompleted {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "x"}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_token_accounting: Some(InputTokenAccounting::ExcludesCached),
                    ..Usage::default()
                },
            },
        ]
    );
}

/// The captured chunk verbatim: a complete `{}` argument object arrives
/// before the call has any identity at all.
#[tokio::test]
async fn openai_compatible_replays_arguments_captured_before_the_call_started() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"type\":\"function\",\"function\":{\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{}}\n\n",
    );
    let provider = ZaiProvider::new("k".into(), Some(http_server(body.as_bytes().to_vec()).0));
    let events = drain(provider.stream(request()).unwrap()).await;

    assert_eq!(
        events[..3],
        [
            ProviderEvent::ToolCallStarted {
                id: "call_1".into(),
                name: "read".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallInputDelta {
                id: "call_1".into(),
                delta: "{}".into(),
            },
            ProviderEvent::ToolCallCompleted {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
        ]
    );
    assert_eq!(
        events.last().and_then(ProviderEvent::stop_reason),
        Some(StopReason::ToolUse)
    );
}

/// A refusal and a tool call cannot both be what the turn did. The rule
/// used to live only in the OpenAI Responses mapper; it is now part of the
/// shared mapper core, so z.ai enforces it too.
#[tokio::test]
async fn a_refusal_combined_with_tool_calls_is_terminal() {
    let openai = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}],\"usage\":{}}\n\n",
    );
    let provider = ZaiProvider::new("k".into(), Some(http_server(openai.as_bytes().to_vec()).0));
    let events = drain(provider.stream(request()).unwrap()).await;
    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error))
            if error.contains("combined refusal and tool calls")),
        "{events:?}"
    );
    assert_no_dangling_completion(&events);
}

/// A rejected turn must not leak the null-input completions the mapper
/// builds before it validates the stop reason.
fn assert_no_dangling_completion(events: &[ProviderEvent]) {
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::TurnComplete { .. })),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            ProviderEvent::ToolCallCompleted { input, .. } if input.is_null()
        )),
        "{events:?}"
    );
}

/// The rule is about the *combination*: a refusal on its own is a normal,
/// completed turn.
#[tokio::test]
async fn a_refusal_without_tool_calls_completes_the_turn() {
    let openai = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}],\"usage\":{}}\n\n";
    let provider = ZaiProvider::new("k".into(), Some(http_server(openai.as_bytes().to_vec()).0));
    let events = drain(provider.stream(request()).unwrap()).await;
    assert_eq!(
        events.last().and_then(ProviderEvent::stop_reason),
        Some(StopReason::Refusal),
        "{events:?}"
    );
}

/// Truncated calls are completed in *wire* order, which chat-completions
/// states as the tool-call index — not the order the chunks announced them
/// in. A server that interleaves two calls announces whichever emits a
/// name first.
#[tokio::test]
async fn truncated_openai_compatible_calls_complete_in_tool_index_order() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"edit\",\"arguments\":\"{\\\"path\\\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],\"usage\":{}}\n\n",
    );
    let provider = ZaiProvider::new("k".into(), Some(http_server(body.as_bytes().to_vec()).0));
    let events = drain(provider.stream(request()).unwrap()).await;

    // Announced 1 then 0 …
    let started = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallStarted { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(started, vec!["call_b", "call_a"], "{events:?}");
    // … completed 0 then 1, both null-input per the truncation contract.
    let completed = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallCompleted { id, input, .. } => {
                Some((id.as_str(), input.is_null()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed, vec![("call_a", true), ("call_b", true)]);
    assert_eq!(
        events.last().and_then(ProviderEvent::stop_reason),
        Some(StopReason::MaxTokens)
    );
}

/// Cache *writes* were only ever read by the OpenAI mapper. With one
/// usage normalizer they are read here too: without them a z.ai request
/// that wrote a prefix looked like it wrote nothing, and the written
/// tokens stayed folded into the plain input count.
#[tokio::test]
async fn usage_reports_cache_writes() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2600,\"completion_tokens\":50,\"prompt_tokens_details\":{\"cached_tokens\":2000,\"cache_write_tokens\":400}}}\n\n",
    );
    let provider = ZaiProvider::new("k".into(), Some(http_server(body.as_bytes().to_vec()).0));
    let events = drain(provider.stream(request()).unwrap()).await;

    let Some(ProviderEvent::TurnComplete { usage, .. }) = events.last() else {
        panic!("expected TurnComplete, got {events:?}");
    };
    assert_eq!(usage.cache_read_input_tokens, 2_000);
    assert_eq!(usage.cache_creation_input_tokens, 400);
    // Reads and writes are both carved out of the reported prompt total.
    assert_eq!(usage.input_tokens, 200);
    assert_eq!(usage.output_tokens, 50);
    assert_eq!(usage.context_tokens(), 2_650);
}

#[tokio::test]
async fn openai_compatible_done_completes_a_finished_stream_without_usage() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let provider = ZaiProvider::new("k".into(), Some(http_server(body.as_bytes().to_vec()).0));
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
    let provider = ZaiProvider::new("k".into(), Some(http_server(body.as_bytes().to_vec()).0));
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
    let provider = ZaiProvider::new("k".into(), Some(base));

    let events = drain(provider.stream(request()).unwrap()).await;

    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("duplicate OpenAI-compatible tool id"))
    );
}

#[tokio::test]
async fn uses_chat_completions_endpoint() {
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

    let provider = ZaiProvider::new("test-key".into(), Some(format!("http://{addr}")));
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
fn serializes_system_and_tool_results_in_protocol_order() {
    let provider = ZaiProvider::new("k".into(), None);
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
                    item_id: None,
                },
                ContentBlock::ToolCall {
                    id: "call_2".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "two"}),
                    item_id: None,
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
                    images: Vec::new(),
                },
                ContentBlock::Text {
                    text: "continue with this context".into(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call_2".into(),
                    content: "two result".into(),
                    is_error: false,
                    images: Vec::new(),
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
    // Tools are the chat-completions `function` wrapper, and the body
    // carries exactly the chat-completions keys — no `system` block, no
    // `max_tokens`, nothing from another dialect.
    assert_eq!(body["model"], "glm-4.7");
    assert_eq!(body["stream"], true);
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    let mut keys = body
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        [
            "messages",
            "model",
            "stream",
            "stream_options",
            "tool_stream",
            "tools"
        ]
    );
}

/// One tool result carrying one image, sent to `model`.
fn tool_result_with_image(model: &str) -> serde_json::Value {
    let provider = ZaiProvider::new("k".into(), None);
    let mut req = Request {
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "screenshot.png (64x64)".into(),
                is_error: false,
                images: vec![ImageContent {
                    media_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                }],
            }],
        }],
        ..request()
    };
    req.model = model.into();
    provider.wire_body_for_test(&req)["messages"][0].clone()
}

/// A vision model gets the real image as a part of the tool message the
/// result text rides in — the same shape a user image uses.
#[test]
fn a_vision_tool_result_carries_text_and_image_url_parts() {
    assert_eq!(
        tool_result_with_image("zai/glm-4.6v"),
        serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "content": [
                {"type": "text", "text": "screenshot.png (64x64)"},
                {
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,aGVsbG8="},
                },
            ],
        })
    );
}

/// Gating is per request, so a mid-session switch to a text-only model
/// degrades to the same named gap a user image gets there.
#[test]
fn a_non_vision_model_sees_a_named_gap_in_a_tool_result() {
    assert_eq!(
        tool_result_with_image("zai/glm-4.7"),
        serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "content": "screenshot.png (64x64)\n[image omitted: this model cannot view images]",
        })
    );
}

/// Text-only results keep the plain string they always had, on a vision
/// model too: cached prefixes must not move.
#[test]
fn a_text_only_tool_result_content_stays_a_plain_string() {
    let provider = ZaiProvider::new("k".into(), None);
    let mut req = Request {
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "1: model".into(),
                is_error: false,
                images: Vec::new(),
            }],
        }],
        ..request()
    };
    req.model = "zai/glm-4.6v".into();

    assert_eq!(
        provider.wire_body_for_test(&req)["messages"][0],
        serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "content": "1: model",
        })
    );
}

#[tokio::test]
async fn invalid_model_id_is_preflight_error() {
    let provider = ZaiProvider::new("k".into(), None);
    assert!(provider.stream(Request::with_model("glm-4.7")).is_err());
}

#[tokio::test]
async fn rejects_model_for_another_provider() {
    let provider = ZaiProvider::new("k".into(), None);
    let error = provider
        .stream(Request::with_model("openai/gpt-5.2"))
        .err()
        .expect("provider mismatch must fail preflight");
    assert!(error.to_string().contains("expected zai"));
}

#[tokio::test]
async fn reserved_options_are_rejected_before_zai_network_io() {
    let provider = ZaiProvider::new("k".into(), Some("http://127.0.0.1:1".into()));
    for key in [
        "model",
        "messages",
        "tools",
        "stream",
        "stream_options",
        "tool_stream",
    ] {
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

/// Prompt caching on the OpenAI-compatible route is prefix matching on
/// the serialized request: z.ai bills a cache hit only for the leading
/// messages that are byte-identical to the previous call. So a turn that
/// only appends must leave every earlier message — and the system
/// prompt, and the tool declarations — exactly where and as they were.
///
/// This is the coverage the removed Anthropic flavor's cache-breakpoint
/// tests used to provide; nothing else watches the wire body for it.
#[test]
fn appending_a_turn_leaves_the_cached_prefix_byte_identical() {
    let provider = ZaiProvider::new("k".into(), None);
    let mut req = request();
    req.system_prompt = Some("follow instructions".into());
    req.messages = vec![
        ChatMessage::user_text("read both"),
        ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "one"}),
                item_id: None,
            }],
        },
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "one result".into(),
                is_error: false,
                images: Vec::new(),
            }],
        },
    ];

    let first = provider.wire_body_for_test(&req);
    let prefix_len = first["messages"].as_array().unwrap().len();

    // The next round of the same conversation: one more exchange on the
    // end, nothing else touched.
    req.messages.push(ChatMessage {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: "the file says one".into(),
        }],
    });
    req.messages.push(ChatMessage::user_text("now the other"));
    let second = provider.wire_body_for_test(&req);

    // Everything a cache prefix covers is unchanged, in place and as
    // text: comparing serialized forms catches key reordering too.
    assert_eq!(
        serde_json::to_string(&first["tools"]).unwrap(),
        serde_json::to_string(&second["tools"]).unwrap(),
        "tool declarations sit in the cached prefix"
    );
    assert_eq!(first["model"], second["model"]);
    let before = first["messages"].as_array().unwrap();
    let after = second["messages"].as_array().unwrap();
    assert!(after.len() > prefix_len);
    for index in 0..prefix_len {
        assert_eq!(
            serde_json::to_string(&before[index]).unwrap(),
            serde_json::to_string(&after[index]).unwrap(),
            "message {index} moved or changed and would miss the cache"
        );
    }
    // The system prompt is the very front of the prefix and stays there.
    assert_eq!(after[0]["role"], "system");
    assert_eq!(after[0]["content"], "follow instructions");

    // The same request built twice is byte-identical: nothing in the
    // body is clock-, order- or identity-dependent.
    assert_eq!(
        serde_json::to_string(&provider.wire_body_for_test(&req)).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
}
