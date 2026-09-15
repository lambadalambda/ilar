//! The image_gen tool against a local stand-in for the images endpoint:
//! the request shape per auth mode, the edit's data URLs, and the saved
//! and attached result.

use ilar::tools::image_gen::ImageGenBackend;
use ilar::tools::{ToolContext, ToolRegistry};

/// One-shot server answering with a generated image; hands back the raw
/// request.
fn image_server(png: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
    use base64::Engine as _;
    let listener = futures::executor::block_on(async {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
    });
    let addr = listener.local_addr().unwrap();
    let body = serde_json::json!({
        "created": 1,
        "data": [{"b64_json": base64::engine::general_purpose::STANDARD.encode(&png)}]
    })
    .to_string();
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
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        String::from_utf8_lossy(&req).into_owned()
    });
    (format!("http://{addr}"), handle)
}

fn tiny_png() -> Vec<u8> {
    ilar::image::encode_png(2, 2, &[200u8; 16]).unwrap()
}

fn body_of(raw: &str) -> serde_json::Value {
    let (_, body) = raw.split_once("\r\n\r\n").expect("request body");
    serde_json::from_str(body).expect("json body")
}

fn header(raw: &str, name: &str) -> Option<String> {
    raw.lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        })
        .map(|(_, v)| v.trim().to_string())
}

async fn run(
    backend: ImageGenBackend,
    cwd: &std::path::Path,
    input: serde_json::Value,
) -> ilar::tools::ToolOutput {
    // A model that can be handed the drawing back: the tool only
    // attaches the image when the session's model accepts images.
    run_with_vision(backend, cwd, input, true).await
}

async fn run_with_vision(
    backend: ImageGenBackend,
    cwd: &std::path::Path,
    input: serde_json::Value,
    vision: bool,
) -> ilar::tools::ToolOutput {
    let registry = ToolRegistry::builtin().with_image_gen(backend).unwrap();
    let tool = registry.get("image_gen").expect("installed");
    let mut ctx = ToolContext::root(cwd.to_path_buf());
    ctx.session_id = "sess-1".into();
    ctx.call_id = Some("call_img_1".into());
    ctx.vision = vision;
    tool.run(input, ctx).await
}

