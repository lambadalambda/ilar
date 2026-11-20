//! OpenCode wire routing and local SSE round trips.

use futures::StreamExt;
use ilar::provider::opencode::OpenCodeProvider;
use ilar::provider::zai::ZaiProvider;
use ilar::provider::{Provider, ProviderEvent, Request, ToolDefinition};

fn request(model: &str) -> Request {
    Request {
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        messages: vec![ilar::session::ChatMessage::user_text("hi")],
        ..Request::with_model(model)
    }
}

/// One-shot server: answers the first request with `sse_body` and hands
/// back the raw request text.
fn http_server(sse_body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    let (base, handle) = http_server_steps(&[sse_body]);
    (
        base,
        tokio::spawn(async move { handle.await.unwrap().remove(0) }),
    )
}

fn http_server_steps(bodies: &[&'static str]) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let bodies = bodies.to_vec();
    let listener = futures::executor::block_on(async {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
    });
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut requests = Vec::new();
        for sse_body in bodies {
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
            socket.write_all(sse_body.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            requests.push(String::from_utf8_lossy(&req).into_owned());
        }
        requests
    });
    (format!("http://{addr}"), handle)
}

const NEON: &str = "opencode/gemini-4-neon";
const NEON_TEXT: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hel\"}]}}]}\n\n",
    "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo\"}]},\"finishReason\":\"STOP\"}]}\n\n",
    "data: {\"usageMetadata\":{\"promptTokenCount\":21,\"cachedContentTokenCount\":8,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":3}}\n\n",
);

#[tokio::test]
async fn neon_streams_text_on_the_native_wire_with_zen_affinity() {
    use ilar::provider::StopReason;
    let (base, server) = http_server(NEON_TEXT);
    let provider = OpenCodeProvider::zen(
        "neon-key".into(),
        Some(format!("{base}/proxy/google/v1beta/")),
    );
    let events = drain(
        provider
            .stream(Request {
                cache_key: Some("neon-session".into()),
                system_prompt: Some("be brief".into()),
                options: ilar::model::with_output_cap(
                    ilar::model::variant_options(NEON, Some("xhigh")).unwrap_or_default(),
                    NEON,
                    Some(12),
                ),
                ..request(NEON)
            })
            .unwrap(),
    )
    .await;
    let raw = server.await.unwrap();
    assert!(
        raw.starts_with(
            "POST /proxy/google/v1beta/models/gemini-4-neon:streamGenerateContent?alt=sse HTTP/1.1"
        ),
        "{raw}"
    );
    for header in [
        "authorization: Bearer neon-key",
        "x-opencode-session: neon-session",
        "x-opencode-client: ilar",
        concat!("user-agent: ilar/", env!("CARGO_PKG_VERSION")),
    ] {
        assert!(raw.contains(header), "{raw}");
    }
    let body = body_of(&raw);
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
    assert_eq!(
        body["contents"][0],
        serde_json::json!({"role":"user","parts":[{"text":"hi"}]})
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
        serde_json::json!({"type":"object"})
    );
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "XHIGH"
    );
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 12);
    assert_eq!(ProviderEvent::text_of(&events), "hello");
    assert!(
        matches!(events.as_slice(), [ProviderEvent::TextDelta(_), ProviderEvent::TextDelta(_), ProviderEvent::TurnComplete { stop_reason: StopReason::EndTurn, usage }] if (usage.input_tokens, usage.cache_read_input_tokens, usage.output_tokens) == (13, 8, 5)),
        "{events:?}"
    );
}

