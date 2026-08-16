use std::sync::{Arc, Mutex};

use ilar::tools::web::{SearchBackend, SearchHit, SearchResults, WebSearchTool, html_to_text};
use ilar::tools::{Tool, ToolConcurrency, ToolContext, ToolRegistry, WorkspaceAccess};

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

#[test]
fn html_to_text_is_unicode_safe_before_hidden_tags() {
    let text = html_to_text("İstanbul<script>bad()</script><STYLE>also bad</STYLE>safe");
    assert_eq!(text, "İstanbul\nsafe");
}

#[test]
fn html_to_text_preserves_adjacent_block_boundaries() {
    let text = html_to_text("<h1>Title</h1><p>first</p><p>second<br>line</p><li>item</li>");
    assert_eq!(text, "Title\nfirst\nsecond\nline\nitem");
}

#[test]
fn html_to_text_handles_gt_inside_quoted_attributes() {
    let text = html_to_text(r#"<div title="1 > 0">safe</div><p>after</p>"#);
    assert_eq!(text, "safe\nafter");
}

#[test]
fn html_to_text_handles_comparisons_inside_raw_text_elements() {
    let text = html_to_text("<script>if (a < 'x') { bad(); }</script><p>safe</p>");
    assert_eq!(text, "safe");
}

#[test]
fn html_to_text_ignores_raw_text_closing_lookalikes() {
    let html = concat!(
        "<script>const a = '</script=bad>'; const b = '</script.foo>'; ",
        "const c = '< /script>'; const d = '</ script>'; evil();</script>",
        "<p>safe</p>",
    );
    assert_eq!(html_to_text(html), "safe");
}

// ---- webfetch tool ----

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
async fn webfetch_http_error_is_tool_error() {
    let (reg, _) = registry();
    let out = fetch(&reg, "http://127.0.0.1:1/nope").await;
    assert!(out.is_error);
    assert!(out.content.contains("blocked"), "{}", out.content);
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

#[tokio::test]
async fn websearch_clamps_limits_and_backend_overdelivery() {
    struct RecordingBackend {
        limits: Arc<Mutex<Vec<usize>>>,
    }
    impl SearchBackend for RecordingBackend {
        fn search(&self, _query: &str, limit: usize) -> ilar::tools::web::ToolFutureSearch {
            self.limits.lock().unwrap().push(limit);
            Box::pin(async {
                Ok(SearchResults {
                    hits: (0..100)
                        .map(|index| SearchHit {
                            title: format!("hit {index}"),
                            url: format!("https://example.com/{index}"),
                            snippet: "snippet".into(),
                        })
                        .collect(),
                })
            })
        }
    }

    let limits = Arc::new(Mutex::new(Vec::new()));
    let tool = WebSearchTool::new(Box::new(RecordingBackend {
        limits: limits.clone(),
    }));
    let low = tool
        .run(
            serde_json::json!({"query": "low", "limit": 0}),
            ToolContext::root(std::env::temp_dir()),
        )
        .await;
    let high = tool
        .run(
            serde_json::json!({"query": "high", "limit": 99999}),
            ToolContext::root(std::env::temp_dir()),
        )
        .await;

    assert_eq!(*limits.lock().unwrap(), vec![1, 20]);
    assert_eq!(low.content.matches("https://example.com/").count(), 1);
    assert_eq!(high.content.matches("https://example.com/").count(), 20);
    assert_eq!(tool.input_schema()["properties"]["limit"]["minimum"], 1);
    assert_eq!(tool.input_schema()["properties"]["limit"]["maximum"], 20);
}

#[tokio::test]
async fn websearch_bounds_queries_and_backend_errors() {
    struct HugeError;
    impl SearchBackend for HugeError {
        fn search(&self, _query: &str, _limit: usize) -> ilar::tools::web::ToolFutureSearch {
            Box::pin(async { anyhow::bail!("{}", "x".repeat(200_000)) })
        }
    }

    let tool = WebSearchTool::new(Box::new(HugeError));
    let query_error = tool
        .run(
            serde_json::json!({"query": "q".repeat(2_000)}),
            ToolContext::root(std::env::temp_dir()),
        )
        .await;
    assert!(query_error.is_error);
    assert!(query_error.content.len() < 2_000);

    let backend_error = tool
        .run(
            serde_json::json!({"query": "safe"}),
            ToolContext::root(std::env::temp_dir()),
        )
        .await;
    assert!(backend_error.is_error);
    assert!(backend_error.content.len() <= 100_000);
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
