//! webfetch + websearch tools — see meta/issues/web-tools.md.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde::Deserialize;
use url::{Host, Url};

use crate::tools::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};

const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_TEXT_CHARS: usize = 60_000;
const MAX_FETCH_URL_CHARS: usize = 4_096;
const MAX_FETCH_ERROR_CHARS: usize = 8_192;
const MAX_SEARCH_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_SEARCH_RESULTS: usize = 5;
const MIN_SEARCH_RESULTS: usize = 1;
const MAX_SEARCH_RESULTS: usize = 20;
const MAX_SEARCH_TITLE_CHARS: usize = 500;
const MAX_SEARCH_URL_CHARS: usize = 2_048;
const MAX_SEARCH_SNIPPET_CHARS: usize = 4_000;
const MAX_SEARCH_QUERY_CHARS: usize = 1_000;
const MAX_SEARCH_OUTPUT_CHARS: usize = 100_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 10;
const BLOCK_SEPARATOR: char = '\0';

/// Dependency-free HTML to text conversion with byte-safe tag scanning.
pub fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len() / 2);
    let mut position = 0;
    let mut hidden: Option<String> = None;
    while position < html.len() {
        if let Some(hidden_name) = hidden.as_deref() {
            let Some(end) = find_raw_text_close(&html[position..], hidden_name) else {
                break;
            };
            position += end;
            hidden = None;
            text.push(BLOCK_SEPARATOR);
            continue;
        }
        let Some(relative_start) = html[position..].find('<') else {
            text.push_str(&html[position..]);
            break;
        };
        let start = position + relative_start;
        text.push_str(&html[position..start]);
        if html[start..].starts_with("<!--") {
            let Some(relative_end) = html[start + 4..].find("-->") else {
                break;
            };
            position = start + 4 + relative_end + 3;
            continue;
        }
        let Some(relative_end) = find_tag_end(&html[start + 1..]) else {
            if hidden.is_none() {
                text.push_str(&html[start..]);
            }
            break;
        };
        let end = start + 1 + relative_end;
        let (closing, name) = tag_name(&html[start + 1..end]);
        let name = name.to_ascii_lowercase();
        if !closing && matches!(name.as_str(), "script" | "style") {
            hidden = Some(name);
            text.push(BLOCK_SEPARATOR);
        } else if is_block_tag(&name) {
            text.push(BLOCK_SEPARATOR);
        }
        position = end + 1;
    }
    normalize_text(&decode_entities(&text))
}

fn find_raw_text_close(input: &str, expected: &str) -> Option<usize> {
    for (start, _) in input.match_indices('<') {
        let tag = &input[start + 1..];
        let Some(after_slash) = tag.strip_prefix('/') else {
            continue;
        };
        let Some(name) = after_slash.get(..expected.len()) else {
            continue;
        };
        if !name.eq_ignore_ascii_case(expected) {
            continue;
        }
        let delimiter = after_slash[expected.len()..].chars().next();
        let valid_delimiter = match delimiter {
            Some('>' | '/') => true,
            Some(character) => character.is_ascii_whitespace(),
            None => false,
        };
        if !valid_delimiter {
            continue;
        }
        let end = find_tag_end(tag)?;
        return Some(start + 1 + end + 1);
    }
    None
}

fn find_tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    for (index, character) in tag.char_indices() {
        match (quote, character) {
            (Some(expected), found) if expected == found => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, '>') => return Some(index),
            _ => {}
        }
    }
    None
}

fn tag_name(tag: &str) -> (bool, &str) {
    let tag = tag.trim_start();
    let (closing, tag) = match tag.strip_prefix('/') {
        Some(tag) => (true, tag.trim_start()),
        None => (false, tag),
    };
    let end = tag
        .find(|character: char| {
            !character.is_ascii_alphanumeric() && character != '-' && character != ':'
        })
        .unwrap_or(tag.len());
    (closing, &tag[..end])
}

fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "br"
            | "dd"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "ul"
    )
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

fn normalize_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut pending_space = false;
    let mut pending_break = false;
    for character in text.chars() {
        if character == BLOCK_SEPARATOR {
            pending_break = !output.is_empty();
            pending_space = false;
        } else if character.is_whitespace() {
            pending_space = !output.is_empty();
        } else {
            if pending_break {
                if !output.ends_with('\n') {
                    output.push('\n');
                }
            } else if pending_space && !output.ends_with('\n') {
                output.push(' ');
            }
            output.push(character);
            pending_space = false;
            pending_break = false;
        }
    }
    output
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

#[derive(Debug)]
struct PublicDnsResolver;

