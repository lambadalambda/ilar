use base64::Engine;
use futures::StreamExt;
use ilar::auth::{
    AuthStore, OAuthCallback, account_id_from_id_token, login_flow, parse_callback,
    pkce_from_verifier, refresh_tokens,
};
use ilar::provider::openai::OpenAIProvider;
use ilar::provider::{Provider, ProviderEvent, Request, StopReason};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    assert_eq!(
        parse_callback(head).expect("parses"),
        OAuthCallback::Code {
            code: "SplxlOBeZQQYbYS6WxSbIA".into(),
            state: "xyz".into(),
        }
    );
    assert!(parse_callback(b"GET /nope HTTP/1.1\r\n\r\n").is_err());
}

#[test]
fn callback_parameters_are_percent_decoded() {
    let head = b"GET /auth/callback?code=a%2Bb%2Fc%3D&state=hello%20world HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert_eq!(
        parse_callback(head).expect("parses encoded callback"),
        OAuthCallback::Code {
            code: "a+b/c=".into(),
            state: "hello world".into(),
        }
    );
}

#[test]
fn callback_denial_is_parsed_with_description() {
    let head = b"GET /auth/callback?error=access_denied&error_description=No%20thanks&state=xyz HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert_eq!(
        parse_callback(head).expect("parses denial callback"),
        OAuthCallback::Error {
            error: "access_denied".into(),
            description: Some("No thanks".into()),
            state: "xyz".into(),
        }
    );
}

#[test]
fn callback_rejects_ambiguous_or_empty_parameters() {
    for target in [
        "/auth/callback?code=&state=xyz",
        "/auth/callback?code=one&code=two&state=xyz",
        "/auth/callback?code=one&error=access_denied&state=xyz",
        "/auth/callback?code=one&error_description=denied&state=xyz",
        "/auth/callback?code=one&state=",
    ] {
        let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(parse_callback(request.as_bytes()).is_err(), "{target}");
    }
}

#[tokio::test]
async fn auth_store_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("new/state/directory");
    let store = AuthStore::open(state_dir);
    assert!(store.load().unwrap().is_none());
    store
        .save(&ilar::auth::TokenSet {
            access_token: "a1".into(),
            refresh_token: Some("r1".into()),
            id_token: None,
            account_id: Some("acc".into()),
            expires_at: Some(4102444800),
        })
        .await
        .unwrap();
    let loaded = store.load().unwrap().unwrap();
    assert_eq!(loaded.access_token, "a1");
    assert_eq!(loaded.refresh_token.as_deref(), Some("r1"));
    assert_eq!(loaded.account_id.as_deref(), Some("acc"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(store.tokens_path()).unwrap().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn auth_store_distinguishes_missing_and_malformed_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().to_path_buf());
    assert!(store.load().unwrap().is_none());

    std::fs::write(store.tokens_path(), "not-json").unwrap();
    let error = store.load().expect_err("malformed store must be an error");
    assert!(
        error.to_string().contains("parsing OAuth store"),
        "{error:#}"
    );

    std::fs::remove_file(store.tokens_path()).unwrap();
    std::fs::create_dir(store.tokens_path()).unwrap();
    let error = store.load().expect_err("unreadable store must be an error");
    assert!(
        error.to_string().contains("reading OAuth store"),
        "{error:#}"
    );
}

#[cfg(unix)]
#[test]
fn auth_store_rejects_symlinks_instead_of_treating_them_as_missing() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().to_path_buf());
    let target = dir.path().join("target.json");
    std::fs::write(&target, r#"{"access_token":"secret"}"#).unwrap();
    symlink(&target, store.tokens_path()).unwrap();
    assert!(store.load().is_err());

    std::fs::remove_file(store.tokens_path()).unwrap();
    symlink(dir.path().join("missing.json"), store.tokens_path()).unwrap();
    assert!(store.load().is_err());
}

#[tokio::test]
async fn concurrent_refreshes_rotate_once_and_share_the_result() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = 0;
        loop {
            let accepted = tokio::time::timeout(
                if requests == 0 {
                    std::time::Duration::from_secs(2)
                } else {
                    std::time::Duration::from_millis(200)
                },
                listener.accept(),
            )
            .await;
            let Ok(Ok((mut socket, _))) = accepted else {
                break;
            };
            requests += 1;
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let content_length = text
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + content_length {
                        break;
                    }
                }
            }
            let token = format!("t{}", requests + 1);
            let body = format!(
                r#"{{"access_token":"{token}","refresh_token":"rotated","expires_in":3600}}"#
            );
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        requests
    });

    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().to_path_buf());
    store
        .save(&ilar::auth::TokenSet {
            access_token: "stale".into(),
            refresh_token: Some("r1".into()),
            id_token: None,
            account_id: Some("account".into()),
            expires_at: Some(0),
        })
        .await
        .unwrap();
    let http = reqwest::Client::new();
    let token_url = format!("http://{address}/oauth/token");
    let (first, second) = tokio::join!(
        refresh_tokens(&store, "stale", &token_url, &http),
        refresh_tokens(&store, "stale", &token_url, &http),
    );

    assert_eq!(first.unwrap().access_token, "t2");
    assert_eq!(second.unwrap().access_token, "t2");
    assert_eq!(server.await.unwrap(), 1);
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
        .await
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
    assert_eq!(store.load().unwrap().unwrap().access_token, "t2");
    assert_eq!(
        store.load().unwrap().unwrap().account_id.as_deref(),
        Some("acc_99")
    );
}

