//! z.ai GLM provider: Anthropic-compatible (`/v1/messages`) and
//! OpenAI-compatible (`/chat/completions`) flavors.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::task::{Context, Poll};

use futures::{FutureExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::event::{ProviderEvent, StopReason};
use super::request::{Request, ToolDefinition, resolve_model};
use super::sse::SseParser;
use super::{EventStream, Provider};
use crate::session::{ChatMessage, ContentBlock, Role, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flavor {
    #[default]
    Anthropic,
    OpenAI,
}

#[derive(Clone)]
pub struct ZaiProvider {
    api_key: String,
    base_url: String,
    flavor: Flavor,
    http: reqwest::Client,
}

impl ZaiProvider {
    /// Test accessor for the wire body (prefix-stability checks).
    pub fn wire_body_for_test(&self, req: &Request) -> serde_json::Value {
        self.wire_body(req).expect("wire body")
    }

    pub fn new(api_key: String, base_url: Option<String>, flavor: Flavor) -> Self {
        let default_base = match flavor {
            Flavor::Anthropic => "https://api.z.ai/api/anthropic",
            // Coding-plan billing lives under /api/coding/paas/v4; the
            // plain /api/paas/v4 endpoint requires a separate balance.
            Flavor::OpenAI => "https://api.z.ai/api/coding/paas/v4",
        };
        Self {
            api_key,
            base_url: base_url.unwrap_or_else(|| default_base.into()),
            flavor,
            http: reqwest::Client::new(),
        }
    }

    fn wire_body(&self, req: &Request) -> anyhow::Result<serde_json::Value> {
        let (provider, model_id) = resolve_model(&req.model)?;
        if provider != "zai" {
            anyhow::bail!("model provider mismatch: expected zai, got {provider}");
        }
        let mut body = serde_json::Map::new();
        match self.flavor {
            Flavor::Anthropic => {
                body.insert("model".into(), serde_json::json!(model_id));
                // System as a block array with a cache breakpoint: the
                // system prompt + tools are the stable prefix every turn
                // reuses.
                if let Some(system) = &req.system_prompt {
                    body.insert(
                        "system".into(),
                        serde_json::json!([{
                            "type": "text",
                            "text": system,
                            "cache_control": {"type": "ephemeral"},
                        }]),
                    );
                }
                body.insert("max_tokens".into(), serde_json::json!(16_384));
                // Messages: moving cache breakpoint on the last block of the
                // last message (the canonical incremental-caching pattern —
                // each turn's entry covers everything up to that turn's end).
                let mut wire_messages: Vec<serde_json::Value> =
                    req.messages.iter().map(anthropic_message).collect();
                if let Some(message) = wire_messages.last_mut()
                    && let Some(content) = message["content"].as_array_mut()
                    && let Some(last_block) = content.last_mut()
                {
                    last_block["cache_control"] = serde_json::json!({"type": "ephemeral"});
                }
                body.insert("messages".into(), serde_json::json!(wire_messages));
                // Tools: breakpoint on the last tool definition.
                let mut wire_tools: Vec<serde_json::Value> =
                    req.tools.iter().map(anthropic_tool).collect();
                if let Some(last_tool) = wire_tools.last_mut() {
                    last_tool["cache_control"] = serde_json::json!({"type": "ephemeral"});
                }
                body.insert("tools".into(), serde_json::json!(wire_tools));
                body.insert("stream".into(), serde_json::json!(true));
            }
            Flavor::OpenAI => {
                body.insert("model".into(), serde_json::json!(model_id));
                let mut messages = Vec::new();
                if let Some(system) = &req.system_prompt {
                    messages.push(serde_json::json!({
                        "role": "system",
                        "content": system,
                    }));
                }
                messages.extend(req.messages.iter().flat_map(openai_message));
                body.insert("messages".into(), serde_json::json!(messages));
                body.insert(
                    "tools".into(),
                    serde_json::json!(
                        req.tools
                            .iter()
                            .map(|t| serde_json::json!({
                                "type": "function",
                                "function": {
                                    "name": t.name,
                                    "description": t.description,
                                    "parameters": t.input_schema,
                                },
                            }))
                            .collect::<Vec<_>>()
                    ),
                );
                body.insert("stream".into(), serde_json::json!(true));
                body.insert(
                    "stream_options".into(),
                    serde_json::json!({"include_usage": true}),
                );
            }
        }
        if let Some(options) = req.options.as_object() {
            body.extend(options.clone());
        }
        Ok(serde_json::Value::Object(body))
    }
}

/// Neutral -> Anthropic wire: content-block preserving (thinking blocks
/// must round-trip when tool use interleaves with thinking).
fn anthropic_message(msg: &ChatMessage) -> serde_json::Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<serde_json::Value> = msg
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => serde_json::json!({"type": "text", "text": text}),
            ContentBlock::Thinking { text, signature } => serde_json::json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature,
            }),
            ContentBlock::ToolCall { id, name, input } => serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            }),
        })
        .collect();
    serde_json::json!({"role": role, "content": content})
}

