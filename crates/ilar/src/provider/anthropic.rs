//! Direct Anthropic Messages API.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::request::{merge_options, resolve_model};
use super::transport::{self, Credential, EventMapper, TransportResponse};
use super::{EventStream, Provider, ProviderEvent, Request, StopReason};
use crate::session::{ContentBlock, ImageContent, InputTokenAccounting, Role, Usage};

pub(crate) const PREFIX: &str = "anthropic";
pub(crate) const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const BASE_URL: &str = "https://api.anthropic.com/v1";
const API_VERSION: &str = "2026-10-21";
const REPLAY_ITEM: &str = "anthropic_thinking";

pub(crate) fn validate_base_url(base: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(base).context("invalid Anthropic base URL")?;
    ensure!(
        url.has_host()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "invalid Anthropic base URL"
    );
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => host == "localhost",
        None => false,
    };
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "Anthropic base URL must use HTTPS except on loopback"
    );
    Ok(url)
}

pub(crate) fn is_replay_item(item: &Value) -> bool {
    item["type"].as_str() == Some(REPLAY_ITEM)
}

type Identity = [u8; 32];

/// Bounded, in-memory recovery shared by clients built from one config.
/// Keep only fingerprints, never the opaque thinking payloads themselves.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReplayRecovery(Arc<Mutex<VecDeque<(Identity, Identity)>>>);

impl ReplayRecovery {
    fn rejected(&self, scope: Identity) -> HashSet<Identity> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(s, id)| (*s == scope).then_some(*id))
            .collect()
    }

    fn remember(&self, scope: Identity, ids: impl IntoIterator<Item = Identity>) {
        const MAX_REJECTED: usize = 4096;
        let mut entries = self.0.lock().unwrap();
        for id in ids {
            let entry = (scope, id);
            if !entries.contains(&entry) {
                if entries.len() == MAX_REJECTED {
                    entries.pop_front();
                }
                entries.push_back(entry);
            }
        }
    }
}

fn identity(parts: &[&str]) -> Identity {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.len().to_le_bytes());
        digest.update(part.as_bytes());
    }
    digest.finalize().into()
}

fn thinking_identity(part: &Value) -> Option<Identity> {
    let kind = part["type"].as_str()?;
    let key = match kind {
        "thinking" => "signature",
        "redacted_thinking" => "data",
        _ => return None,
    };
    Some(identity(&[kind, part[key].as_str()?]))
}

#[derive(Clone)]
pub struct AnthropicProvider {
    key: String,
    base_url: String,
    client: reqwest::Client,
    recovery: ReplayRecovery,
}

