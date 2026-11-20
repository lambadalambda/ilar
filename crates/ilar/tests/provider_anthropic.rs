//! Local contract tests for direct Anthropic Messages SSE.

use futures::StreamExt;
use ilar::provider::anthropic::AnthropicProvider;
use ilar::provider::{Provider, ProviderEvent, Request, StopReason, ToolDefinition};
use ilar::session::{ChatMessage, ContentBlock, InputTokenAccounting, Role};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MODEL: &str = "anthropic/claude-haiku-5-5";

async fn server(
    status: &str,
    response: String,
) -> (String, tokio::task::JoinHandle<(String, Value)>) {
    let (base, task) = server_steps(vec![(status, response)]).await;
    (
        base,
        tokio::spawn(async move { task.await.unwrap().remove(0) }),
    )
}

async fn server_steps(
    steps: Vec<(&str, String)>,
) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let steps: Vec<_> = steps
        .into_iter()
        .map(|(status, body)| (status.to_owned(), body))
        .collect();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, response) in steps {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (headers, offset) = loop {
                assert_ne!(socket.read_buf(&mut bytes).await.unwrap(), 0);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break (String::from_utf8(bytes[..end].to_vec()).unwrap(), end + 4);
                }
            };
            let len = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() < offset + len {
                assert_ne!(socket.read_buf(&mut bytes).await.unwrap(), 0);
            }
            let body = serde_json::from_slice(&bytes[offset..offset + len]).unwrap();
            socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).as_bytes()).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            requests.push((headers.to_ascii_lowercase(), body));
        }
        requests
    });
    (format!("http://{addr}/v1/"), task)
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

fn start() -> Value {
    json!({"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":1,"cache_creation_input_tokens":600,"cache_read_input_tokens":1000}}})
}

fn signed(signature: &str) -> ContentBlock {
    ContentBlock::Reasoning {
        item: json!({"type":"anthropic_thinking","block":{"type":"thinking","thinking":"","signature":signature}}),
    }
}

fn edited_history() -> Request {
    let assistant = |content| ChatMessage {
        role: Role::Assistant,
        content,
    };
    Request {
        system_prompt: Some("Edited system".into()),
        messages: vec![
            ChatMessage::user_text("Hello"),
            assistant(vec![signed("valid"), ContentBlock::text("Earlier reply")]),
            ChatMessage::user_text("[image omitted]"),
            assistant(vec![
                signed("stale"),
                ContentBlock::text("Checking"),
                signed("later"),
                ContentBlock::ToolCall {
                    id: "toolu_1".into(),
                    name: "read".into(),
                    input: json!({}),
                    item_id: None,
                },
            ]),
            ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "found".into(),
                    is_error: false,
                    images: vec![],
                }],
            },
        ],
        ..Request::with_model(MODEL)
    }
}

const BINDING_ERROR: &str = "messages.3.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation.";

#[tokio::test]
async fn prefix_rejection_recovers_once_without_losing_valid_history_or_tools() {
    let error =
        json!({"type":"error","error":{"type":"invalid_request_error","message":BINDING_ERROR}})
            .to_string();
    let (base, server) = server_steps(vec![
        ("400 Bad Request", error),
        (
            "200 OK",
            sse(&[
                start(),
                stop("end_turn")[0].clone(),
                stop("end_turn")[1].clone(),
            ]),
        ),
    ])
    .await;
    let events: Vec<_> = AnthropicProvider::new("test-key".into(), Some(base))
        .stream(edited_history())
        .unwrap()
        .collect()
        .await;
    assert!(
        matches!(&events[..], [ProviderEvent::TurnComplete { .. }]),
        "{events:?}"
    );
    let requests = server.await.unwrap();
    let (first, second) = (&requests[0].1, &requests[1].1);
    assert_eq!(first["messages"][3]["content"][0]["signature"], "stale");
    assert_eq!(
        second["messages"].as_array().unwrap()[..2],
        first["messages"].as_array().unwrap()[..2]
    );
    assert_eq!(
        second["messages"][3]["content"],
        json!([
            first["messages"][3]["content"][1],
            first["messages"][3]["content"][3]
        ])
    );
    assert_eq!(second["messages"][4], first["messages"][4]);
    assert_eq!(second["system"], first["system"]);
}

