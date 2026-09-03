//! The OpenCode gateways speak two OpenAI wires; the catalog row decides
//! which one a request takes.

use futures::StreamExt;
use ilar::provider::opencode::OpenCodeProvider;
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
        socket.write_all(sse_body.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        String::from_utf8_lossy(&req).into_owned()
    });
    (format!("http://{addr}"), handle)
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
    let events = drain(provider.stream(request("opencode/glm-5.2")).unwrap()).await;
    let raw = server.await.unwrap();

    assert!(raw.starts_with("POST /chat/completions HTTP/1.1"), "{raw}");
    assert!(raw.contains("authorization: Bearer k"), "{raw}");
    let body = body_of(&raw);
    assert_eq!(body["model"], "glm-5.2");
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
        .stream(request("opencode/glm-5.2"))
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

    assert_eq!(events[0], ProviderEvent::ThinkingDelta("The".into()));
    assert_eq!(events[1], ProviderEvent::ThinkingDelta(" user".into()));
    assert_eq!(events[2], ProviderEvent::ThinkingCompleted);
    assert_eq!(events[3], ProviderEvent::TextDelta("hi".into()));
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