impl AnthropicProvider {
    pub fn new(key: String, base_url: Option<String>) -> Self {
        let base_url = base_url.unwrap_or_else(|| BASE_URL.into());
        let mut builder = reqwest::Client::builder()
            .connect_timeout(transport::CONNECT_TIMEOUT)
            .read_timeout(transport::IDLE_TIMEOUT)
            // x-api-key is not Authorization; never follow a redirect with it.
            .redirect(reqwest::redirect::Policy::none());
        if reqwest::Url::parse(&base_url).is_ok_and(|url| url.scheme() == "http") {
            // Keep cleartext test requests on this machine, regardless of
            // proxy configuration or local DNS overrides.
            builder = builder.no_proxy().resolve_to_addrs(
                "localhost",
                &[
                    (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                    (std::net::Ipv6Addr::LOCALHOST, 0).into(),
                ],
            );
        }
        let client = builder.build().expect("valid Anthropic HTTP client");
        Self {
            key,
            base_url,
            client,
            recovery: ReplayRecovery::default(),
        }
    }

    pub(crate) fn with_recovery(mut self, recovery: ReplayRecovery) -> Self {
        self.recovery = recovery;
        self
    }

    fn endpoint(&self) -> anyhow::Result<reqwest::Url> {
        let mut url = validate_base_url(&self.base_url)?;
        url.set_path(&format!("{}/messages", url.path().trim_end_matches('/')));
        Ok(url)
    }
}

impl Provider for AnthropicProvider {
    fn provider_prefix(&self) -> Option<&'static str> {
        Some(PREFIX)
    }

    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        let (prefix, model) = resolve_model(&req.model)?;
        ensure!(prefix == PREFIX, "expected {PREFIX} model");
        ensure!(!self.key.trim().is_empty(), "missing {API_KEY_ENV}");
        let mut header = reqwest::header::HeaderValue::from_str(&self.key)
            .context("invalid Anthropic API key header")?;
        header.set_sensitive(true);
        let endpoint = self.endpoint()?;
        let scope = req
            .cache_key
            .as_deref()
            .map(|key| identity(&[endpoint.as_str(), model, key]));
        let request = self
            .client
            .post(endpoint)
            .header("x-api-key", header)
            .header("anthropic-version", API_VERSION)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        let mut body = request_body(&req, model)?;
        let recovery = self.recovery.clone();
        if let Some(scope) = scope {
            let rejected = recovery.rejected(scope);
            if !rejected.is_empty() {
                prune_thinking(&mut body, |_, _, id| rejected.contains(&id));
            }
        }
        let key = self.key.clone();
        Ok(transport::stream(
            async move {
                let mut response = request
                    .try_clone()
                    .expect("headers-only request")
                    .json(&body)
                    .send()
                    .await
                    .map_err(transport::request_error)?;
                // The server rejected the input before any response content.
                // Never re-send after successful headers or an SSE error.
                if response.status() == reqwest::StatusCode::BAD_REQUEST {
                    let status = response.status();
                    let error = super::error_body::bounded_error_body(response, &[&key]).await;
                    let Some(removed) = remove_invalidated_thinking(&mut body, &error) else {
                        return Err(transport::fatal(format!("HTTP {status}: {error}")));
                    };
                    response = request
                        .json(&body)
                        .send()
                        .await
                        .map_err(transport::request_error)?;
                    if response.status().is_success()
                        && let Some(scope) = scope
                    {
                        recovery.remember(scope, removed);
                    }
                }
                Ok(TransportResponse {
                    response,
                    secrets: vec![key],
                    provider: PREFIX,
                    credential: Credential::ApiKey,
                })
            },
            MessagesStream::default(),
        ))
    }
}

// Anthropic's prefix-check error names the first invalid block by wire
// position. Keep the earlier run intact; a gap between signed blocks is
// itself invalid. Do not match a generic 400 or a corrupt signature.
fn remove_invalidated_thinking(body: &mut Value, error: &str) -> Option<Vec<Identity>> {
    let error: Value = serde_json::from_str(error).ok()?;
    if error["error"]["type"] != "invalid_request_error" {
        return None;
    }
    let (path, detail) = error["error"]["message"].as_str()?.split_once(": ")?;
    if !detail.contains("bound to a different conversation") {
        return None;
    }
    let (message, block) = path.strip_prefix("messages.")?.split_once(".content.")?;
    let start = (message.parse::<usize>().ok()?, block.parse::<usize>().ok()?);
    thinking_identity(
        body["messages"]
            .get(start.0)?
            .get("content")?
            .get(start.1)?,
    )?;
    Some(prune_thinking(body, |message, block, _| {
        (message, block) >= start
    }))
}

fn prune_thinking(
    body: &mut Value,
    mut remove: impl FnMut(usize, usize, Identity) -> bool,
) -> Vec<Identity> {
    let mut removed = Vec::new();
    let Some(messages) = body["messages"].as_array_mut() else {
        return removed;
    };
    for (i, message) in messages.iter_mut().enumerate() {
        let Some(content) = message["content"].as_array_mut() else {
            continue;
        };
        let mut j = 0;
        content.retain(|part| {
            let index = j;
            j += 1;
            if let Some(id) = thinking_identity(part)
                && remove(i, index, id)
            {
                removed.push(id);
                false
            } else {
                true
            }
        });
    }
    messages.retain(|message| !message["content"].as_array().is_some_and(Vec::is_empty));
    removed
}