impl Resolve for PublicDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let addresses = addresses.collect::<Vec<_>>();
            if addresses.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no addresses found for {host}"),
                )
                .into());
            }
            if addresses.iter().any(|address| is_blocked_ip(address.ip())) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("blocked private address for {host}"),
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

fn http_client(timeout: Duration, follow_redirects: bool) -> reqwest::Client {
    let redirect = if follow_redirects {
        reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            match validate_url(attempt.url(), false) {
                Ok(()) => attempt.follow(),
                Err(error) => attempt.error(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    error.to_string(),
                )),
            }
        })
    } else {
        reqwest::redirect::Policy::none()
    };
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout)
        .redirect(redirect)
        .dns_resolver(Arc::new(PublicDnsResolver))
        .no_proxy()
        .build()
        .expect("valid web HTTP client")
}

fn parse_url(input: &str, allow_private: bool) -> anyhow::Result<Url> {
    let url = Url::parse(input)?;
    validate_url(&url, allow_private)?;
    Ok(url)
}

fn validate_url(url: &Url, allow_private: bool) -> anyhow::Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("only http and https URLs are allowed");
    }
    let host = url
        .host()
        .ok_or_else(|| anyhow::anyhow!("URL must contain a host"))?;
    if allow_private {
        return Ok(());
    }
    match host {
        Host::Domain(domain)
            if domain.eq_ignore_ascii_case("localhost")
                || domain.to_ascii_lowercase().ends_with(".localhost") =>
        {
            anyhow::bail!("blocked private URL target")
        }
        Host::Ipv4(address) if is_blocked_ip(IpAddr::V4(address)) => {
            anyhow::bail!("blocked private URL target")
        }
        Host::Ipv6(address) if is_blocked_ip(IpAddr::V6(address)) => {
            anyhow::bail!("blocked private URL target")
        }
        _ => Ok(()),
    }
}

fn is_blocked_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_blocked_ipv4(address),
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4() {
                return is_blocked_ipv4(mapped);
            }
            let segments = address.segments();
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || (segments[0] == 0x0064
                    && segments[1] == 0xff9b
                    && segments[2..6] == [0, 0, 0, 0])
                || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 1)
                || segments[..6] == [0, 0, 0, 0, 0xffff, 0]
                || segments[0] == 0x2002
                || (segments[0] == 0x2001 && segments[1] == 0)
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || segments[0] & 0xffc0 == 0xfec0
                || segments[5] == 0x5efe
        }
    }
}

fn is_blocked_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_multicast()
        || address == Ipv4Addr::BROADCAST
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 240
}

#[derive(Debug, thiserror::Error)]
enum BodyReadError {
    #[error("response too large (limit {0} bytes)")]
    TooLarge(usize),
    #[error("response body failed")]
    Transport(#[source] reqwest::Error),
}

async fn bounded_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, BodyReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(BodyReadError::TooLarge(limit));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(BodyReadError::Transport)?;
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(BodyReadError::TooLarge(limit));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

// ---- webfetch ----

pub struct WebFetchTool {
    http: reqwest::Client,
    allow_private_initial: bool,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self {
            http: http_client(REQUEST_TIMEOUT, true),
            allow_private_initial: false,
        }
    }
}

impl WebFetchTool {
    #[cfg(test)]
    fn for_test(timeout: Duration) -> Self {
        Self {
            http: http_client(timeout, true),
            allow_private_initial: true,
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
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"url": {"type": "string", "maxLength": MAX_FETCH_URL_CHARS}},
            "required": ["url"]
        })
    }
    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let http = self.http.clone();
        let allow_private_initial = self.allow_private_initial;
        Box::pin(async move {
            let input: FetchInput = match serde_json::from_value(input) {
                Ok(v) => v,
                Err(e) => return ToolOutput::error(format!("invalid input for webfetch: {e}")),
            };
            if input.url.chars().count() > MAX_FETCH_URL_CHARS {
                return ToolOutput::error(format!(
                    "webfetch URL exceeds {MAX_FETCH_URL_CHARS} characters"
                ));
            }
            let url = match parse_url(&input.url, allow_private_initial) {
                Ok(url) => url,
                Err(error) => {
                    return ToolOutput::error(bounded_format(
                        format_args!("webfetch invalid URL: {error}"),
                        MAX_FETCH_ERROR_CHARS,
                    ));
                }
            };
            let display_url = redacted_url(&url);
            match http.get(url).send().await {
                Ok(response) if response.status().is_success() => {
                    let content_type = response
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    match bounded_body(response, MAX_FETCH_BYTES).await {
                        Ok(bytes) => {
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
                        Err(BodyReadError::Transport(error)) => ToolOutput::error(bounded_format(
                            format_args!(
                                "webfetch {display_url}: {}",
                                safe_reqwest_error(error, MAX_FETCH_ERROR_CHARS)
                            ),
                            MAX_FETCH_ERROR_CHARS,
                        )),
                        Err(error) => ToolOutput::error(bounded_format(
                            format_args!("webfetch {display_url}: {error}"),
                            MAX_FETCH_ERROR_CHARS,
                        )),
                    }
                }
                Ok(response) => ToolOutput::error(bounded_format(
                    format_args!("webfetch {display_url}: HTTP {}", response.status()),
                    MAX_FETCH_ERROR_CHARS,
                )),
                Err(error) => ToolOutput::error(bounded_format(
                    format_args!(
                        "webfetch {display_url}: {}",
                        safe_reqwest_error(error, MAX_FETCH_ERROR_CHARS)
                    ),
                    MAX_FETCH_ERROR_CHARS,
                )),
            }
        })
    }
}

