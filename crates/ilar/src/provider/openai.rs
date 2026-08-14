//! OpenAI Responses API provider.

use std::collections::HashMap;
use std::task::{Context, Poll};

use std::panic::AssertUnwindSafe;

use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::event::{ProviderEvent, StopReason};
use super::request::{Request, ToolDefinition, resolve_model};
use super::sse::SseParser;
use super::{EventStream, Provider};
use crate::session::{ChatMessage, ContentBlock, Role, Usage};

#[derive(Clone)]
enum Auth {
    ApiKey(String),
    /// ChatGPT OAuth (Codex-style): bearer from the token store, one
    /// refresh-and-retry on 401.
    ChatGpt {
        store: crate::auth::AuthStore,
    },
}

#[derive(Clone)]
pub struct OpenAIProvider {
    auth: Auth,
    base_url: String,
    token_url: String,
    http: reqwest::Client,
}

impl OpenAIProvider {
    /// `base_url` overrides the default `https://api.openai.com/v1`
    /// (proxies, gateways).
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            auth: Auth::ApiKey(api_key),
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
            token_url: format!("{}/oauth/token", crate::auth::AUTH_BASE),
            http: reqwest::Client::new(),
        }
    }

    /// ChatGPT-account auth: Responses API through the ChatGPT backend.
    pub fn with_chatgpt_auth(store: crate::auth::AuthStore, base_url: Option<String>) -> Self {
        Self {
            auth: Auth::ChatGpt { store },
            base_url: base_url.unwrap_or_else(|| "https://chatgpt.com/backend-api/codex".into()),
            token_url: format!("{}/oauth/token", crate::auth::AUTH_BASE),
            http: reqwest::Client::new(),
        }
    }

    /// Test hook: point the refresh endpoint at a mock server.
    pub fn with_token_url_for_test(mut self, url: impl Into<String>) -> Self {
        self.token_url = url.into();
        self
    }

    fn wire_body(&self, req: &Request) -> anyhow::Result<serde_json::Value> {
        let (_provider, model_id) = resolve_model(&req.model)?;
        let input = req
            .messages
            .iter()
            .flat_map(wire_input_items)
            .collect::<Vec<_>>();
        let mut body = serde_json::Map::new();
        body.insert("model".into(), serde_json::json!(model_id));
        body.insert("instructions".into(), serde_json::json!(req.system_prompt));
        body.insert("input".into(), serde_json::json!(input));
        body.insert(
            "tools".into(),
            serde_json::json!(req.tools.iter().map(wire_tool).collect::<Vec<_>>()),
        );
        body.insert("stream".into(), serde_json::json!(true));
        // Passthrough options (temperature etc.) merged at top level; a
        // user-specified key overrides the default.
        if let Some(options) = req.options.as_object() {
            body.extend(options.clone());
        }
        Ok(serde_json::Value::Object(body))
    }
}

/// One neutral message may map to zero (dropped thinking) or more wire items.
fn wire_input_items(msg: &ChatMessage) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    // Group tool results with their role; text becomes message items.
    let mut text = String::new();
    let flush_text = |text: &mut String, items: &mut Vec<serde_json::Value>| {
        if !text.is_empty() {
            items.push(serde_json::json!({
                "role": match msg.role { Role::User => "user", Role::Assistant => "assistant" },
                "content": std::mem::take(text),
            }));
        }
    };
    for block in &msg.content {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::Thinking { .. } => {} // reasoning items are server-managed
            ContentBlock::ToolCall { id, name, input } => {
                flush_text(&mut text, &mut items);
                items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": input.to_string(),
                }));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                flush_text(&mut text, &mut items);
                items.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": content,
                    "is_error": *is_error,
                }));
            }
        }
    }
    flush_text(&mut text, &mut items);
    items
}

fn wire_tool(tool: &ToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.input_schema,
    })
}

impl Provider for OpenAIProvider {
    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        let mut body = self.wire_body(&req)?;
        let is_chatgpt = matches!(self.auth, Auth::ChatGpt { .. });
        if is_chatgpt && let Some(object) = body.as_object_mut() {
            // The ChatGPT backend rejects server-side state retention.
            object.insert("store".into(), serde_json::json!(false));
        }