fn request_body(req: &Request, model: &str) -> anyhow::Result<Value> {
    let cache = || json!({"type": "ephemeral"}); // API default: five minutes.
    let mut body = serde_json::Map::new();
    merge_options(
        &mut body,
        &req.options,
        &[
            "model",
            "stream",
            "messages",
            "system",
            "tools",
            "cache_control",
        ],
    )?;
    body.insert("model".into(), json!(model));
    body.insert("stream".into(), json!(true));
    body.insert("messages".into(), json!(messages(req)));
    body.entry("max_tokens").or_insert(json!(8192));
    ensure!(
        body["max_tokens"].as_u64().is_some_and(|n| n > 0),
        "Anthropic max_tokens must be a positive integer"
    );
    if crate::model::find(&req.model).is_some() {
        body.entry("thinking")
            .or_insert_with(|| json!({"type": "adaptive", "display": "summarized"}));
    }
    if let Some(system) = req
        .system_prompt
        .as_ref()
        .filter(|text| !text.trim().is_empty())
    {
        body.insert(
            "system".into(),
            json!([{"type": "text", "text": system, "cache_control": cache()}]),
        );
    }
    if !req.tools.is_empty() {
        let mut tools: Vec<_> = req.tools.iter().map(|tool| json!({
            "name": tool.name, "description": tool.description, "input_schema": tool.input_schema,
        })).collect();
        tools.last_mut().expect("nonempty tools")["cache_control"] = cache();
        body.insert("tools".into(), json!(tools));
    }
    // Keep stable tools/system breakpoints even when the user's suffix changes;
    // automatic caching advances a separate breakpoint as the transcript grows.
    body.insert("cache_control".into(), cache());
    Ok(Value::Object(body))
}

fn image_block(image: &ImageContent) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": image.media_type, "data": image.data}})
}

fn messages(req: &Request) -> Vec<Value> {
    let no_prefill = crate::model::find(&req.model).is_some();
    let continuation = || {
        (
            Role::User,
            vec![json!({"type": "text", "text": "Continue."})],
        )
    };
    let mut turns: Vec<(Role, Vec<Value>)> = Vec::new();
    for message in &req.messages {
        let mut parts = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                    parts.push(json!({"type": "text", "text": text}))
                }
                ContentBlock::Image { image } => parts.push(image_block(image)),
                ContentBlock::Reasoning { item }
                    if message.role == Role::Assistant && is_replay_item(item) =>
                {
                    parts.push(item["block"].clone());
                }
                ContentBlock::ToolCall {
                    id, name, input, ..
                } => parts.push(json!({
                    "type": "tool_use", "id": id, "name": name,
                    "input": if input.is_object() { input.clone() } else { json!({}) },
                })),
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    images,
                } => {
                    let mut blocks = Vec::new();
                    if !content.trim().is_empty() {
                        blocks.push(json!({"type": "text", "text": content}));
                    }
                    blocks.extend(images.iter().map(image_block));
                    parts.push(json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": blocks, "is_error": is_error}));
                }
                _ => {}
            }
        }
        if parts.is_empty() {
            continue;
        }
        // An assistant tail needed a synthetic user turn when resumed.
        // Reconstruct it between consecutive assistant responses so the
        // next tool request sends the same prefix the signature saw.
        if no_prefill
            && message.role == Role::Assistant
            && turns
                .last()
                .is_some_and(|(role, _)| *role == Role::Assistant)
        {
            turns.push(continuation());
        }
        match turns.last_mut() {
            Some((role, existing)) if *role == message.role => existing.extend(parts),
            _ => turns.push((message.role, parts)),
        }
    }
    if no_prefill
        && turns
            .last()
            .is_some_and(|(role, _)| *role == Role::Assistant)
    {
        turns.push(continuation());
    }
    // Anthropic requires all results first, in the user message immediately
    // after their calls. The neutral log can split results and user text.
    turns
        .into_iter()
        .map(|(role, mut parts)| {
            if role == Role::User {
                parts.sort_by_key(|part| part["type"] != "tool_result");
            }
            let role = match role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            json!({"role": role, "content": parts})
        })
        .collect()
}