#[tokio::test]
async fn chatgpt_auth_without_tokens_is_preflight_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().to_path_buf());
    let provider =
        OpenAIProvider::with_chatgpt_auth(store.clone(), Some("http://127.0.0.1:1".into()));
    let err = provider
        .stream(Request::with_model("openai/gpt-5.2"))
        .err()
        .expect("preflight error");
    assert!(err.to_string().contains("login"), "{err}");

    std::fs::write(store.tokens_path(), "not-json").unwrap();
    let provider = OpenAIProvider::with_chatgpt_auth(store, Some("http://127.0.0.1:1".into()));
    let err = provider
        .stream(Request::with_model("openai/gpt-5.2"))
        .err()
        .expect("malformed auth must fail preflight");
    assert!(err.to_string().contains("auth store"), "{err:#}");
    assert!(!err.to_string().contains("not logged in"), "{err:#}");
}

#[tokio::test]
async fn login_flow_times_out_without_callback() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::open(dir.path().to_path_buf());
    let started = std::time::Instant::now();
    let login_store = store.clone();
    let login = tokio::spawn(async move {
        login_flow(&login_store, std::time::Duration::from_millis(500), false).await
    });
    let mut spurious = loop {
        match tokio::net::TcpStream::connect("127.0.0.1:1455").await {
            Ok(socket) => break socket,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    };
    spurious
        .write_all(b"GET /not-the-callback HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    drop(spurious);
    let mut slow = tokio::net::TcpStream::connect("127.0.0.1:1455")
        .await
        .unwrap();
    slow.write_all(b"GET /auth/call").await.unwrap();
    let result = login.await.unwrap();
    assert!(result.is_err());
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(400),
        "spurious callback ended login early: {:?}",
        started.elapsed()
    );
}

/// Live smoke: real ChatGPT backend through ilar's provider.
/// Requires ILAR_LIVE_CHATGPT_STATE_DIR pointing at a state dir whose
/// auth.json holds a valid ChatGPT OAuth TokenSet.
///   cargo test -p ilar --test oauth live_chatgpt -- --ignored --nocapture
#[tokio::test]
#[ignore]
async fn live_chatgpt_backend_text_turn() {
    let state_dir = std::env::var("ILAR_LIVE_CHATGPT_STATE_DIR")
        .expect("ILAR_LIVE_CHATGPT_STATE_DIR with seeded auth.json");
    let store = AuthStore::open(state_dir.into());
    let provider = OpenAIProvider::with_chatgpt_auth(store, None);

    let mut stream = provider
        .stream(Request {
            messages: vec![ilar::session::ChatMessage::user_text(
                "Reply with exactly: hello",
            )],
            ..Request::with_model("openai/gpt-5.6-sol")
        })
        .unwrap();
    let mut text = String::new();
    let mut terminal = None;
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(t) => text.push_str(&t),
            ProviderEvent::TurnComplete { stop_reason, .. } => {
                terminal = Some(stop_reason);
                break;
            }
            ProviderEvent::Error(e) => panic!("provider error: {e}"),
            _ => {}
        }
    }
    println!("chatgpt-backend text: {text}");
    assert!(!text.is_empty());
    assert_eq!(terminal, Some(StopReason::EndTurn));
}

async fn live_chatgpt_prompt_cache_probe(keyed: bool) {
    let state_dir = std::env::var("ILAR_LIVE_CHATGPT_STATE_DIR")
        .expect("ILAR_LIVE_CHATGPT_STATE_DIR with seeded auth.json");
    let provider = OpenAIProvider::with_chatgpt_auth(AuthStore::open(state_dir.into()), None);
    let provider = if keyed {
        provider.with_prompt_cache_key_for_test()
    } else {
        provider
    };
    let cache_key = format!("ilar-live-probe-{}", uuid::Uuid::new_v4());
    let synthetic_prefix = (0..3_000)
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let mut observed = Vec::new();

    for attempt in 0..3 {
        let mut request = Request::with_model("openai/gpt-5.6-sol");
        request.messages = vec![ilar::session::ChatMessage::user_text(
            synthetic_prefix.clone(),
        )];
        request.cache_key = keyed.then(|| cache_key.clone());
        request.options = serde_json::json!({"reasoning": {"effort": "low"}});
        let mut stream = provider.stream(request).unwrap();
        let usage = loop {
            match stream.next().await {
                Some(ProviderEvent::TurnComplete { usage, .. }) => break usage,
                Some(ProviderEvent::Error(error)) => panic!("provider error: {error}"),
                Some(_) => {}
                None => panic!("provider stream ended without usage"),
            }
        };
        println!(
            "{} cache probe {}: uncached={} cached={} output={}",
            if keyed { "keyed" } else { "automatic" },
            attempt + 1,
            usage.input_tokens,
            usage.cache_read_input_tokens,
            usage.output_tokens
        );
        assert!(
            usage
                .input_tokens
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens)
                >= 1_024,
            "probe input was not cache eligible"
        );
        observed.push(usage);
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    assert_eq!(observed.len(), 3);
}

/// Live ChatGPT prompt-cache routing probe. Prints token counts only, never
/// prompt or response content.
#[tokio::test]
#[ignore]
async fn live_chatgpt_prompt_cache_routing_probe() {
    live_chatgpt_prompt_cache_probe(false).await;
}

/// Live support probe for the undocumented ChatGPT cache-key behavior.
#[tokio::test]
#[ignore]
async fn live_chatgpt_prompt_cache_key_probe() {
    live_chatgpt_prompt_cache_probe(true).await;
}