        let url = format!("{}/responses", self.base_url);
        let (token, account, auth_store) = match &self.auth {
            Auth::ApiKey(key) => (key.clone(), None, None),
            Auth::ChatGpt { store } => {
                let tokens = store.load().ok_or_else(|| {
                    anyhow::anyhow!("OpenAI ChatGPT auth: not logged in — run `ilar login`")
                })?;
                (tokens.access_token, tokens.account_id, Some(store.clone()))
            }
        };

        let (tx, rx) = mpsc::channel(64);
        let http = self.http.clone();
        let tx_panic = tx.clone();
        let token_url = self.token_url.clone();
        let pump = async move {
            let mut mapper = EventMapper::default();
            let mut parser = SseParser::new();
            let mut current_token = token;
            let mut current_account = account;
            let mut refreshed = false;

            let response = loop {
                let mut builder = http
                    .post(&url)
                    .bearer_auth(&current_token)
                    .json(&body)
                    .timeout(std::time::Duration::from_secs(600));
                if is_chatgpt {
                    builder = builder
                        .header("originator", "codex_cli_rs")
                        .header("OpenAI-Beta", "responses=experimental");
                    if let Some(account) = &current_account {
                        builder = builder.header("chatgpt-account-id", account);
                    }
                }
                let request = match builder.build() {
                    Ok(request) => request,
                    Err(e) => {
                        let _ = tx.send(ProviderEvent::Error(e.to_string())).await;
                        return;
                    }
                };
                match http.execute(request).await {
                    Ok(response) => {
                        if response.status() == reqwest::StatusCode::UNAUTHORIZED
                            && is_chatgpt
                            && !refreshed
                            && let Some(store) = &auth_store
                        {
                            refreshed = true;
                            match crate::auth::refresh_tokens(store, &token_url, &http).await {
                                Ok(tokens) => {
                                    current_token = tokens.access_token;
                                    if tokens.account_id.is_some() {
                                        current_account = tokens.account_id;
                                    }
                                    continue;
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(ProviderEvent::Error(format!(
                                            "token refresh failed: {e:#}"
                                        )))
                                        .await;
                                    return;
                                }
                            }
                        }
                        break response;
                    }
                    Err(e) => {
                        let _ = tx.send(ProviderEvent::Error(e.to_string())).await;
                        return;
                    }
                }
            };
            match response {
                response if response.status().is_success() => {
                    let mut stream = response.bytes_stream();
                    while let Some(chunk) = stream.next().await {
                        match chunk {
                            Ok(bytes) => {
                                for data in parser.feed(&bytes) {
                                    for event in mapper.map(&data) {
                                        if tx.send(event).await.is_err() {
                                            return; // consumer dropped
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(ProviderEvent::Error(e.to_string())).await;
                                return;
                            }
                        }
                    }
                    if let Some(event) = mapper.finish() {
                        let _ = tx.send(event).await;
                    }
                }
                response => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    let _ = tx
                        .send(ProviderEvent::Error(format!("HTTP {status}: {body}")))
                        .await;
                }
            }
        };
        let handle = tokio::spawn(async move {
            // A pump panic must surface as an Error event, not a silent
            // clean stream end (contract: TurnComplete or Error terminal).
            if let Err(panic) = AssertUnwindSafe(pump).catch_unwind().await {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "provider pump panicked".into());
                let _ = tx_panic
                    .send(ProviderEvent::Error(format!("internal error: {message}")))
                    .await;
            }
        });

        Ok(Box::pin(AbortOnDropStream {
            stream: ReceiverStream::new(rx),
            handle: Some(handle),
        }))
    }
}