#[derive(Default)]
struct MessagesStream {
    started: bool,
    next_index: u64,
    block: Option<Block>,
    calls: HashSet<String>,
    stop_reason: Option<StopReason>,
    usage: Usage,
}

enum Block {
    Text,
    Thinking {
        text: String,
        signature: String,
    },
    RedactedThinking(Value),
    Tool {
        id: String,
        name: String,
        input: Value,
        partial: String,
    },
    Other,
}

impl MessagesStream {
    fn update_usage(&mut self, value: &Value) {
        // Each field is cumulative, but deltas can omit fields included at
        // message_start. Anthropic's input_tokens already excludes both caches.
        for (key, count) in [
            ("input_tokens", &mut self.usage.input_tokens),
            ("output_tokens", &mut self.usage.output_tokens),
            (
                "cache_read_input_tokens",
                &mut self.usage.cache_read_input_tokens,
            ),
            (
                "cache_creation_input_tokens",
                &mut self.usage.cache_creation_input_tokens,
            ),
        ] {
            if let Some(n) = value[key].as_u64() {
                *count = n;
            }
        }
        self.usage.input_token_accounting = Some(InputTokenAccounting::ExcludesCached);
    }

    fn check_index(&self, event: &Value) -> Result<(), String> {
        if !self.started
            || self.stop_reason.is_some()
            || event["index"].as_u64() != Some(self.next_index)
        {
            return Err("unexpected Anthropic content block index or order".into());
        }
        Ok(())
    }

    fn start_block(
        &mut self,
        block: &Value,
        events: &mut Vec<ProviderEvent>,
    ) -> Result<(), String> {
        if self.block.is_some() {
            return Err("Anthropic opened a content block before closing the previous one".into());
        }
        self.block = Some(match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
                    events.push(ProviderEvent::TextDelta(text.into()));
                }
                Block::Text
            }
            Some("thinking") => {
                let thinking = block["thinking"].as_str().unwrap_or_default();
                if !thinking.is_empty() {
                    events.push(ProviderEvent::ReasoningSummaryDelta(thinking.into()));
                }
                Block::Thinking {
                    text: thinking.into(),
                    signature: block["signature"].as_str().unwrap_or_default().into(),
                }
            }
            Some("redacted_thinking") => Block::RedactedThinking(block.clone()),
            Some("tool_use") => {
                let id = nonempty(block, "id")?;
                let name = nonempty(block, "name")?;
                if !self.calls.insert(id.into()) {
                    return Err("duplicate Anthropic tool call id".into());
                }
                events.push(ProviderEvent::ToolCallStarted {
                    id: id.into(),
                    name: name.into(),
                    item_id: None,
                });
                Block::Tool {
                    id: id.into(),
                    name: name.into(),
                    input: block["input"].clone(),
                    partial: String::new(),
                }
            }
            _ => Block::Other,
        });
        Ok(())
    }

    fn delta(&mut self, delta: &Value, events: &mut Vec<ProviderEvent>) -> Result<(), String> {
        let block = self
            .block
            .as_mut()
            .ok_or("Anthropic delta without an open content block")?;
        match (block, delta["type"].as_str()) {
            (Block::Text, Some("text_delta")) => {
                let text = field(delta, "text")?;
                if !text.is_empty() {
                    events.push(ProviderEvent::TextDelta(text.into()));
                }
            }
            (Block::Thinking { text, .. }, Some("thinking_delta")) => {
                let fragment = field(delta, "thinking")?;
                text.push_str(fragment);
                if !fragment.is_empty() {
                    events.push(ProviderEvent::ReasoningSummaryDelta(fragment.into()));
                }
            }
            (Block::Thinking { signature, .. }, Some("signature_delta")) => {
                signature.push_str(field(delta, "signature")?);
            }
            (Block::Tool { id, partial, .. }, Some("input_json_delta")) => {
                let delta = field(delta, "partial_json")?;
                partial.push_str(delta);
                if !delta.is_empty() {
                    events.push(ProviderEvent::ToolCallInputDelta {
                        id: id.clone(),
                        delta: delta.into(),
                    });
                }
            }
            // New kinds of SSE deltas can be added without a version change.
            (_, Some("text_delta" | "thinking_delta" | "signature_delta" | "input_json_delta")) => {
                return Err("Anthropic delta does not match its content block".into());
            }
            _ => {}
        }
        Ok(())
    }

    fn stop_block(&mut self, events: &mut Vec<ProviderEvent>) -> Result<(), String> {
        let block = self
            .block
            .take()
            .ok_or("Anthropic stopped an unopened content block")?;
        self.next_index += 1;
        match block {
            Block::Text | Block::Other => {}
            Block::Thinking { text, signature } => {
                if signature.is_empty() {
                    return Err("missing Anthropic thinking signature".into());
                }
                if !text.is_empty() {
                    events.push(ProviderEvent::ReasoningSummaryCompleted);
                }
                events.push(replay(
                    json!({"type": "thinking", "thinking": text, "signature": signature}),
                ));
            }
            Block::RedactedThinking(block) => {
                nonempty(&block, "data")?;
                events.push(replay(block));
            }
            Block::Tool {
                id,
                name,
                input,
                partial,
            } => {
                let input = if partial.is_empty() {
                    input
                } else {
                    serde_json::from_str(&partial).unwrap_or(Value::Null)
                };
                // Truncated/invalid input must not become a valid empty call.
                let input = if input.is_object() {
                    input
                } else {
                    Value::Null
                };
                events.push(ProviderEvent::ToolCallCompleted { id, name, input });
            }
        }
        Ok(())
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("missing Anthropic {key}"))
}