// ---- websearch ----

pub struct WebSearchTool {
    backend: Box<dyn SearchBackend>,
    timeout: Duration,
}

#[derive(Deserialize)]
struct SearchInput {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

impl WebSearchTool {
    pub fn new(backend: Box<dyn SearchBackend>) -> Self {
        Self {
            backend,
            timeout: REQUEST_TIMEOUT,
        }
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "websearch"
    }
    fn description(&self) -> &'static str {
        "Search the web. Returns titles, URLs and snippets."
    }
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {
                    "type": "integer",
                    "description": "Max results (default 5, range 1-20)",
                    "default": DEFAULT_SEARCH_RESULTS,
                    "minimum": MIN_SEARCH_RESULTS,
                    "maximum": MAX_SEARCH_RESULTS
                }
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
        if input.query.trim().is_empty() {
            return Box::pin(async { ToolOutput::error("websearch query must not be empty") });
        }
        if input.query.chars().count() > MAX_SEARCH_QUERY_CHARS {
            return Box::pin(async {
                ToolOutput::error(format!(
                    "websearch query exceeds {MAX_SEARCH_QUERY_CHARS} characters"
                ))
            });
        }
        let limit = input
            .limit
            .unwrap_or(DEFAULT_SEARCH_RESULTS)
            .clamp(MIN_SEARCH_RESULTS, MAX_SEARCH_RESULTS);
        let fut = backend.search(&input.query, limit);
        let timeout = self.timeout;
        Box::pin(async move {
            match tokio::time::timeout(timeout, fut).await {
                Err(_) => ToolOutput::error("websearch timed out"),
                Ok(Ok(results)) if results.hits.is_empty() => ToolOutput::text(truncate_chars(
                    &format!("no results for {:?}", input.query),
                    MAX_SEARCH_OUTPUT_CHARS,
                )),
                Ok(Ok(results)) => {
                    let lines: Vec<String> = results
                        .hits
                        .into_iter()
                        .take(limit)
                        .map(|hit| {
                            format!(
                                "{}\n{}\n{}",
                                truncate_chars(&hit.title, MAX_SEARCH_TITLE_CHARS),
                                truncate_chars(&hit.url, MAX_SEARCH_URL_CHARS),
                                truncate_chars(&hit.snippet, MAX_SEARCH_SNIPPET_CHARS)
                            )
                        })
                        .collect();
                    ToolOutput::text(truncate_chars(&lines.join("\n\n"), MAX_SEARCH_OUTPUT_CHARS))
                }
                Ok(Err(error)) => ToolOutput::error(bounded_format(
                    format_args!("websearch: {error}"),
                    MAX_SEARCH_OUTPUT_CHARS,
                )),
            }
        })
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

struct LimitedWriter {
    output: String,
    remaining: usize,
}

impl std::fmt::Write for LimitedWriter {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let mut characters = value.chars();
        while self.remaining > 0 {
            let Some(character) = characters.next() else {
                return Ok(());
            };
            self.output.push(character);
            self.remaining -= 1;
        }
        characters
            .next()
            .is_none()
            .then_some(())
            .ok_or(std::fmt::Error)
    }
}

fn bounded_format(arguments: std::fmt::Arguments<'_>, limit: usize) -> String {
    let mut writer = LimitedWriter {
        output: String::new(),
        remaining: limit,
    };
    let _ = std::fmt::write(&mut writer, arguments);
    writer.output
}