#[tokio::test]
async fn configured_followup_remembers_removed_signatures_only_for_its_session() {
    let end = || {
        sse(&[
            start(),
            stop("end_turn")[0].clone(),
            stop("end_turn")[1].clone(),
        ])
    };
    let error =
        json!({"type":"error","error":{"type":"invalid_request_error","message":BINDING_ERROR}})
            .to_string();
    let (base, server) = server_steps(vec![
        ("400 Bad Request", error),
        ("200 OK", end()),
        ("200 OK", end()),
        ("200 OK", end()),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ilar.toml"), format!("[general]\nmodel = '{MODEL}'\n[providers.anthropic]\nbase_url = '{base}'\napi_key = 'test-key'\n")).unwrap();
    let config = ilar::config::Loader::no_env()
        .config_dir(dir.path().to_path_buf())
        .resolve()
        .unwrap();
    let send = |request| {
        config
            .provider_result(MODEL)
            .unwrap()
            .stream(request)
            .unwrap()
            .collect::<Vec<_>>()
    };
    let mut req = edited_history();
    req.cache_key = Some("one-session".into());
    assert!(matches!(
        send(req.clone()).await.last(),
        Some(ProviderEvent::TurnComplete { .. })
    ));
    req.messages.extend([
        ChatMessage {
            role: Role::Assistant,
            content: vec![signed("fresh"), ContentBlock::text("Next reply")],
        },
        ChatMessage::user_text("Next turn"),
    ]);
    let _ = send(req.clone()).await;
    req.cache_key = Some("other-session".into());
    let _ = send(req).await;
    let requests = server.await.unwrap();
    let followup = &requests[2].1["messages"];
    assert_eq!(
        followup.as_array().unwrap()[..5],
        requests[1].1["messages"].as_array().unwrap()[..]
    );
    assert_eq!(followup[5]["content"][0]["signature"], "fresh");
    assert_eq!(
        requests[3].1["messages"][3]["content"][0]["signature"],
        "stale"
    );
}

#[tokio::test]
async fn recovery_does_not_retry_a_second_rejection() {
    let error = |message| {
        json!({"type":"error","error":{"type":"invalid_request_error","message":message}})
            .to_string()
    };
    let (base, server) = server_steps(vec![
        ("400 Bad Request", error(BINDING_ERROR)),
        ("400 Bad Request", error("messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation.")),
    ]).await;
    let events: Vec<_> = AnthropicProvider::new("test-key".into(), Some(base))
        .stream(edited_history())
        .unwrap()
        .collect()
        .await;
    assert!(
        matches!(&events[..], [ProviderEvent::Error(error)] if error.contains("messages.1.content.0")),
        "{events:?}"
    );
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn signature_error_in_sse_after_text_is_not_retried() {
    let (base, _) = server("200 OK", sse(&[
        start(), json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial"}}),
        json!({"type":"error","error":{"type":"invalid_request_error","message":BINDING_ERROR}}),
    ])).await;
    let events: Vec<_> = AnthropicProvider::new("test-key".into(), Some(base))
        .stream(edited_history())
        .unwrap()
        .collect()
        .await;
    assert!(
        matches!(&events[..], [ProviderEvent::TextDelta(text), ProviderEvent::Error(_)] if text == "partial"),
        "{events:?}"
    );
}

#[tokio::test]
async fn assistant_tail_is_kept_as_history_not_as_prefill() {
    let mut req = edited_history();
    req.messages.truncate(2);
    let (base, server) = server(
        "200 OK",
        sse(&[
            start(),
            stop("end_turn")[0].clone(),
            stop("end_turn")[1].clone(),
        ]),
    )
    .await;
    let _ = AnthropicProvider::new("test-key".into(), Some(base))
        .stream(req)
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    let (_, body) = server.await.unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["content"][0]["signature"], "valid");
    assert_eq!(messages[1]["content"][1]["text"], "Earlier reply");
    assert_eq!(messages[2]["role"], "user");
    assert!(
        !messages[2]["content"][0]["text"]
            .as_str()
            .unwrap()
            .trim()
            .is_empty()
    );
}

#[tokio::test]
async fn resume_continuation_stays_in_the_prefix_of_a_tool_followup() {
    let end = || {
        sse(&[
            start(),
            stop("end_turn")[0].clone(),
            stop("end_turn")[1].clone(),
        ])
    };
    let (base, server) = server_steps(vec![("200 OK", end()), ("200 OK", end())]).await;
    let provider = AnthropicProvider::new("test-key".into(), Some(base));
    let mut req = edited_history();
    req.messages.truncate(2);
    let _ = provider
        .stream(req.clone())
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    req.messages.extend([
        ChatMessage {
            role: Role::Assistant,
            content: vec![
                signed("new-thought"),
                ContentBlock::ToolCall {
                    id: "toolu_2".into(),
                    name: "read".into(),
                    input: json!({}),
                    item_id: None,
                },
            ],
        },
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_2".into(),
                content: "found".into(),
                is_error: false,
                images: vec![],
            }],
        },
    ]);
    let _ = provider.stream(req).unwrap().collect::<Vec<_>>().await;
    let requests = server.await.unwrap();
    let first = requests[0].1["messages"].as_array().unwrap();
    let followup = requests[1].1["messages"].as_array().unwrap();
    assert_eq!(&followup[..first.len()], first);
    assert_eq!(
        followup[first.len()]["content"][0]["signature"],
        "new-thought"
    );
    assert_eq!(
        followup[first.len() + 1]["content"][0]["tool_use_id"],
        "toolu_2"
    );
}