/// Stream wrapper that aborts the pump task when dropped — the blessed
/// cancellation pattern from the provider docs.
struct AbortOnDropStream<S> {
    stream: S,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl<S> Drop for AbortOnDropStream<S> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl<S: Stream + Unpin> Stream for AbortOnDropStream<S> {
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}

/// Maps Responses API data payloads to neutral events.
#[derive(Default)]
struct EventMapper {
    /// item_id (fc_...) -> (call_id, name): deltas carry item_id,
    /// neutral tool events carry call_id.
    calls: HashMap<String, (String, String)>,
    /// Calls announced but not completed (arguments.done pending).
    pending: Vec<String>,
    tool_call_seen: bool,
    refusal_seen: bool,
    completed: bool,
}

impl EventMapper {
    fn map(&mut self, data: &str) -> Vec<ProviderEvent> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            return Vec::new();
        };
        let kind = value["type"].as_str().unwrap_or_default();
        match kind {
            "response.output_text.delta" => {
                vec![ProviderEvent::TextDelta(
                    value["delta"].as_str().unwrap_or_default().into(),
                )]
            }
            "response.refusal.delta" => {
                // Surface refusal text as deltas so it isn't lost; the
                // stop reason marks the turn as refused.
                self.refusal_seen = true;
                vec![ProviderEvent::TextDelta(
                    value["delta"].as_str().unwrap_or_default().into(),
                )]
            }
            "response.output_item.added" => {
                let item = &value["item"];
                if item["type"] == "function_call" {
                    let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    let item_id = item["id"].as_str().unwrap_or_default().to_string();
                    self.calls.insert(item_id, (call_id.clone(), name.clone()));
                    self.pending.push(call_id.clone());
                    self.tool_call_seen = true;
                    vec![ProviderEvent::ToolCallStarted { id: call_id, name }]
                } else {
                    Vec::new()
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = value["item_id"].as_str().unwrap_or_default();
                let id = self
                    .calls
                    .get(item_id)
                    .map(|(id, _)| id.clone())
                    .unwrap_or_else(|| item_id.into());
                vec![ProviderEvent::ToolCallInputDelta {
                    id,
                    delta: value["delta"].as_str().unwrap_or_default().into(),
                }]
            }
            "response.function_call_arguments.done" => {
                let item_id = value["item_id"].as_str().unwrap_or_default();
                let (id, name) = self
                    .calls
                    .get(item_id)
                    .cloned()
                    .unwrap_or_else(|| (item_id.into(), String::new()));
                self.pending.retain(|p| p != &id);
                let args = value["arguments"].as_str().unwrap_or_default();
                let input = serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
                vec![ProviderEvent::ToolCallCompleted { id, name, input }]
            }
            "response.completed" | "response.incomplete" => {
                self.completed = true;
                let mut events = Vec::new();
                let mut truncated = false;
                if kind == "response.incomplete" {
                    // Truncated mid-arguments: synthesize null-input
                    // completions so every Started call is Completed
                    // (event contract).
                    truncated = true;
                    let pending = std::mem::take(&mut self.pending);
                    for id in pending {
                        let name = self
                            .calls
                            .values()
                            .find(|(cid, _)| cid == &id)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_default();
                        events.push(ProviderEvent::ToolCallCompleted {
                            id,
                            name,
                            input: serde_json::Value::Null,
                        });
                    }
                }
                let stop_reason = if self.refusal_seen {
                    StopReason::Refusal
                } else if truncated {
                    StopReason::MaxTokens
                } else if self.tool_call_seen {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                };
                events.push(ProviderEvent::TurnComplete {
                    stop_reason,
                    usage: wire_usage(&value["response"]["usage"]),
                });
                events
            }
            "response.failed" | "error" => {
                self.completed = true; // terminal: don't synthesize a second error
                let message = value["response"]["error"]["message"]
                    .as_str()
                    .or_else(|| value["message"].as_str())
                    .unwrap_or("unknown provider error");
                vec![ProviderEvent::Error(message.into())]
            }
            _ => Vec::new(),
        }
    }

    /// Stream ended without TurnComplete/Error: synthesize an error rather
    /// than letting the consumer hang.
    fn finish(&mut self) -> Option<ProviderEvent> {
        if self.completed {
            None
        } else {
            Some(ProviderEvent::Error(
                "stream ended before completion".into(),
            ))
        }
    }
}

fn wire_usage(usage: &serde_json::Value) -> Usage {
    Usage {
        input_tokens: usage["input_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens"].as_u64())
            .unwrap_or_default(),
        output_tokens: usage["output_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64())
            .unwrap_or_default(),
        cache_read_input_tokens: usage["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or_default(),
        cache_creation_input_tokens: 0,
    }
}