fn error_chain(error: &dyn std::error::Error, limit: usize) -> String {
    let mut writer = LimitedWriter {
        output: String::new(),
        remaining: limit,
    };
    let _ = std::fmt::write(&mut writer, format_args!("{error}"));
    let mut source = error.source();
    while let Some(error) = source.filter(|_| writer.remaining > 0) {
        let _ = std::fmt::write(&mut writer, format_args!(": {error}"));
        source = error.source();
    }
    writer.output
}

fn safe_reqwest_error(error: reqwest::Error, limit: usize) -> String {
    let error = error.without_url();
    error_chain(&error, limit)
}

fn redacted_url(url: &Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    truncate_chars(url.as_str(), MAX_FETCH_URL_CHARS)
}

/// Tavily backend (ILAR_TAVILY_API_KEY).
pub struct TavilyBackend {
    api_key: String,
    http: reqwest::Client,
    endpoint: Url,
    allow_private_initial: bool,
}

impl TavilyBackend {
    pub fn from_env() -> Option<Self> {
        std::env::var("ILAR_TAVILY_API_KEY")
            .ok()
            .filter(|api_key| !api_key.trim().is_empty())
            .map(|api_key| Self {
                api_key,
                http: http_client(REQUEST_TIMEOUT, false),
                endpoint: Url::parse("https://api.tavily.com/search").expect("valid Tavily URL"),
                allow_private_initial: false,
            })
    }

    #[cfg(test)]
    fn for_test(api_key: impl Into<String>, endpoint: impl AsRef<str>, timeout: Duration) -> Self {
        Self {
            api_key: api_key.into(),
            http: http_client(timeout, false),
            endpoint: Url::parse(endpoint.as_ref()).expect("valid test Tavily URL"),
            allow_private_initial: true,
        }
    }
}

#[derive(Deserialize)]
struct TavilyResponse {
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    content: String,
}

impl SearchBackend for TavilyBackend {
    fn search(&self, query: &str, limit: usize) -> ToolFutureSearch {
        if query.trim().is_empty() || query.chars().count() > MAX_SEARCH_QUERY_CHARS {
            return Box::pin(async { anyhow::bail!("invalid Tavily query length") });
        }
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        let query = query.to_string();
        let endpoint = self.endpoint.clone();
        let allow_private_initial = self.allow_private_initial;
        let limit = limit.clamp(MIN_SEARCH_RESULTS, MAX_SEARCH_RESULTS);
        Box::pin(async move {
            let body = serde_json::json!({
                "api_key": api_key,
                "query": query,
                "max_results": limit,
            });
            validate_url(&endpoint, allow_private_initial)?;
            let response = http.post(endpoint).json(&body).send().await?;
            let status = response.status();
            if !status.is_success() {
                anyhow::bail!("tavily HTTP {status}");
            }
            let bytes = bounded_body(response, MAX_SEARCH_RESPONSE_BYTES).await?;
            let value: TavilyResponse = serde_json::from_slice(&bytes)?;
            let hits = value
                .results
                .into_iter()
                .take(limit.min(MAX_SEARCH_RESULTS))
                .map(|result| SearchHit {
                    title: result.title,
                    url: result.url,
                    snippet: result.content,
                })
                .collect();
            Ok(SearchResults { hits })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn spawn_server(
        headers: String,
        body: Vec<u8>,
        header_delay: Duration,
        body_delay: Duration,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 8_192];
            let _ = socket.read(&mut request).await;
            tokio::time::sleep(header_delay).await;
            if socket.write_all(headers.as_bytes()).await.is_err() {
                return;
            }
            tokio::time::sleep(body_delay).await;
            let _ = socket.write_all(&body).await;
        });
        format!("http://{address}/test")
    }

    async fn fetch(tool: &WebFetchTool, url: &str) -> ToolOutput {
        tool.run(
            serde_json::json!({"url": url}),
            ToolContext::root(std::env::temp_dir()),
        )
        .await
    }

    async fn response_url(content_type: &str, body: &[u8]) -> String {
        spawn_server(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ),
            body.to_vec(),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await
    }

    #[tokio::test]
    async fn fetch_converts_html_and_preserves_plain_text() {
        let html = b"<html><body><h1>Hello</h1><p>World of <a href='#'>links</a></p></body></html>";
        let html_url = response_url("text/html; charset=utf-8", html).await;
        let html = fetch(&WebFetchTool::for_test(Duration::from_secs(2)), &html_url).await;
        assert!(!html.is_error, "{}", html.content);
        assert_eq!(html.content, "Hello\nWorld of links");

        let plain = b"just plain text #hashtag 100% intact";
        let plain_url = response_url("text/plain", plain).await;
        let plain = fetch(&WebFetchTool::for_test(Duration::from_secs(2)), &plain_url).await;
        assert_eq!(plain.content, "just plain text #hashtag 100% intact");
    }