fn anthropic_tool(tool: &ToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

/// Neutral -> OpenAI chat-completions wire. Tool results expand into
/// separate `role: "tool"` messages (the wire format requires it).
fn openai_message(msg: &ChatMessage) -> Vec<serde_json::Value> {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let mut content_text = String::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => content_text.push_str(text),
            ContentBlock::Thinking { .. } => {}
            ContentBlock::ToolCall { id, name, input } => tool_calls.push(serde_json::json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": input.to_string()},
            })),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => tool_results.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tool_use_id,
                "content": content,
            })),
        }
    }
    if !tool_results.is_empty() {
        let mut messages = Vec::new();
        if !tool_calls.is_empty() {
            let mut value = serde_json::Map::new();
            value.insert("role".into(), serde_json::json!(role));
            value.insert("content".into(), serde_json::Value::Null);
            value.insert("tool_calls".into(), serde_json::json!(tool_calls));
            messages.push(serde_json::Value::Object(value));
        }
        messages.extend(tool_results);
        if !content_text.is_empty() {
            messages.push(serde_json::json!({
                "role": role,
                "content": content_text,
            }));
        }
        return messages;
    }
    let mut value = serde_json::Map::new();
    value.insert("role".into(), serde_json::json!(role));
    value.insert(
        "content".into(),
        if content_text.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(content_text)
        },
    );
    if !tool_calls.is_empty() {
        value.insert("tool_calls".into(), serde_json::json!(tool_calls));
    }
    vec![serde_json::Value::Object(value)]
}

impl Provider for ZaiProvider {
    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        let body = self.wire_body(&req)?;
        let url = match self.flavor {
            Flavor::Anthropic => format!("{}/v1/messages", self.base_url),
            Flavor::OpenAI => format!("{}/chat/completions", self.base_url),
        };
        let mut request = self
            .http
            .post(url)
            .timeout(std::time::Duration::from_secs(600));
        request = match self.flavor {
            Flavor::Anthropic => request
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01"),
            Flavor::OpenAI => request.bearer_auth(&self.api_key),
        };
        let request = request.json(&body).build()?;

