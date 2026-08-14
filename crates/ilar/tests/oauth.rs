use base64::Engine;
use futures::StreamExt;
use ilar::auth::{
    AuthStore, account_id_from_id_token, login_flow, parse_callback, pkce_from_verifier,
};
use ilar::provider::openai::OpenAIProvider;
use ilar::provider::{Provider, ProviderEvent, Request, StopReason};

fn b64url(json: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes())
}

#[test]
fn pkce_matches_rfc7636_vector() {
    // RFC 7636 Appendix B test vector.
    let pkce = pkce_from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    assert_eq!(
        pkce.challenge,
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn account_id_extracted_from_id_token() {
    let payload =
        r#"{"sub":"user_1","https://api.openai.com/auth":{"chatgpt_account_id":"acc_12345"}}"#;
    let jwt = format!(
        "{}.{}.{}",
        b64url(r#"{"alg":"none"}"#),
        b64url(payload),
        "sig"
    );
    assert_eq!(account_id_from_id_token(&jwt).as_deref(), Some("acc_12345"));
    assert_eq!(account_id_from_id_token("garbage"), None);
}

#[test]
fn callback_request_parsed() {
    let head = b"GET /auth/callback?code=SplxlOBeZQQYbYS6WxSbIA&state=xyz HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    let (code, state) = parse_callback(head).expect("parses");
    assert_eq!(code, "SplxlOBeZQQYbYS6WxSbIA");
    assert_eq!(state, "xyz");
    assert!(parse_callback(b"GET /nope HTTP/1.1\r\n\r\n").is_none());
}

#[test]
fn auth_store_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().to_path_buf());
    assert!(store.load().is_none());
    store
        .save(&ilar::auth::TokenSet {
            access_token: "a1".into(),
            refresh_token: Some("r1".into()),
            id_token: None,
            account_id: Some("acc".into()),
            expires_at: Some(4102444800),
        })
        .unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.access_token, "a1");
    assert_eq!(loaded.refresh_token.as_deref(), Some("r1"));
    assert_eq!(loaded.account_id.as_deref(), Some("acc"));
}

/// Mock token endpoint + mock responses endpoint; proves refresh-on-401.
#[tokio::test]
async fn chatgpt_auth_refreshes_on_401_and_retries() {
    // --- token server ---
    let token_listener = futures::executor::block_on(async {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
    });
    let token_addr = token_listener.local_addr().unwrap();
    let token_handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = token_listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(head_end) = text.find("\r\n\r\n") {
                let content_length = text
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if buf.len() >= head_end + 4 + content_length {
                    break;
                }
            }
        }
        let body = String::from_utf8_lossy(&buf);
        assert!(
            body.contains("grant_type=refresh_token") && body.contains("refresh_token=r1"),
            "unexpected token request: {body}"
        );
        let payload =
            r#"{"sub":"u","https://api.openai.com/auth":{"chatgpt_account_id":"acc_99"}}"#;
        let jwt = format!(
            "{}.{}.{}",
            b64url(r#"{"alg":"none"}"#),
            b64url(payload),
            "sig"
        );
        let json = format!(
            r#"{{"access_token":"t2","refresh_token":"r2","id_token":"{jwt}","token_type":"Bearer","expires_in":3600}}"#
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
            json.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    // --- responses server: 401 first, then SSE ---
    let resp_listener = futures::executor::block_on(async {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
    });
    let resp_addr = resp_listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_clone = seen.clone();
    let resp_handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for round in 0..2 {
            let (mut socket, _) = resp_listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let content_length = text
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + content_length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf);
            let auth = text
                .lines()
                .find(|l| l.to_lowercase().starts_with("authorization:"))
                .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
                .unwrap_or_default();
            seen_clone.lock().unwrap().push(auth);
            if round == 0 {
                socket
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
            } else {
                let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n";
                socket
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}").as_bytes())
                    .await
                    .unwrap();
            }
        }
    });

    // --- provider with expired token + refresh token ---
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().to_path_buf());
    store
        .save(&ilar::auth::TokenSet {
            access_token: "stale".into(),
            refresh_token: Some("r1".into()),
            id_token: None,
            account_id: None,
            expires_at: Some(0),
        })
        .unwrap();
    let provider =
        OpenAIProvider::with_chatgpt_auth(store.clone(), Some(format!("http://{resp_addr}")))
            .with_token_url_for_test(format!("http://{token_addr}/oauth/token"));

    let mut stream = provider
        .stream(Request::with_model("openai/gpt-5.2"))
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    token_handle.await.unwrap();
    resp_handle.await.unwrap();

    assert!(
        matches!(&events[0], ProviderEvent::TextDelta(t) if t == "hello"),
        "{events:?}"
    );
    assert_eq!(
        events.last().unwrap().clone().stop_reason(),
        Some(StopReason::EndTurn)
    );
    // Two different bearer tokens seen: stale, then refreshed t2.
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.iter()
            .map(|s| s.replace("Bearer ", ""))
            .collect::<Vec<_>>(),
        vec!["stale".to_string(), "t2".to_string()],
        "{seen:?}"
    );
    // Store now holds the refreshed tokens.
    assert_eq!(store.load().unwrap().access_token, "t2");
    assert_eq!(store.load().unwrap().account_id.as_deref(), Some("acc_99"));
}

#[tokio::test]
async fn chatgpt_auth_without_tokens_is_preflight_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().to_path_buf());
    let provider = OpenAIProvider::with_chatgpt_auth(store, Some("http://127.0.0.1:1".into()));
    let err = provider
        .stream(Request::with_model("openai/gpt-5.2"))
        .err()
        .expect("preflight error");
    assert!(err.to_string().contains("login"), "{err}");
}

#[tokio::test]
async fn login_flow_times_out_without_callback() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().to_path_buf());
    let result = login_flow(&store, std::time::Duration::from_millis(300), false).await;
    assert!(result.is_err());
}