#[tokio::test]
async fn rejects_remote_cleartext_before_sending_credentials() {
    let result = AnthropicProvider::new("test-key".into(), Some("http://example.test/v1".into()))
        .stream(edited_history());
    assert!(
        result
            .err()
            .is_some_and(|err| err.to_string().contains("HTTPS"))
    );
}

fn stop(reason: &str) -> [Value; 2] {
    [
        json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":19,"input_tokens":8}}),
        json!({"type":"message_stop"}),
    ]
}

#[tokio::test]
async fn native_text_tools_signed_thinking_and_usage_round_trip() {
    let (base, first) = server("200 OK", sse(&[
        start(),
        json!({"type":"ping"}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Check the file."}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"nature"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":"I "}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"will check."}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"\"file\"}"}}),
        json!({"type":"content_block_stop","index":3}),
        stop("tool_use")[0].clone(), stop("tool_use")[1].clone(),
    ])).await;
    let request = Request {
        system_prompt: Some("Stable system prefix".into()),
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            input_schema: json!({"type":"object"}),
        }],
        messages: vec![ChatMessage::user_text("Hi")],
        options: json!({"max_tokens": 4096, "thinking": {"type":"adaptive","display":"summarized"}, "output_config":{"effort":"low"}}),
        ..Request::with_model(MODEL)
    };
    let events: Vec<_> = AnthropicProvider::new("test-key".into(), Some(base))
        .stream(request.clone())
        .unwrap()
        .collect()
        .await;
    let (headers, body) = first.await.unwrap();
    assert!(headers.starts_with("post /v1/messages "), "{headers}");
    assert!(headers.contains("\r\nx-api-key: test-key"));
    assert!(headers.contains("\r\nanthropic-version: 2026-10-21"));
    assert!(!headers.contains("authorization:"));
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["thinking"], request.options["thinking"]);
    assert_eq!(body["output_config"], request.options["output_config"]);
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    assert_eq!(body["tools"][0]["input_schema"], json!({"type":"object"}));
    assert_eq!(body["cache_control"], json!({"type":"ephemeral"}));
    assert_eq!(ProviderEvent::text_of(&events), "I will check.");
    assert!(events.contains(&ProviderEvent::ReasoningSummaryDelta(
        "Check the file.".into()
    )));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ProviderEvent::ToolCallInputDelta { .. }))
            .count(),
        2
    );
    let Some(ProviderEvent::TurnComplete {
        stop_reason: StopReason::ToolUse,
        usage,
    }) = events.last()
    else {
        panic!("{events:?}")
    };
    assert_eq!(
        usage.input_token_accounting,
        Some(InputTokenAccounting::ExcludesCached)
    );
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
            usage.output_tokens
        ),
        (8, 600, 1000, 19)
    );
    assert_eq!(usage.context_tokens(), 1627);
    let price = ilar::model::pricing_for(MODEL).unwrap();
    assert!(
        (price.cost(usage) - (8.0 * 0.1 + 600.0 * 0.125 + 1000.0 * 0.01 + 19.0 * 0.5) / 1e6).abs()
            < 1e-12
    );

    let mut followup = request;
    let mut content: Vec<_> = events
        .into_iter()
        .filter_map(|event| match event {
            ProviderEvent::ReasoningItem { item } => Some(ContentBlock::Reasoning { item }),
            ProviderEvent::ToolCallCompleted { id, name, input } => Some(ContentBlock::ToolCall {
                id,
                name,
                input,
                item_id: None,
            }),
            _ => None,
        })
        .collect();
    // A model switch can leave opaque state from another provider in the log.
    content.insert(
        0,
        ContentBlock::Reasoning {
            item: json!({"type":"reasoning","encrypted_content":"foreign"}),
        },
    );
    followup.messages.push(ChatMessage {
        role: Role::Assistant,
        content,
    });
    followup
        .messages
        .push(ChatMessage::user_text("Extra context"));
    followup.messages.push(ChatMessage {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "found".into(),
            is_error: false,
            images: vec![],
        }],
    });
    let (base, second) = server(
        "200 OK",
        sse(&[
            start(),
            stop("end_turn")[0].clone(),
            stop("end_turn")[1].clone(),
        ]),
    )
    .await;
    let _ = AnthropicProvider::new("test-key".into(), Some(base))
        .stream(followup)
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    let (_, body) = second.await.unwrap();
    assert_eq!(
        body["messages"][1]["content"],
        json!([
            {"type":"thinking","thinking":"Check the file.","signature":"signature"},
            {"type":"redacted_thinking","data":"opaque"},
            {"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"file"}}
        ])
    );
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(body["messages"][2]["content"][1]["text"], "Extra context");
}