        let (tx, rx) = mpsc::channel(64);
        let http = self.http.clone();
        let flavor = self.flavor;
        let tx_panic = tx.clone();
        let pump = async move {
            let mut parser = SseParser::new();
            let mut mapper = match flavor {
                Flavor::Anthropic => PumpMapper::Anthropic(AnthropicMapper::default()),
                Flavor::OpenAI => PumpMapper::OpenAI(OpenAiMapper::default()),
            };
            match http.execute(request).await {
                Ok(response) if response.status().is_success() => {
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
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    let _ = tx
                        .send(ProviderEvent::Error(format!("HTTP {status}: {body}")))
                        .await;
                }
                Err(e) => {
                    let _ = tx.send(ProviderEvent::Error(e.to_string())).await;
                }
            }
        };
        let handle = tokio::spawn(async move {
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

enum PumpMapper {
    Anthropic(AnthropicMapper),
    OpenAI(OpenAiMapper),
}

impl PumpMapper {
    fn map(&mut self, data: &str) -> Vec<ProviderEvent> {
        match self {
            PumpMapper::Anthropic(m) => m.map(data),
            PumpMapper::OpenAI(m) => m.map(data),
        }
    }

    fn finish(&mut self) -> Option<ProviderEvent> {
        match self {
            PumpMapper::Anthropic(m) => m.finish(),
            PumpMapper::OpenAI(m) => m.finish(),
        }
    }
}

/// Anthropic /v1/messages event mapping.
#[derive(Default)]
struct AnthropicMapper {
    /// content-block index -> block state.
    blocks: HashMap<usize, Block>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    completed: bool,
}

enum Block {
    Text,
    Thinking {
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        args: String,
    },
}

impl AnthropicMapper {
    fn map(&mut self, data: &str) -> Vec<ProviderEvent> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            return Vec::new();
        };
        let kind = value["type"].as_str().unwrap_or_default();
        match kind {
            "message_start" => {
                self.usage.input_tokens = value["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap_or_default();
                Vec::new()
            }
            "content_block_start" => {
                let index = value["index"].as_u64().unwrap_or_default() as usize;
                let block = &value["content_block"];
                match block["type"].as_str().unwrap_or_default() {
                    "tool_use" => {
                        self.blocks.insert(
                            index,
                            Block::ToolUse {
                                id: block["id"].as_str().unwrap_or_default().into(),
                                name: block["name"].as_str().unwrap_or_default().into(),
                                args: String::new(),
                            },
                        );
                        vec![ProviderEvent::ToolCallStarted {
                            id: block["id"].as_str().unwrap_or_default().into(),
                            name: block["name"].as_str().unwrap_or_default().into(),
                        }]
                    }
                    "thinking" => {
                        self.blocks
                            .insert(index, Block::Thinking { signature: None });
                        Vec::new()
                    }
                    _ => {
                        self.blocks.insert(index, Block::Text);
                        Vec::new()
                    }
                }
            }
            "content_block_delta" => {
                let index = value["index"].as_u64().unwrap_or_default() as usize;
                let delta = &value["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => vec![ProviderEvent::TextDelta(
                        delta["text"].as_str().unwrap_or_default().into(),
                    )],
                    "thinking_delta" => vec![ProviderEvent::ThinkingDelta(
                        delta["thinking"].as_str().unwrap_or_default().into(),
                    )],
                    "signature_delta" => {
                        if let Some(Block::Thinking { signature, .. }) = self.blocks.get_mut(&index)
                        {
                            *signature = delta["signature"].as_str().map(String::from);
                        }
                        Vec::new()
                    }
                    "input_json_delta" => {
                        let partial = delta["partial_json"].as_str().unwrap_or_default();
                        let (id, push_delta) = match self.blocks.get_mut(&index) {
                            Some(Block::ToolUse { id, args, .. }) => {
                                args.push_str(partial);
                                (id.clone(), partial.to_string())
                            }
                            _ => (String::new(), String::new()),
                        };
                        if push_delta.is_empty() {
                            Vec::new()
                        } else {
                            vec![ProviderEvent::ToolCallInputDelta {
                                id,
                                delta: push_delta,
                            }]
                        }
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_stop" => {
                let index = value["index"].as_u64().unwrap_or_default() as usize;
                match self.blocks.remove(&index) {
                    Some(Block::Thinking { signature, .. }) => {
                        vec![ProviderEvent::ThinkingCompleted { signature }]
                    }
                    Some(Block::ToolUse { id, name, args }) => {
                        let input = if args.trim().is_empty() {
                            serde_json::Value::Null
                        } else {
                            serde_json::from_str(&args).unwrap_or(serde_json::Value::Null)
                        };
                        vec![ProviderEvent::ToolCallCompleted { id, name, input }]
                    }
                    _ => Vec::new(),
                }
            }
            "message_delta" => {
                if let Some(stop) = value["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(match stop {
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        "refusal" => StopReason::Refusal,
                        "pause_turn" => StopReason::Paused,
                        _ => StopReason::Stopped,
                    });
                }
                if let Some(usage) = value["usage"].as_object() {
                    merge_usage(&mut self.usage, usage);
                }
                Vec::new()
            }
            "message_stop" => {
                self.completed = true;
                let mut events = Vec::new();
                let truncated = self.stop_reason == Some(StopReason::MaxTokens);
                if truncated {
                    // Synthesize completions for any pending calls,
                    // in content-block order.
                    let blocks = std::mem::take(&mut self.blocks);
                    let mut ordered: Vec<_> = blocks.into_iter().collect();
                    ordered.sort_by_key(|(i, _)| *i);
                    for (_, block) in ordered {
                        if let Block::ToolUse { id, name, .. } = block {
                            events.push(ProviderEvent::ToolCallCompleted {
                                id,
                                name,
                                input: serde_json::Value::Null,
                            });
                        }
                    }
                }
                events.push(ProviderEvent::TurnComplete {
                    stop_reason: self.stop_reason.clone().unwrap_or(StopReason::EndTurn),
                    usage: self.usage,
                });
                events
            }
            "error" => {
                self.completed = true;
                let message = value["error"]["message"]
                    .as_str()
                    .or_else(|| value["error"].as_str())
                    .unwrap_or("unknown provider error");
                vec![ProviderEvent::Error(message.into())]
            }
            _ => Vec::new(),
        }
    }

    fn finish(&mut self) -> Option<ProviderEvent> {
        if self.completed {
            None
        } else {
            Some(ProviderEvent::Error(
                "stream ended before message_stop".into(),
            ))
        }
    }
}

/// OpenAI chat-completions event mapping (z.ai paas v4 flavor).
#[derive(Default)]
struct OpenAiMapper {
    usage: Usage,
    stop_reason: Option<StopReason>,
    /// tool-call index -> (id, name, args buffer).
    calls: HashMap<usize, (String, String, String)>,
    /// Reasoning deltas seen since the last block boundary; chat-completions
    /// has no explicit boundary, so reasoning "completes" when content or a
    /// tool call arrives.
    thinking_open: bool,
    completed: bool,
}

