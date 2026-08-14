//! webfetch + websearch tools — see meta/issues/web-tools.md.

use serde::Deserialize;

use crate::tools::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput};

const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_TEXT_CHARS: usize = 60_000;

/// Crude but dependency-free HTML → text: drops script/style content,
/// strips tags, decodes common entities, collapses whitespace.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    // Remove script/style blocks wholesale.
    let mut current = rest.to_string();
    for tag in ["script", "style"] {
        let mut cleaned = String::with_capacity(current.len());
        let mut search: &str = &current;
        let close = format!("</{tag}>");
        while let Some(start) = find_case_insensitive(search, &format!("<{tag}")) {
            cleaned.push_str(&search[..start]);
            let after = &search[start..];
            match find_case_insensitive(after, &close) {
                Some(end) => search = &after[end + close.len()..],
                None => {
                    search = after;
                    break;
                }
            }
        }
        cleaned.push_str(search);
        current = cleaned;
    }
    rest = &current;
    let mut in_tag = false;
    for c in rest.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&out);
    collapse_whitespace(&decoded)
}

fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .to_lowercase()
        .find(&needle.to_lowercase())
        .filter(|_| true)
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for c in text.chars() {
        let is_ws = c.is_whitespace();
        if is_ws {
            if !last_space {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        last_space = is_ws;
    }
    out.trim().to_string()
}

/// One search hit.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
}

/// Pluggable search backend (Tavily in production, mocks in tests).
pub trait SearchBackend: Send + Sync {
    fn search(&self, query: &str, limit: usize) -> ToolFutureSearch;
}

/// Alias to keep the boxed future readable.
pub type ToolFutureSearch =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<SearchResults>> + Send>>;

// ---- webfetch ----

pub struct WebFetchTool {
    http: reqwest::Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

#[derive(Deserialize)]
struct FetchInput {
    url: String,
}

impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "webfetch"
    }
    fn description(&self) -> &'static str {
        "Fetch a URL and return its content as plain text (HTML converted)."
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"url": {"type": "string"}},
            "required": ["url"]
        })
    }
    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let http = self.http.clone();
        Box::pin(async move {
            let input: FetchInput = match serde_json::from_value(input) {
                Ok(v) => v,
                Err(e) => return ToolOutput::error(format!("invalid input for webfetch: {e}")),
            };
            match http.get(&input.url).send().await {
                Ok(response) if response.status().is_success() => {
                    let content_type = response
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    match response.bytes().await {
                        Ok(bytes) if bytes.len() <= MAX_FETCH_BYTES => {
                            let body = String::from_utf8_lossy(&bytes);
                            let text = if content_type.contains("html") {
                                html_to_text(&body)
                            } else {
                                body.to_string()
                            };
                            if text.chars().count() > MAX_TEXT_CHARS {
                                ToolOutput::text(format!(
                                    "{}\n\n…(truncated at {MAX_TEXT_CHARS} chars)",
                                    text.chars().take(MAX_TEXT_CHARS).collect::<String>()
                                ))
                            } else {
                                ToolOutput::text(text)
                            }
                        }
                        Ok(bytes) => ToolOutput::error(format!(
                            "webfetch {}: response too large ({} bytes)",
                            input.url,
                            bytes.len()
                        )),
                        Err(e) => ToolOutput::error(format!("webfetch {}: {e}", input.url)),
                    }
                }
                Ok(response) => ToolOutput::error(format!(
                    "webfetch {}: HTTP {}",
                    input.url,
                    response.status()
                )),
                Err(e) => ToolOutput::error(format!("webfetch {}: {e}", input.url)),
            }
        })
    }
}

// ---- websearch ----

pub struct WebSearchTool {
    backend: Box<dyn SearchBackend>,
}

#[derive(Deserialize)]
struct SearchInput {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

impl WebSearchTool {
    pub fn new(backend: Box<dyn SearchBackend>) -> Self {
        Self { backend }
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "websearch"
    }
    fn description(&self) -> &'static str {
        "Search the web. Returns titles, URLs and snippets."
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "description": "Max results (default 5)"}
            },
            "required": ["query"]
        })
    }
    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let backend = &self.backend;
        let input: SearchInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Box::pin(async move {
                    ToolOutput::error(format!("invalid input for websearch: {e}"))
                });
            }
        };
        let limit = input.limit.unwrap_or(5);
        let fut = backend.search(&input.query, limit);
        Box::pin(async move {
            match fut.await {
                Ok(results) if results.hits.is_empty() => {
                    ToolOutput::text(format!("no results for {:?}", input.query))
                }
                Ok(results) => {
                    let lines: Vec<String> = results
                        .hits
                        .iter()
                        .map(|h| format!("{}\n{}\n{}", h.title, h.url, h.snippet))
                        .collect();
                    ToolOutput::text(lines.join("\n\n"))
                }
                Err(e) => ToolOutput::error(format!("websearch: {e}")),
            }
        })
    }
}

/// Tavily backend (ILAR_TAVILY_API_KEY).
pub struct TavilyBackend {
    api_key: String,
    http: reqwest::Client,
}

impl TavilyBackend {
    pub fn from_env() -> Option<Self> {
        std::env::var("ILAR_TAVILY_API_KEY")
            .ok()
            .map(|api_key| Self {
                api_key,
                http: reqwest::Client::new(),
            })
    }
}

impl SearchBackend for TavilyBackend {
    fn search(&self, query: &str, limit: usize) -> ToolFutureSearch {
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        let query = query.to_string();
        Box::pin(async move {
            let body = serde_json::json!({
                "api_key": api_key,
                "query": query,
                "max_results": limit,
            });
            let response = http
                .post("https://api.tavily.com/search")
                .json(&body)
                .send()
                .await?;
            let status = response.status();
            let value: serde_json::Value = response.json().await?;
            if !status.is_success() {
                anyhow::bail!("tavily HTTP {status}");
            }
            let hits = value["results"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|r| SearchHit {
                            title: r["title"].as_str().unwrap_or_default().into(),
                            url: r["url"].as_str().unwrap_or_default().into(),
                            snippet: r["content"].as_str().unwrap_or_default().into(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(SearchResults { hits })
        })
    }
}