    #[tokio::test]
    async fn fetch_rejects_declared_and_streamed_oversize_bodies() {
        let declared = spawn_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\n\r\n".into(),
            Vec::new(),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;
        let output = fetch(&WebFetchTool::for_test(Duration::from_secs(2)), &declared).await;
        assert!(output.is_error && output.content.contains("too large"));

        let payload = vec![b'x'; MAX_FETCH_BYTES + 1];
        let mut chunked = format!("{:x}\r\n", payload.len()).into_bytes();
        chunked.extend(payload);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        let streamed = spawn_server(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".into(),
            chunked,
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;
        let output = fetch(&WebFetchTool::for_test(Duration::from_secs(2)), &streamed).await;
        assert!(output.is_error && output.content.contains("too large"));
    }

    #[tokio::test]
    async fn fetch_total_timeout_covers_headers_and_body() {
        for (header_delay, body_delay) in [
            (Duration::from_millis(250), Duration::ZERO),
            (Duration::ZERO, Duration::from_millis(250)),
        ] {
            let url = spawn_server(
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n".into(),
                b"ok".to_vec(),
                header_delay,
                body_delay,
            )
            .await;
            let started = std::time::Instant::now();
            let output = fetch(&WebFetchTool::for_test(Duration::from_millis(50)), &url).await;
            assert!(output.is_error);
            assert!(
                output.content.to_ascii_lowercase().contains("timed out"),
                "{}",
                output.content
            );
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }

    #[tokio::test]
    async fn fetch_errors_do_not_persist_secret_url_components() {
        let url = spawn_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n".into(),
            b"ok".to_vec(),
            Duration::ZERO,
            Duration::from_millis(250),
        )
        .await
        .replace("/test", "/path-secret?token=query-secret");
        let output = fetch(&WebFetchTool::for_test(Duration::from_millis(50)), &url).await;
        assert!(output.is_error);
        assert!(
            !output.content.contains("path-secret"),
            "{}",
            output.content
        );
        assert!(
            !output.content.contains("query-secret"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn fetch_blocks_private_redirect_destination() {
        let url = spawn_server(
            "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data\r\nContent-Length: 0\r\n\r\n".into(),
            Vec::new(),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;
        let output = fetch(&WebFetchTool::for_test(Duration::from_secs(2)), &url).await;
        assert!(output.is_error);
        assert!(output.content.contains("blocked"), "{}", output.content);
    }

    #[test]
    fn private_and_metadata_targets_are_rejected() {
        for url in [
            "http://localhost/",
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://169.254.169.254/",
            "http://[::1]/",
            "http://[fd00:ec2::254]/",
            "http://[64:ff9b::a9fe:a9fe]/",
            "http://[64:ff9b:1::a9fe:a9fe]/",
            "http://[::ffff:0:a9fe:a9fe]/",
            "http://[2002:a9fe:a9fe::1]/",
            "file:///etc/passwd",
        ] {
            assert!(parse_url(url, false).is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn tavily_rejects_oversized_json() {
        let url = spawn_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 99999999\r\n\r\n"
                .into(),
            Vec::new(),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;
        let backend = TavilyBackend::for_test("key", url, Duration::from_secs(2));
        let error = backend.search("query", 5).await.unwrap_err();
        assert!(error.to_string().contains("too large"), "{error:#}");
    }

    #[tokio::test]
    async fn tavily_does_not_follow_redirects_with_credentials() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target.local_addr().unwrap();
        let redirect = spawn_server(
            format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target_address}/steal\r\nContent-Length: 0\r\n\r\n"
            ),
            Vec::new(),
            Duration::ZERO,
            Duration::ZERO,
        )
        .await;
        let backend = TavilyBackend::for_test("secret", redirect, Duration::from_secs(2));
        let error = backend.search("query", 5).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 307"), "{error:#}");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), target.accept())
                .await
                .is_err(),
            "credential-bearing redirect reached its target"
        );
    }

    #[tokio::test]
    async fn search_backend_has_a_total_timeout() {
        struct Pending;
        impl SearchBackend for Pending {
            fn search(&self, _query: &str, _limit: usize) -> ToolFutureSearch {
                Box::pin(std::future::pending())
            }
        }
        let tool = WebSearchTool {
            backend: Box::new(Pending),
            timeout: Duration::from_millis(50),
        };
        let output = tool
            .run(
                serde_json::json!({"query": "query"}),
                ToolContext::root(std::env::temp_dir()),
            )
            .await;
        assert!(output.is_error);
        assert!(output.content.contains("timed out"));
    }
}