#[tokio::test]
async fn neon_replays_signed_parallel_calls_from_the_session_log() {
    use ilar::agent::{LoopConfig, loop_event_channel, run_turn};
    use ilar::session::{ContentBlock, SessionEvent, SessionMeta, SessionStore, new_id};
    use ilar::tools::{ToolContext, ToolRegistry};
    use tokio_util::sync::CancellationToken;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("first.txt"), "first fixture").unwrap();
    std::fs::write(dir.path().join("second.txt"), "second fixture").unwrap();
    let store = SessionStore::new(dir.path().join("sessions"));
    let id = new_id();
    store
        .create(SessionMeta {
            session_id: id.clone(),
            parent_id: None,
            agent: "build".into(),
            model: NEON.into(),
            workspace: None,
            cwd: None,
        })
        .unwrap();
    let (base, server) = http_server_steps(&[
        concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Looking.\"},{\"functionCall\":{\"id\":\"native-first\",\"name\":\"read\",\"args\":{\"path\":\"first.txt\"}},\"thoughtSignature\":\"c2lnLWE=\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"second.txt\"}}}]},\"finishReason\":\"STOP\"}]}\n\n",
        ),
        NEON_TEXT,
    ]);
    let provider = OpenCodeProvider::zen("k".into(), Some(base));
    run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &id,
        "read both files",
        &[],
        None,
        LoopConfig::default(),
        loop_event_channel(256).0,
        CancellationToken::new(),
        ToolContext::root(dir.path().to_path_buf()),
        None,
    )
    .await
    .unwrap();
    let session = store.load(&id).unwrap();
    assert!(session.events().iter().any(|event| matches!(event,
        SessionEvent::AssistantMessage { content, .. }
        if content.windows(2).any(|pair| matches!(pair,
            [ContentBlock::Reasoning { item }, ContentBlock::ToolCall { id, .. }]
            if item["signature"] == "c2lnLWE=" && id == "native-first")))));
    assert!(
        !session
            .transcript_for("opencode/glm-5.3")
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::Reasoning { .. }))
    );
    let raw = tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    let body = body_of(&raw[1]);
    let contents = body["contents"].as_array().unwrap();
    let calls = &contents[1];
    assert_eq!(calls["role"], "model");
    assert_eq!(
        calls["parts"],
        serde_json::json!([
            {"text":"Looking."},
            {"functionCall":{"id":"native-first","name":"read","args":{"path":"first.txt"}},"thoughtSignature":"c2lnLWE="},
            {"functionCall":{"name":"read","args":{"path":"second.txt"}}}
        ])
    );
    assert_eq!(contents[2]["role"], "user");
    let results = contents[2]["parts"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["functionResponse"]["id"], "native-first");
    assert_eq!(results[0]["functionResponse"]["name"], "read");
    assert!(
        results[0].to_string().contains("first fixture"),
        "{results:?}"
    );
    assert!(results[1]["functionResponse"].get("id").is_none());
    assert!(
        results[1].to_string().contains("second fixture"),
        "{results:?}"
    );
}

#[tokio::test]
async fn neon_rejects_a_batch_whose_first_call_has_no_signature() {
    let (base, _server) = http_server(concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"first\"}}}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"second\"}},\"thoughtSignature\":\"c2ln\"}]},\"finishReason\":\"STOP\"}]}\n\n",
    ));
    let provider = OpenCodeProvider::zen("neon-key".into(), Some(base));
    let events = drain(provider.stream(request(NEON)).unwrap()).await;
    assert!(
        matches!(events.as_slice(), [ProviderEvent::Error(error)] if error.contains("thoughtSignature")),
        "{events:?}"
    );
}

#[tokio::test]
async fn neon_resumes_a_foreign_tool_chain_as_text() {
    use ilar::agent::{LoopConfig, loop_event_channel, resume_turn, run_turn};
    use ilar::provider::{MockProvider, StopReason};
    use ilar::session::{SessionEvent, SessionMeta, SessionStore, new_id};
    use ilar::tools::{ToolContext, ToolRegistry};
    use tokio_util::sync::CancellationToken;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fixture.txt"), "foreign result fixture").unwrap();
    let store = SessionStore::new(dir.path().join("sessions"));
    let id = new_id();
    let foreign = "opencode/glm-5.3";
    store
        .create(SessionMeta {
            session_id: id.clone(),
            parent_id: None,
            agent: "build".into(),
            model: foreign.into(),
            workspace: None,
            cwd: None,
        })
        .unwrap();
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "foreign-call".into(),
                name: "read".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "foreign-call".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "fixture.txt"}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![ProviderEvent::Error("connection lost".into())],
    ]);
    let registry = ToolRegistry::builtin();
    let context = || ToolContext::root(dir.path().to_path_buf());
    let failed = run_turn(
        &provider,
        &registry,
        &store,
        &id,
        "read the fixture",
        &[],
        None,
        LoopConfig::default(),
        loop_event_channel(256).0,
        CancellationToken::new(),
        context(),
        None,
    )
    .await;
    assert!(failed.unwrap_err().to_string().contains("connection lost"));
    store
        .acquire_writer(&id)
        .unwrap()
        .load()
        .unwrap()
        .append(SessionEvent::ModelChange {
            id: new_id(),
            model: NEON.into(),
            variant: None,
            ts: chrono::Utc::now(),
        })
        .unwrap();

    let (base, server) = http_server(NEON_TEXT);
    let neon = OpenCodeProvider::zen("neon-key".into(), Some(base));
    resume_turn(
        &neon,
        &registry,
        &store,
        &id,
        None,
        LoopConfig::default(),
        loop_event_channel(256).0,
        CancellationToken::new(),
        context(),
        None,
    )
    .await
    .unwrap();
    let raw = server.await.unwrap();
    let body = body_of(&raw);
    let parts = body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .collect::<Vec<_>>();
    assert!(
        parts.iter().all(|part| part.get("functionCall").is_none()
            && part.get("functionResponse").is_none()
            && part.get("thoughtSignature").is_none()),
        "{raw}"
    );
    let text = parts
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("read the fixture")
            && text.contains("fixture.txt")
            && text.contains("foreign result fixture"),
        "{text}"
    );
}