/// A text-only session gets the file and is told so: the result used to
/// promise "the image is attached" to a model that cannot take one, and
/// the chat wire then substituted "[image omitted]".
#[tokio::test]
async fn a_text_only_session_gets_the_file_and_no_promise_of_an_image() {
    let dir = tempfile::tempdir().unwrap();
    let (base, server) = image_server(tiny_png());
    let backend = ImageGenBackend::with_api_key("k".into(), Some(base), dir.path().join("images"));

    let output = run_with_vision(
        backend,
        dir.path(),
        serde_json::json!({"prompt": "a lighthouse"}),
        false,
    )
    .await;
    let _ = server.await.unwrap();

    assert!(!output.is_error, "{}", output.content);
    assert!(output.images().is_empty(), "{:?}", output.images().len());
    assert!(
        output.content.contains("takes no images"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("call_img_1.png"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn an_api_key_generation_posts_to_generations_and_saves_the_png() {
    let dir = tempfile::tempdir().unwrap();
    let (base, server) = image_server(tiny_png());
    let backend =
        ImageGenBackend::with_api_key("sk-test".into(), Some(base), dir.path().join("images"));

    let output = run(
        backend,
        dir.path(),
        serde_json::json!({"prompt": "a teal otter, flat vector", "size": "1536x1024", "quality": "low"}),
    )
    .await;
    let raw = server.await.unwrap();

    assert!(!output.is_error, "{}", output.content);
    assert!(
        raw.starts_with("POST /images/generations HTTP/1.1"),
        "{raw}"
    );
    assert_eq!(
        header(&raw, "authorization").as_deref(),
        Some("Bearer sk-test")
    );
    assert!(header(&raw, "chatgpt-account-id").is_none(), "{raw}");
    assert!(header(&raw, "originator").is_none(), "{raw}");
    let body = body_of(&raw);
    assert_eq!(body["model"], "gpt-image-2");
    assert_eq!(body["prompt"], "a teal otter, flat vector");
    assert_eq!(body["size"], "1536x1024");
    assert_eq!(body["quality"], "low");
    assert!(body.get("images").is_none());

    // Saved under images/<session>/<call>.png, and attached.
    let saved = std::fs::read_dir(dir.path().join("images"))
        .unwrap()
        .flatten()
        .flat_map(|session_dir| std::fs::read_dir(session_dir.path()).unwrap().flatten())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(
        saved[0].file_name().and_then(|n| n.to_str()),
        Some("call_img_1.png")
    );
    assert_eq!(std::fs::read(&saved[0]).unwrap(), tiny_png());
    assert!(
        output.content.contains(saved[0].to_str().unwrap()),
        "{}",
        output.content
    );
    assert_eq!(output.images().len(), 1);
    assert_eq!(output.images()[0].media_type, "image/png");
}

#[tokio::test]
async fn a_chatgpt_login_signs_like_the_responses_wire() {
    let dir = tempfile::tempdir().unwrap();
    let store = ilar::auth::AuthStore::open(dir.path().join("state"));
    store
        .save(&ilar::auth::TokenSet {
            access_token: "chatgpt-token".into(),
            refresh_token: None,
            id_token: None,
            account_id: Some("acct_42".into()),
        })
        .await
        .unwrap();
    let (base, server) = image_server(tiny_png());
    let backend = ImageGenBackend::with_chatgpt_auth(store, Some(base), dir.path().join("images"));

    let output = run(
        backend,
        dir.path(),
        serde_json::json!({"prompt": "a lighthouse"}),
    )
    .await;
    let raw = server.await.unwrap();

    assert!(!output.is_error, "{}", output.content);
    assert_eq!(
        header(&raw, "authorization").as_deref(),
        Some("Bearer chatgpt-token")
    );
    assert_eq!(
        header(&raw, "chatgpt-account-id").as_deref(),
        Some("acct_42")
    );
    assert_eq!(header(&raw, "originator").as_deref(), Some("codex_cli_rs"));
    let body = body_of(&raw);
    assert_eq!(body["size"], "auto");
    assert_eq!(body["quality"], "auto");
}

#[tokio::test]
async fn references_turn_the_call_into_an_edit_with_data_urls() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("logo.png"), tiny_png()).unwrap();
    let (base, server) = image_server(tiny_png());
    let backend = ImageGenBackend::with_api_key("k".into(), Some(base), dir.path().join("images"));

    let output = run(
        backend,
        dir.path(),
        serde_json::json!({"prompt": "make it monochrome", "reference_paths": ["logo.png"]}),
    )
    .await;
    let raw = server.await.unwrap();

    assert!(!output.is_error, "{}", output.content);
    assert!(raw.starts_with("POST /images/edits HTTP/1.1"), "{raw}");
    let body = body_of(&raw);
    let url = body["images"][0]["image_url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"), "{url}");
}

#[tokio::test]
async fn bad_arguments_are_refused_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let backend = ImageGenBackend::with_api_key(
        "k".into(),
        Some("http://127.0.0.1:9".into()),
        dir.path().join("images"),
    );
    let refused = |input| run(backend.clone(), dir.path(), input);
    assert!(refused(serde_json::json!({"prompt": "  "})).await.is_error);
    assert!(
        refused(serde_json::json!({"prompt": "x", "size": "huge"}))
            .await
            .content
            .contains("WIDTHxHEIGHT")
    );
    assert!(
        refused(serde_json::json!({"prompt": "x", "quality": "ultra"}))
            .await
            .is_error
    );
    assert!(
        refused(serde_json::json!({"prompt": "x", "reference_paths": ["missing.png"]}))
            .await
            .content
            .contains("cannot read reference")
    );
    assert!(
        refused(serde_json::json!({"prompt": "x", "reference_paths": ["a","b","c","d","e","f"]}))
            .await
            .content
            .contains("at most 5")
    );
}