fn nonempty<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    let value = field(value, key)?;
    if value.is_empty() {
        Err(format!("empty Anthropic {key}"))
    } else {
        Ok(value)
    }
}

fn replay(block: Value) -> ProviderEvent {
    ProviderEvent::ReasoningItem {
        item: json!({"type": REPLAY_ITEM, "block": block}),
    }
}

impl EventMapper for MessagesStream {
    fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String> {
        let event: Value = serde_json::from_str(data).map_err(|err| err.to_string())?;
        let mut events = Vec::new();
        match event["type"].as_str() {
            Some("message_start") if !self.started => {
                self.started = true;
                self.update_usage(&event["message"]["usage"]);
            }
            Some("content_block_start" | "content_block_delta" | "content_block_stop") => {
                self.check_index(&event)?;
                match event["type"].as_str() {
                    Some("content_block_start") => {
                        self.start_block(&event["content_block"], &mut events)?
                    }
                    Some("content_block_delta") => self.delta(&event["delta"], &mut events)?,
                    _ => self.stop_block(&mut events)?,
                }
            }
            Some("message_delta") if self.started && self.block.is_none() => {
                self.update_usage(&event["usage"]);
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(match reason {
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
                        "refusal" => StopReason::Refusal,
                        "pause_turn" => StopReason::Stopped,
                        _ => return Err("unknown Anthropic stop reason".into()),
                    });
                }
            }
            Some("message_stop") if self.started && self.block.is_none() => {
                let stop_reason = self
                    .stop_reason
                    .take()
                    .ok_or("Anthropic stopped without a stop reason")?;
                events.push(ProviderEvent::TurnComplete {
                    stop_reason,
                    usage: self.usage,
                });
            }
            Some("message_start" | "message_delta" | "message_stop") => {
                return Err("unexpected Anthropic message event order".into());
            }
            Some("error") => {
                let error = match event["error"]["type"].as_str() {
                    Some("overloaded_error" | "rate_limit_error") => ProviderEvent::RateLimited {
                        message: "Anthropic stream rate limited or overloaded".into(),
                        retry_after: None,
                    },
                    Some("api_error" | "timeout_error") => {
                        ProviderEvent::RetryableError("Anthropic transient stream error".into())
                    }
                    _ => return Err("Anthropic returned a stream error".into()),
                };
                events.push(error);
            }
            _ => {}
        }
        Ok(events)
    }

    fn finish(&mut self) -> Option<ProviderEvent> {
        Some(ProviderEvent::RetryableError(
            "Anthropic stream ended before message_stop".into(),
        ))
    }
}