const CHAT_TURN: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{}}\n\n",
    "data: [DONE]\n\n",
);

async fn drain(mut stream: ilar::provider::EventStream) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn body_of(raw: &str) -> serde_json::Value {
    let (_, body) = raw.split_once("\r\n\r\n").expect("request body");
    serde_json::from_str(body).expect("json body")
}

#[tokio::test]
async fn chat_rows_post_to_chat_completions_without_zai_fields() {
    let (base, server) = http_server(CHAT_TURN);
    let provider = OpenCodeProvider::zen("k".into(), Some(base));
    let events = drain(provider.stream(request("opencode/glm-5.3")).unwrap()).await;
    let raw = server.await.unwrap();

    assert!(raw.starts_with("POST /chat/completions HTTP/1.1"), "{raw}");
    assert!(raw.contains("authorization: Bearer k"), "{raw}");
    let body = body_of(&raw);
    assert_eq!(body["model"], "glm-5.3");
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body.get("tool_stream").is_none(), "tool_stream is z.ai's");
    assert!(events.contains(&ProviderEvent::TextDelta("ok".into())));
}

#[tokio::test]
async fn responses_rows_post_to_responses() {
    let (base, server) = http_server(
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    );
    let provider = OpenCodeProvider::go("k".into(), Some(base));
    let _ = drain(
        provider
            .stream(request("opencode-go/gpt-5.6-luna"))
            .unwrap(),
    )
    .await;
    let raw = server.await.unwrap();

    assert!(raw.starts_with("POST /responses HTTP/1.1"), "{raw}");
    let body = body_of(&raw);
    assert_eq!(body["model"], "gpt-5.6-luna");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert!(body.get("input").is_some());
}

#[tokio::test]
async fn unknown_ids_take_the_chat_wire() {
    let (base, server) = http_server(CHAT_TURN);
    let provider = OpenCodeProvider::zen("k".into(), Some(base));
    let _ = drain(provider.stream(request("opencode/mystery-9")).unwrap()).await;
    let raw = server.await.unwrap();
    assert!(raw.starts_with("POST /chat/completions HTTP/1.1"), "{raw}");
    assert_eq!(body_of(&raw)["model"], "mystery-9");
}

#[tokio::test]
async fn each_gateway_refuses_the_other_prefix() {
    let zen = OpenCodeProvider::zen("k".into(), Some("http://127.0.0.1:9".into()));
    let go = OpenCodeProvider::go("k".into(), Some("http://127.0.0.1:9".into()));
    assert_eq!(zen.provider_prefix(), Some("opencode"));
    assert_eq!(go.provider_prefix(), Some("opencode-go"));

    // Both wires check the prefix: a Responses row and a chat row alike.
    let err = zen
        .stream(request("opencode-go/gpt-5.6-luna"))
        .err()
        .expect("go id on zen");
    assert!(err.to_string().contains("expected opencode"), "{err}");
    let err = go
        .stream(request("opencode/glm-5.3"))
        .err()
        .expect("zen id on go");
    assert!(err.to_string().contains("expected opencode-go"), "{err}");
}

/// Kimi, Nemotron and Ling behind Zen stream thinking as `reasoning`
/// rather than `reasoning_content`; both spell the same delta.
#[tokio::test]
async fn reasoning_is_read_under_either_spelling() {
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\",\"reasoning\":\"The\",\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"The\"}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning\":\" user\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{}}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _server) = http_server(sse);
    let provider = OpenCodeProvider::zen("k".into(), Some(base));
    let events = drain(provider.stream(request("opencode/kimi-k3")).unwrap()).await;

    // The other spelling is announced ahead of its first delta, so the
    // thought goes back under the same name.
    assert_eq!(
        events[0],
        ProviderEvent::ThinkingField(ilar::session::ReasoningField::Reasoning)
    );
    assert_eq!(events[1], ProviderEvent::ThinkingDelta("The".into()));
    assert_eq!(events[2], ProviderEvent::ThinkingDelta(" user".into()));
    assert_eq!(events[3], ProviderEvent::ThinkingCompleted);
    assert_eq!(events[4], ProviderEvent::TextDelta("hi".into()));
}

