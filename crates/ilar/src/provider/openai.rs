//! OpenAI Responses API provider.

use std::collections::HashMap;
use std::task::{Context, Poll};

use std::panic::AssertUnwindSafe;

use anyhow::Context as _;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::event::{ProviderEvent, StopReason};
use super::request::{Request, ToolDefinition, merge_options, resolve_model};
use super::sse::SseParser;
use super::{EventStream, Provider};
use crate::session::{ChatMessage, ContentBlock, InputTokenAccounting, Role, Usage};

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
        let (provider, model_id) = resolve_model(&req.model)?;
        if provider != "openai" {
            anyhow::bail!("model provider mismatch: expected openai, got {provider}");
        }
        if !req.continuations.is_empty() {
            anyhow::bail!("OpenAI does not support opaque paused continuations");
        }
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
        merge_options(
            &mut body,
            &req.options,
            &["model", "instructions", "input", "tools", "stream"],
        )?;
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
            ContentBlock::Diagnostic { .. } => {}
            ContentBlock::ProviderReplay { .. } => {}
            ContentBlock::Reasoning { item } => {
                flush_text(&mut text, &mut items);
                items.push(item.clone());
            }
            ContentBlock::ToolCall { id, name, input } => {
                flush_text(&mut text, &mut items);
                let input = input
                    .is_object()
                    .then_some(input)
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
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
                is_error: _,
            } => {
                flush_text(&mut text, &mut items);
                items.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": content,
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
    fn provider_prefix(&self) -> Option<&'static str> {
        Some("openai")
    }

    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        let mut body = self.wire_body(&req)?;
        let is_chatgpt = matches!(self.auth, Auth::ChatGpt { .. });
        if is_chatgpt && let Some(object) = body.as_object_mut() {
            // The ChatGPT backend rejects server-side state retention.
            object.insert("store".into(), serde_json::json!(false));
        }
        if let Some(object) = body.as_object_mut()
            && object.get("store") == Some(&serde_json::Value::Bool(false))
        {
            let include = object
                .entry("include")
                .or_insert_with(|| serde_json::json!([]));
            if let Some(include) = include.as_array_mut()
                && !include
                    .iter()
                    .any(|item| item == "reasoning.encrypted_content")
            {
                include.push(serde_json::json!("reasoning.encrypted_content"));
            }
        }

        let url = format!("{}/responses", self.base_url);
        let (token, account, auth_store) = match &self.auth {
            Auth::ApiKey(key) => (key.clone(), None, None),
            Auth::ChatGpt { store } => {
                let tokens = store
                    .load()
                    .context("OpenAI ChatGPT auth store")?
                    .ok_or_else(|| {
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
                            match crate::auth::refresh_tokens(
                                store,
                                &current_token,
                                &token_url,
                                &http,
                            )
                            .await
                            {
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
                                let data = match parser.feed(&bytes) {
                                    Ok(data) => data,
                                    Err(error) => {
                                        let _ =
                                            tx.send(ProviderEvent::Error(error.to_string())).await;
                                        return;
                                    }
                                };
                                for data in data {
                                    let events = match mapper.map(&data) {
                                        Ok(events) => events,
                                        Err(error) => {
                                            let _ = tx.send(ProviderEvent::Error(error)).await;
                                            return;
                                        }
                                    };
                                    for event in events {
                                        let terminal = matches!(
                                            event,
                                            ProviderEvent::TurnComplete { .. }
                                                | ProviderEvent::Error(_)
                                        );
                                        if tx.send(event).await.is_err() {
                                            return; // consumer dropped
                                        }
                                        if terminal {
                                            return;
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
                    if let Err(error) = parser.finish() {
                        let _ = tx.send(ProviderEvent::Error(error.to_string())).await;
                        return;
                    }
                    if let Some(event) = mapper.finish() {
                        let _ = tx.send(event).await;
                    }
                }
                response => {
                    let status = response.status();
                    let body =
                        super::error_body::bounded_error_body(response, &[current_token.as_str()])
                            .await;
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
    completed_inputs: HashMap<String, serde_json::Value>,
    completed_items: std::collections::HashSet<String>,
    /// Calls announced but not completed (arguments.done pending).
    pending: Vec<String>,
    tool_call_seen: bool,
    refusal_seen: bool,
    completed: bool,
}

impl EventMapper {
    fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String> {
        if self.completed {
            return Err("OpenAI event arrived after terminal completion".into());
        }
        let value = serde_json::from_str::<serde_json::Value>(data)
            .map_err(|error| format!("invalid OpenAI event JSON: {error}"))?;
        let kind = required_str(&value, "type", "OpenAI event type")?;
        let events = match kind {
            "response.output_text.delta" => {
                vec![ProviderEvent::TextDelta(
                    required_str(&value, "delta", "OpenAI text delta")?.into(),
                )]
            }
            "response.refusal.delta" => {
                // Surface refusal text as deltas so it isn't lost; the
                // stop reason marks the turn as refused.
                self.refusal_seen = true;
                vec![ProviderEvent::TextDelta(
                    required_str(&value, "delta", "OpenAI refusal delta")?.into(),
                )]
            }
            "response.output_item.added" => {
                let item = value
                    .get("item")
                    .filter(|item| item.is_object())
                    .ok_or_else(|| "missing OpenAI output item".to_string())?;
                let item_type = required_str(item, "type", "OpenAI output item type")?;
                if item_type == "function_call" {
                    let call_id = required_str(item, "call_id", "OpenAI tool call id")?.to_string();
                    let name = required_str(item, "name", "OpenAI tool name")?.to_string();
                    let item_id = required_str(item, "id", "OpenAI tool item id")?.to_string();
                    if self.calls.contains_key(&item_id)
                        || self.calls.values().any(|(id, _)| id == &call_id)
                    {
                        return Err(format!("duplicate OpenAI tool call id {call_id:?}"));
                    }
                    self.calls.insert(item_id, (call_id.clone(), name.clone()));
                    self.pending.push(call_id.clone());
                    self.tool_call_seen = true;
                    vec![ProviderEvent::ToolCallStarted { id: call_id, name }]
                } else {
                    Vec::new()
                }
            }
            "response.output_item.done" => {
                let item = value
                    .get("item")
                    .filter(|item| item.is_object())
                    .ok_or_else(|| "missing OpenAI completed output item".to_string())?;
                let item_type = required_str(item, "type", "OpenAI completed item type")?;
                if item_type == "reasoning" {
                    vec![ProviderEvent::ReasoningItem { item: item.clone() }]
                } else if item_type == "function_call" {
                    let item_id = required_str(item, "id", "OpenAI completed tool item id")?;
                    if !self.completed_items.insert(item_id.into()) {
                        return Err(format!("duplicate completed OpenAI tool item {item_id:?}"));
                    }
                    let call_id = required_str(item, "call_id", "OpenAI completed tool call id")?;
                    let name = required_str(item, "name", "OpenAI completed tool name")?;
                    let arguments =
                        required_str(item, "arguments", "OpenAI completed tool arguments")?;
                    let Some((started_id, started_name)) = self.calls.get(item_id) else {
                        return Err(format!(
                            "completed OpenAI item references unknown tool {item_id:?}"
                        ));
                    };
                    if started_id != call_id || started_name != name {
                        return Err(format!(
                            "completed OpenAI item {item_id:?} contradicts its start"
                        ));
                    }
                    let completed = self.completed_inputs.get(item_id).ok_or_else(|| {
                        format!("OpenAI item {item_id:?} completed before its arguments")
                    })?;
                    let item_input: serde_json::Value =
                        serde_json::from_str(arguments).map_err(|error| {
                            format!("invalid OpenAI completed item arguments: {error}")
                        })?;
                    if &item_input != completed {
                        return Err(format!(
                            "completed OpenAI item {item_id:?} changed its arguments"
                        ));
                    }
                    Vec::new()
                } else {
                    Vec::new()
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = required_str(&value, "item_id", "OpenAI argument item id")?;
                let id = self
                    .calls
                    .get(item_id)
                    .map(|(id, _)| id.clone())
                    .ok_or_else(|| {
                        format!("arguments reference unknown OpenAI item {item_id:?}")
                    })?;
                if !self.pending.iter().any(|pending| pending == &id) {
                    return Err(format!(
                        "arguments arrived after completion for OpenAI tool call {id:?}"
                    ));
                }
                vec![ProviderEvent::ToolCallInputDelta {
                    id,
                    delta: required_str(&value, "delta", "OpenAI argument delta")?.into(),
                }]
            }
            "response.function_call_arguments.done" => {
                let item_id = required_str(&value, "item_id", "OpenAI argument item id")?;
                let (id, name) = self.calls.get(item_id).cloned().ok_or_else(|| {
                    format!("arguments reference unknown OpenAI item {item_id:?}")
                })?;
                if !self.pending.iter().any(|pending| pending == &id) {
                    return Err(format!("duplicate completion for OpenAI tool call {id:?}"));
                }
                self.pending.retain(|p| p != &id);
                let args = required_str(&value, "arguments", "OpenAI completed arguments")?;
                let input: serde_json::Value = serde_json::from_str(args)
                    .map_err(|error| format!("invalid OpenAI tool arguments: {error}"))?;
                if !input.is_object() {
                    return Err("OpenAI tool arguments must be a JSON object".into());
                }
                self.completed_inputs.insert(item_id.into(), input.clone());
                vec![ProviderEvent::ToolCallCompleted { id, name, input }]
            }
            "response.completed" | "response.incomplete" => {
                let response = value
                    .get("response")
                    .and_then(serde_json::Value::as_object)
                    .ok_or_else(|| "missing OpenAI response object".to_string())?;
                if self.refusal_seen && self.tool_call_seen {
                    return Err("OpenAI response combined refusal and tool calls".into());
                }
                if kind == "response.completed" && !self.pending.is_empty() {
                    return Err("OpenAI response completed with unfinished tool calls".into());
                }
                self.completed = true;
                let mut events = Vec::new();
                let mut truncated = false;
                if kind == "response.incomplete" {
                    let reason = response
                        .get("incomplete_details")
                        .and_then(|details| details.get("reason"))
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| "missing OpenAI incomplete reason".to_string())?;
                    if reason != "max_output_tokens" {
                        return Err(format!("unsupported OpenAI incomplete reason {reason:?}"));
                    }
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
                let stop_reason = if truncated {
                    StopReason::MaxTokens
                } else if self.refusal_seen {
                    StopReason::Refusal
                } else if self.tool_call_seen {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                };
                events.push(ProviderEvent::TurnComplete {
                    stop_reason,
                    usage: wire_usage(response.get("usage").unwrap_or(&serde_json::Value::Null)),
                });
                events
            }
            "response.failed" | "error" => {
                self.completed = true; // terminal: don't synthesize a second error
                vec![ProviderEvent::Error(
                    super::error_body::stream_error_message(&value),
                )]
            }
            _ => Vec::new(),
        };
        Ok(events)
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

fn required_str<'a>(
    value: &'a serde_json::Value,
    key: &str,
    label: &str,
) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing or empty {label}"))
}

fn wire_usage(usage: &serde_json::Value) -> Usage {
    let cached = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or_default();
    let input = usage["input_tokens"]
        .as_u64()
        .or_else(|| usage["prompt_tokens"].as_u64())
        .unwrap_or_default();
    Usage {
        input_tokens: input.saturating_sub(cached),
        output_tokens: usage["output_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64())
            .unwrap_or_default(),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
        input_token_accounting: Some(InputTokenAccounting::ExcludesCached),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_input_is_normalized_out_of_openai_input_total() {
        let usage = wire_usage(&serde_json::json!({
            "input_tokens": 1_800,
            "output_tokens": 50,
            "input_tokens_details": {"cached_tokens": 1_500}
        }));
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.cache_read_input_tokens, 1_500);
        assert_eq!(usage.context_tokens(), 1_850);
    }
}