impl OpenAiMapper {
    /// Close an open reasoning run (chat-completions has no explicit
    /// boundary; reasoning ends when content/tool calls/finish arrive).
    fn close_thinking(&mut self, events: &mut Vec<ProviderEvent>) {
        if self.thinking_open {
            self.thinking_open = false;
            events.push(ProviderEvent::ThinkingCompleted { signature: None });
        }
    }

    fn map(&mut self, data: &str) -> Vec<ProviderEvent> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if let Some(choice) = value["choices"].get(0) {
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str()
                && !text.is_empty()
            {
                events.push(ProviderEvent::TextDelta(text.into()));
            }
            if let Some(reasoning) = delta["reasoning_content"].as_str()
                && !reasoning.is_empty()
            {
                events.push(ProviderEvent::ThinkingDelta(reasoning.into()));
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                self.close_thinking(&mut events);
                for call in calls {
                    let index = call["index"].as_u64().unwrap_or_default() as usize;
                    let function = &call["function"];
                    let entry = self
                        .calls
                        .entry(index)
                        .or_insert_with(|| (String::new(), String::new(), String::new()));
                    if let Some(id) = call["id"].as_str()
                        && entry.0.is_empty()
                    {
                        entry.0 = id.into();
                    }
                    if let Some(name) = function["name"].as_str()
                        && !name.is_empty()
                        && entry.1.is_empty()
                    {
                        entry.1 = name.into();
                        events.push(ProviderEvent::ToolCallStarted {
                            id: entry.0.clone(),
                            name: name.into(),
                        });
                    }
                    if let Some(args) = function["arguments"].as_str()
                        && !args.is_empty()
                    {
                        entry.2.push_str(args);
                        events.push(ProviderEvent::ToolCallInputDelta {
                            id: entry.0.clone(),
                            delta: args.into(),
                        });
                    }
                }
            }
            if let Some(finish) = choice["finish_reason"].as_str() {
                self.close_thinking(&mut events);
                self.stop_reason = Some(match finish {
                    "stop" => StopReason::EndTurn,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    "content_filter" => StopReason::Refusal,
                    _ => StopReason::Stopped,
                });
                // Complete calls: parsed args when the model finished them,
                // null-input synthesis when truncated mid-arguments (event
                // contract: every Started call is Completed).
                let calls = std::mem::take(&mut self.calls);
                let mut ordered: Vec<_> = calls.into_iter().collect();
                ordered.sort_by_key(|(i, _)| *i);
                for (_, (id, name, args)) in ordered {
                    let input = serde_json::from_str(&args).unwrap_or(serde_json::Value::Null);
                    events.push(ProviderEvent::ToolCallCompleted { id, name, input });
                }
            }
        }
        // Mid-stream error payloads (chat-completions reports failures as
        // error chunks rather than terminating the HTTP response).
        if let Some(error) = value["error"].as_object() {
            let message = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown provider error");
            self.completed = true;
            return vec![ProviderEvent::Error(message.into())];
        }
        if let Some(usage) = value["usage"].as_object() {
            merge_usage(&mut self.usage, usage);
            // Guard: some compat servers attach usage to every chunk;
            // TurnComplete must fire exactly once.
            if !self.completed && self.stop_reason.is_some() {
                events.push(ProviderEvent::TurnComplete {
                    stop_reason: self.stop_reason.clone().unwrap_or(StopReason::EndTurn),
                    usage: self.usage,
                });
                self.completed = true;
            }
        }
        events
    }

    fn finish(&mut self) -> Option<ProviderEvent> {
        if self.completed {
            None
        } else if self.stop_reason.is_some() {
            // Stream ended after finish_reason but without a usage chunk.
            Some(ProviderEvent::TurnComplete {
                stop_reason: self.stop_reason.clone().unwrap(),
                usage: self.usage,
            })
        } else {
            Some(ProviderEvent::Error(
                "stream ended before finish_reason".into(),
            ))
        }
    }
}

fn merge_usage(usage: &mut Usage, wire: &serde_json::Map<String, serde_json::Value>) {
    let get = |k: &str| wire.get(k).and_then(|v| v.as_u64()).unwrap_or_default();
    // OpenAI-style nested cached tokens (z.ai openai flavor).
    let cached = wire
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or_default();
    if cached > 0 {
        usage.cache_read_input_tokens = cached;
    }
    let input = get("input_tokens").max(get("prompt_tokens"));
    if input > 0 {
        usage.input_tokens = input;
    }
    let output = get("output_tokens").max(get("completion_tokens"));
    if output > 0 {
        usage.output_tokens = output;
    }
    let cache_read = get("cache_read_input_tokens");
    if cache_read > 0 {
        usage.cache_read_input_tokens = cache_read;
    }
    let cache_create = get("cache_creation_input_tokens");
    if cache_create > 0 {
        usage.cache_creation_input_tokens = cache_create;
    }
}
