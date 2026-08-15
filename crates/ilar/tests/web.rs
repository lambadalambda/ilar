use ilar::tools::web::{SearchHit, SearchResults, html_to_text};
use ilar::tools::{ToolConcurrency, ToolContext, ToolRegistry, WorkspaceAccess};

// ---- html_to_text ----

#[test]
fn html_to_text_strips_tags_and_scripts() {
    let html = r#"
<html><head><style>body { color: red }</style></head>
<body>
<script>evil(); more();</script>
<h1>Title</h1>
<p>Some <b>bold</b> text &amp; an entity &#39;quote&#39;.</p>
</body></html>"#;
    let text = html_to_text(html);
    assert!(text.contains("Title"), "{text}");
    assert!(text.contains("bold text"), "{text}");
    assert!(text.contains("& an entity 'quote'."), "{text}");
    assert!(!text.contains("evil"), "{text}");
    assert!(!text.contains("color: red"), "{text}");
    assert!(!text.contains("<"), "{text}");
}

#[test]
fn html_to_text_collapses_whitespace() {
    let text = html_to_text("<p>a\n\n   b</p>\n\n\n<p>c</p>");
    assert!(!text.contains("\n\n\n"), "{text:?}");
    assert!(text.contains('a') && text.contains('b') && text.contains('c'));
}

// ---- webfetch tool ----

fn spawn_html_server(body: &'static str, content_type: &'static str) -> String {
    let listener = futures::executor::block_on(async {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
    });
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let _ = socket.read(&mut buf).await;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
    });
    format!("http://{addr}/page")
}

async fn fetch(reg: &ToolRegistry, url: &str) -> ilar::tools::ToolOutput {
    reg.get("webfetch")
        .expect("webfetch present")
        .run(
            serde_json::json!({"url": url}),
            ToolContext::root(std::env::temp_dir()),
        )
        .await
}

#[tokio::test]
async fn webfetch_converts_html_to_text() {
    let url = spawn_html_server(
        "<html><body><h1>Hello</h1><p>World of <a href='#'>links</a></p></body></html>",
        "text/html; charset=utf-8",
    );
    let (reg, _) = registry();
    let out = fetch(&reg, &url).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("Hello"), "{}", out.content);
    assert!(out.content.contains("links"), "{}", out.content);
    assert!(!out.content.contains("<a"), "{}", out.content);
}

#[tokio::test]
async fn webfetch_passes_plain_text_through() {
    let url = spawn_html_server("just plain text #hashtag 100% intact", "text/plain");
    let (reg, _) = registry();
    let out = fetch(&reg, &url).await;
    assert!(
        out.content.contains("just plain text #hashtag 100% intact"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn webfetch_http_error_is_tool_error() {
    // Connection refused -> error outcome.
    let (reg, _) = registry();
    let out = fetch(&reg, "http://127.0.0.1:1/nope").await;
    assert!(out.is_error);
}

// ---- websearch (mock backend) ----

struct MockBackend;

impl ilar::tools::web::SearchBackend for MockBackend {
    fn search(&self, query: &str, limit: usize) -> ilar::tools::web::ToolFutureSearch {
        let query = query.to_string();
        Box::pin(async move {
            Ok(SearchResults {
                hits: (0..limit)
                    .map(|i| SearchHit {
                        title: format!("result {i} for {query}"),
                        url: format!("https://example.com/{i}"),
                        snippet: format!("snippet about {query} number {i}"),
                    })
                    .collect(),
            })
        })
    }
}

fn registry() -> (
    ToolRegistry,
    std::sync::Arc<std::sync::Mutex<ilar::todo::TodoList>>,
) {
    let todos = std::sync::Arc::new(std::sync::Mutex::new(ilar::todo::TodoList::default()));
    (
        ToolRegistry::builtin()
            .with_todos(todos.clone())
            .unwrap()
            .with_search(Box::new(MockBackend))
            .unwrap(),
        todos,
    )
}

#[tokio::test]
async fn websearch_renders_hits() {
    let (reg, _) = registry();
    let out = reg
        .get("websearch")
        .unwrap()
        .run(
            serde_json::json!({"query": "rust async", "limit": 3}),
            ToolContext::root(std::env::temp_dir()),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("result 0 for rust async"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("https://example.com/2"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn websearch_empty_results_is_not_error() {
    struct Empty;
    impl ilar::tools::web::SearchBackend for Empty {
        fn search(&self, _query: &str, _limit: usize) -> ilar::tools::web::ToolFutureSearch {
            Box::pin(async { Ok(SearchResults { hits: vec![] }) })
        }
    }
    let reg = ToolRegistry::builtin()
        .with_search(Box::new(Empty))
        .unwrap();
    let out = reg
        .get("websearch")
        .unwrap()
        .run(
            serde_json::json!({"query": "nothing"}),
            ToolContext::root(std::env::temp_dir()),
        )
        .await;
    assert!(!out.is_error);
    assert!(out.content.to_lowercase().contains("no results"));
}

#[test]
fn web_tools_are_read_only() {
    let (reg, _) = registry();
    for name in ["webfetch", "websearch"] {
        let tool = reg.get(name).unwrap();
        assert_eq!(tool.concurrency(), ToolConcurrency::Concurrent);
        assert_eq!(tool.workspace_access(), WorkspaceAccess::None);
    }
}