#[tokio::test]
async fn eof_without_message_stop_is_retryable() {
    let (base, _) = server("200 OK", sse(&[start(), stop("end_turn")[0].clone()])).await;
    let events: Vec<_> = AnthropicProvider::new("test-key".into(), Some(base))
        .stream(Request {
            messages: vec![ChatMessage::user_text("Hi")],
            ..Request::with_model(MODEL)
        })
        .unwrap()
        .collect()
        .await;
    assert!(
        matches!(&events[..], [ProviderEvent::RetryableError(_)]),
        "{events:?}"
    );
}

#[tokio::test]
async fn auth_error_names_config_but_does_not_echo_key() {
    let (base, _) = server("401 Unauthorized", "test-key refused".into()).await;
    let events: Vec<_> = AnthropicProvider::new("test-key".into(), Some(base))
        .stream(Request {
            messages: vec![ChatMessage::user_text("Hi")],
            ..Request::with_model(MODEL)
        })
        .unwrap()
        .collect()
        .await;
    let [ProviderEvent::Error(error)] = &events[..] else {
        panic!("{events:?}")
    };
    assert!(error.contains("ANTHROPIC_API_KEY"), "{error}");
    assert!(!error.contains("test-key"), "{error}");
}

#[tokio::test]
async fn signature_without_a_summary_survives_and_truncated_tool_input_stays_null() {
    let (base, request) = server("200 OK", sse(&[
        start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
        json!({"type":"content_block_stop","index":1}),
        stop("max_tokens")[0].clone(), stop("max_tokens")[1].clone(),
    ])).await;
    let events: Vec<_> = AnthropicProvider::new("test-key".into(), Some(base))
        .stream(Request {
            messages: vec![ChatMessage::user_text("Hi")],
            options: ilar::model::variant_options(MODEL, Some("high")).unwrap(),
            ..Request::with_model(MODEL)
        })
        .unwrap()
        .collect()
        .await;
    assert!(
        matches!(&events[..], [ProviderEvent::ReasoningItem { item }, ProviderEvent::ToolCallStarted { .. }, ProviderEvent::ToolCallInputDelta { .. }, ProviderEvent::ToolCallCompleted { input: Value::Null, .. }, ProviderEvent::TurnComplete { stop_reason: StopReason::MaxTokens, .. }] if item["block"] == json!({"type":"thinking","thinking":"","signature":"opaque"})),
        "{events:?}"
    );
    let (_, body) = request.await.unwrap();
    assert!(body["max_tokens"].as_u64().is_some_and(|max| max > 0));
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");
}

#[tokio::test]
async fn in_stream_overload_maps_to_rate_limit() {
    let (base, _) = server(
        "200 OK",
        sse(&[json!({"type":"error","error":{"type":"overloaded_error","message":"test-key"}})]),
    )
    .await;
    let events: Vec<_> = AnthropicProvider::new("test-key".into(), Some(base))
        .stream(Request {
            messages: vec![ChatMessage::user_text("Hi")],
            ..Request::with_model(MODEL)
        })
        .unwrap()
        .collect()
        .await;
    assert!(
        matches!(&events[..], [ProviderEvent::RateLimited { message, .. }] if !message.contains("test-key")),
        "{events:?}"
    );
}