/// Moonshot's Kimi sends usage in a trailer that repeats the finish
/// reason with an empty delta; that is a usage chunk, not a late event.
#[tokio::test]
async fn kimi_usage_trailer_repeats_the_finish_reason() {
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}],\"usage\":{\"prompt_tokens\":211,\"completion_tokens\":99,\"prompt_tokens_details\":{\"cached_tokens\":0}}}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _server) = http_server(sse);
    let provider = OpenCodeProvider::zen("k".into(), Some(base));
    let events = drain(provider.stream(request("opencode/kimi-k3")).unwrap()).await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::Error(_))),
        "{events:?}"
    );
    let Some(ProviderEvent::TurnComplete { stop_reason, usage }) = events.last() else {
        panic!("{events:?}");
    };
    assert_eq!(*stop_reason, ilar::provider::StopReason::ToolUse);
    assert_eq!(usage.input_tokens, 211);
    assert_eq!(usage.output_tokens, 99);
}

/// A content-free trailer may repeat the finish reason, not change it.
#[tokio::test]
async fn a_trailer_that_changes_the_finish_reason_is_refused() {
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],\"usage\":{}}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _server) = http_server(sse);
    let provider = OpenCodeProvider::zen("k".into(), Some(base));
    let events = drain(provider.stream(request("opencode/kimi-k3")).unwrap()).await;
    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(error)) if error.contains("duplicate")),
        "{events:?}"
    );
}

/// The gateway keys on `x-opencode-session` and refuses requests without
/// one from 2026-09-06; both wires name the session, the client and a
/// user agent, and a request outside any session still names one.
#[tokio::test]
async fn both_wires_name_their_session_to_the_gateway() {
    let header = |raw: &str, name: &str| -> Option<String> {
        raw.lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            })
            .map(|(_, v)| v.trim().to_string())
    };
    let with_session = |model: &str| Request {
        cache_key: Some("session-42".into()),
        ..request(model)
    };

    let (base, server) = http_server(CHAT_TURN);
    let zen = OpenCodeProvider::zen("k".into(), Some(base));
    let _ = drain(zen.stream(with_session("opencode/glm-5.3")).unwrap()).await;
    let raw = server.await.unwrap();
    assert_eq!(
        header(&raw, "x-opencode-session").as_deref(),
        Some("session-42")
    );
    assert_eq!(header(&raw, "x-opencode-client").as_deref(), Some("ilar"));
    assert_eq!(
        header(&raw, "user-agent").as_deref(),
        Some(concat!("ilar/", env!("CARGO_PKG_VERSION")))
    );

    let (base, server) = http_server(
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    );
    let go = OpenCodeProvider::go("k".into(), Some(base));
    let _ = drain(go.stream(with_session("opencode-go/gpt-5.6-luna")).unwrap()).await;
    let raw = server.await.unwrap();
    assert_eq!(
        header(&raw, "x-opencode-session").as_deref(),
        Some("session-42")
    );
    assert_eq!(header(&raw, "x-opencode-client").as_deref(), Some("ilar"));
    // The Codex pair is that backend's, not the gateway's.
    assert!(header(&raw, "session-id").is_none(), "{raw}");

    // No session (topic naming): a per-process one stands in.
    let (base, server) = http_server(CHAT_TURN);
    let zen = OpenCodeProvider::zen("k".into(), Some(base));
    let _ = drain(zen.stream(request("opencode/glm-5.3")).unwrap()).await;
    let raw = server.await.unwrap();
    assert_eq!(
        header(&raw, "x-opencode-session").as_deref(),
        Some(format!("ilar-process-{}", std::process::id()).as_str())
    );

    // z.ai gets none of it.
    let (base, server) = http_server(CHAT_TURN);
    let zai = ZaiProvider::new("k".into(), Some(base));
    let _ = drain(
        zai.stream(Request {
            cache_key: Some("session-42".into()),
            ..Request::with_model("zai/glm-4.7")
        })
        .unwrap(),
    )
    .await;
    let raw = server.await.unwrap();
    assert!(header(&raw, "x-opencode-session").is_none(), "{raw}");
    assert!(header(&raw, "user-agent").is_none(), "{raw}");
}
