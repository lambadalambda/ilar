//! z.ai GLM provider: Anthropic-compatible (`/v1/messages`) and
//! OpenAI-compatible (`/chat/completions`) flavors.

use std::collections::{HashMap, HashSet};

use super::event::{ProviderEvent, StopReason};
use super::request::{Request, ToolDefinition, merge_options, resolve_model};
use super::transport::{self, EventMapper as TransportEventMapper, TransportResponse};
use super::{EventStream, Provider};
use crate::session::{ChatMessage, ContentBlock, InputTokenAccounting, Role, Usage};

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
            http: transport::streaming_client(),
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
                let mut wire_messages: Vec<serde_json::Value> = req
                    .messages
                    .iter()
                    .map(anthropic_message)
                    .collect::<anyhow::Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect();
                let last_is_replay = req.messages.last().is_some_and(|message| {
                    message.content.iter().any(|block| {
                        matches!(
                            block,
                            ContentBlock::ProviderReplay { provider, .. }
                                if provider == "zai-anthropic"
                        )
                    })
                });
                if !last_is_replay
                    && let Some(message) = wire_messages.last_mut()
                    && let Some(content) = message["content"].as_array_mut()
                    && let Some(last_block) = content.last_mut()
                {
                    last_block["cache_control"] = serde_json::json!({"type": "ephemeral"});
                }
                if !req.continuations.is_empty() {
                    let mut content = Vec::new();
                    for continuation in &req.continuations {
                        let blocks = continuation.as_array().ok_or_else(|| {
                            anyhow::anyhow!("Anthropic continuation must be an array")
                        })?;
                        content.extend(blocks.iter().cloned());
                    }
                    wire_messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": content,
                    }));
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
                if !req.continuations.is_empty() {
                    anyhow::bail!("OpenAI-compatible z.ai does not support paused continuations");
                }
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
                // Without this, z.ai buffers the entire response server-side
                // whenever tools are present (nothing streams until the whole
                // turn is generated — verified against glm-5.3), which shows
                // as minutes of dead air and gateway-timeout failures on
                // long generations.
                body.insert("tool_stream".into(), serde_json::json!(true));
            }
        }
        let reserved: &[&str] = match self.flavor {
            Flavor::Anthropic => &["model", "system", "messages", "tools", "stream"],
            Flavor::OpenAI => &[
                "model",
                "messages",
                "tools",
                "stream",
                "stream_options",
                "tool_stream",
            ],
        };
        merge_options(&mut body, &req.options, reserved)?;
        Ok(serde_json::Value::Object(body))
    }
}

/// Neutral -> Anthropic wire: content-block preserving (thinking blocks
/// must round-trip when tool use interleaves with thinking).
fn anthropic_message(msg: &ChatMessage) -> anyhow::Result<Option<serde_json::Value>> {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let replays = msg
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ProviderReplay { provider, content } if provider == "zai-anthropic" => {
                Some(content)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if replays.len() > 1 {
        anyhow::bail!("assistant message contains multiple z.ai Anthropic replay blocks");
    }
    if let Some(content) = replays.first() {
        let Some(wire_blocks) = content.as_array() else {
            anyhow::bail!("invalid z.ai Anthropic provider replay block");
        };
        if msg.role != Role::Assistant {
            anyhow::bail!("invalid z.ai Anthropic provider replay block");
        }
        for block in wire_blocks {
            let block_type = block
                .as_object()
                .and_then(|block| block.get("type"))
                .and_then(serde_json::Value::as_str)
                .filter(|block_type| !block_type.is_empty())
                .ok_or_else(|| anyhow::anyhow!("invalid block in z.ai Anthropic replay"))?;
            if block_type == "tool_use" {
                let id = required_zai_str(block, "id", "replayed Anthropic tool id")
                    .map_err(anyhow::Error::msg)?;
                let name = required_zai_str(block, "name", "replayed Anthropic tool name")
                    .map_err(anyhow::Error::msg)?;
                let input = block
                    .get("input")
                    .filter(|input| input.is_object())
                    .ok_or_else(|| anyhow::anyhow!("invalid replayed Anthropic tool input"))?;
                let matches_neutral = msg.content.iter().any(|neutral| {
                    matches!(
                        neutral,
                        ContentBlock::ToolCall {
                            id: neutral_id,
                            name: neutral_name,
                            input: neutral_input,
                            ..
                        } if neutral_id == id && neutral_name == name && neutral_input == input
                    )
                });
                if !matches_neutral {
                    anyhow::bail!("replayed Anthropic tool call does not match neutral content");
                }
            }
        }
        let replayed_tool_count = wire_blocks
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .count();
        let neutral_tool_count = msg
            .content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
            .count();
        if replayed_tool_count != neutral_tool_count {
            anyhow::bail!("replayed Anthropic tool calls do not match neutral content");
        }
        return Ok(Some(serde_json::json!({
            "role": role,
            "content": content,
        })));
    }
    let content: Vec<serde_json::Value> = msg
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(serde_json::json!({"type": "text", "text": text})),
            ContentBlock::Thinking {
                text,
                signature: Some(signature),
            } => Some(serde_json::json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature,
            })),
            ContentBlock::Thinking {
                signature: None, ..
            } => None,
            ContentBlock::ReasoningSummary { .. } => None,
            ContentBlock::Reasoning { .. } => None,
            ContentBlock::ProviderReplay { .. } => None,
            ContentBlock::Diagnostic { .. } => None,
            ContentBlock::ToolCall {
                id, name, input, ..
            } => {
                let input = input
                    .is_object()
                    .then_some(input)
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                Some(serde_json::json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                }))
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            })),
        })
        .collect();
    Ok((!content.is_empty()).then(|| serde_json::json!({"role": role, "content": content})))
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
            ContentBlock::Thinking { .. }
            | ContentBlock::ReasoningSummary { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ProviderReplay { .. }
            | ContentBlock::Diagnostic { .. } => {}
            ContentBlock::ToolCall {
                id, name, input, ..
            } => {
                let input = if input.is_object() {
                    input.to_string()
                } else {
                    "{}".to_string()
                };
                tool_calls.push(serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": input},
                }));
            }
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
    if content_text.is_empty() && tool_calls.is_empty() {
        return Vec::new();
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
    fn provider_prefix(&self) -> Option<&'static str> {
        Some("zai")
    }

    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        let body = self.wire_body(&req)?;
        let url = match self.flavor {
            Flavor::Anthropic => format!("{}/v1/messages", self.base_url),
            Flavor::OpenAI => format!("{}/chat/completions", self.base_url),
        };
        let mut request = self.http.post(url);
        request = match self.flavor {
            Flavor::Anthropic => request
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01"),
            Flavor::OpenAI => request.bearer_auth(&self.api_key),
        };
        let request = request.json(&body).build()?;

        let http = self.http.clone();
        let flavor = self.flavor;
        let api_key = self.api_key.clone();
        let send = async move {
            let response = http
                .execute(request)
                .await
                .map_err(transport::request_error)?;
            Ok(TransportResponse {
                response,
                secrets: vec![api_key],
            })
        };
        let mapper = match flavor {
            Flavor::Anthropic => PumpMapper::Anthropic(AnthropicMapper::default()),
            Flavor::OpenAI => PumpMapper::OpenAI(OpenAiMapper::default()),
        };
        Ok(transport::stream(send, mapper))
    }
}

enum PumpMapper {
    Anthropic(AnthropicMapper),
    OpenAI(OpenAiMapper),
}

impl TransportEventMapper for PumpMapper {
    fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String> {
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
    tool_ids: HashSet<String>,
    wire_blocks: HashMap<usize, serde_json::Value>,
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
    ServerToolUse {
        args: String,
    },
    Raw,
}

const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;

impl AnthropicMapper {
    fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String> {
        if self.completed {
            return Err("Anthropic event arrived after message_stop".into());
        }
        let value = serde_json::from_str::<serde_json::Value>(data)
            .map_err(|error| format!("invalid Anthropic event JSON: {error}"))?;
        let kind = required_zai_str(&value, "type", "Anthropic event type")?;
        if self.stop_reason.is_some()
            && matches!(
                kind,
                "content_block_start" | "content_block_delta" | "content_block_stop"
            )
        {
            return Err("Anthropic content event arrived after stop reason".into());
        }
        let events = match kind {
            "message_start" => {
                self.usage.input_tokens = value["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap_or_default();
                Vec::new()
            }
            "content_block_start" => {
                let index = required_index(&value)?;
                if self.wire_blocks.contains_key(&index) {
                    return Err(format!("duplicate Anthropic content block index {index}"));
                }
                let block = &value["content_block"];
                let block_type = required_zai_str(block, "type", "Anthropic content block type")?;
                self.wire_blocks.insert(index, block.clone());
                match block_type {
                    "tool_use" => {
                        let id = required_zai_str(block, "id", "Anthropic tool id")?.to_string();
                        let name =
                            required_zai_str(block, "name", "Anthropic tool name")?.to_string();
                        if !self.tool_ids.insert(id.clone()) {
                            return Err(format!("duplicate Anthropic tool id {id:?}"));
                        }
                        self.blocks.insert(
                            index,
                            Block::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                args: String::new(),
                            },
                        );
                        vec![ProviderEvent::ToolCallStarted {
                            id,
                            name,
                            item_id: None,
                        }]
                    }
                    "thinking" => {
                        self.blocks
                            .insert(index, Block::Thinking { signature: None });
                        Vec::new()
                    }
                    "text" => {
                        self.blocks.insert(index, Block::Text);
                        Vec::new()
                    }
                    "server_tool_use" | "mcp_tool_use" => {
                        required_zai_str(block, "id", "Anthropic server tool id")?;
                        required_zai_str(block, "name", "Anthropic server tool name")?;
                        self.blocks.insert(
                            index,
                            Block::ServerToolUse {
                                args: String::new(),
                            },
                        );
                        Vec::new()
                    }
                    _ => {
                        self.blocks.insert(index, Block::Raw);
                        Vec::new()
                    }
                }
            }
            "content_block_delta" => {
                let index = required_index(&value)?;
                let delta = &value["delta"];
                match required_zai_str(delta, "type", "Anthropic delta type")? {
                    "text_delta" => {
                        if !matches!(self.blocks.get(&index), Some(Block::Text)) {
                            return Err(format!("text delta references non-text block {index}"));
                        }
                        let text = required_zai_str(delta, "text", "Anthropic text delta")?;
                        append_wire_string(&mut self.wire_blocks, index, "text", text)?;
                        vec![ProviderEvent::TextDelta(text.into())]
                    }
                    "thinking_delta" => {
                        if !matches!(self.blocks.get(&index), Some(Block::Thinking { .. })) {
                            return Err(format!(
                                "thinking delta references non-thinking block {index}"
                            ));
                        }
                        let thinking =
                            required_zai_str(delta, "thinking", "Anthropic thinking delta")?;
                        append_wire_string(&mut self.wire_blocks, index, "thinking", thinking)?;
                        vec![ProviderEvent::ThinkingDelta(thinking.into())]
                    }
                    "signature_delta" => {
                        let signature_delta =
                            required_zai_str(delta, "signature", "Anthropic signature delta")?;
                        let Some(Block::Thinking { signature, .. }) = self.blocks.get_mut(&index)
                        else {
                            return Err(format!(
                                "signature delta references non-thinking block {index}"
                            ));
                        };
                        signature
                            .get_or_insert_with(String::new)
                            .push_str(signature_delta);
                        append_wire_string(
                            &mut self.wire_blocks,
                            index,
                            "signature",
                            signature_delta,
                        )?;
                        Vec::new()
                    }
                    "input_json_delta" => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| "missing Anthropic argument delta".to_string())?;
                        let (id, args) = match self.blocks.get_mut(&index) {
                            Some(Block::ToolUse { id, args, .. }) => (Some(id.clone()), args),
                            Some(Block::ServerToolUse { args }) => (None, args),
                            _ => {
                                return Err(format!(
                                    "argument delta references non-tool block {index}"
                                ));
                            }
                        };
                        if args.len().saturating_add(partial.len()) > MAX_TOOL_ARGUMENT_BYTES {
                            return Err("Anthropic tool arguments exceed size limit".into());
                        }
                        if partial.is_empty() {
                            return Ok(Vec::new());
                        }
                        args.push_str(partial);
                        id.map(|id| ProviderEvent::ToolCallInputDelta {
                            id,
                            delta: partial.to_string(),
                        })
                        .into_iter()
                        .collect()
                    }
                    "citations_delta" => {
                        let citation = delta
                            .get("citation")
                            .cloned()
                            .ok_or_else(|| "missing Anthropic citation delta".to_string())?;
                        let block = self
                            .wire_blocks
                            .get_mut(&index)
                            .ok_or_else(|| format!("citation references unknown block {index}"))?;
                        let citations = block
                            .as_object_mut()
                            .ok_or_else(|| format!("Anthropic block {index} is not an object"))?
                            .entry("citations")
                            .or_insert_with(|| serde_json::json!([]))
                            .as_array_mut()
                            .ok_or_else(|| {
                                format!("Anthropic block {index} citations are not an array")
                            })?;
                        citations.push(citation);
                        Vec::new()
                    }
                    delta_type => {
                        return Err(format!("unknown Anthropic delta type {delta_type:?}"));
                    }
                }
            }
            "content_block_stop" => {
                let index = required_index(&value)?;
                match self.blocks.remove(&index) {
                    Some(Block::Thinking { signature, .. }) => {
                        vec![ProviderEvent::ThinkingCompleted { signature }]
                    }
                    Some(Block::ToolUse { id, name, args }) => {
                        let input = finish_wire_tool_input(&mut self.wire_blocks, index, &args)?;
                        vec![ProviderEvent::ToolCallCompleted { id, name, input }]
                    }
                    Some(Block::ServerToolUse { args }) => {
                        finish_wire_tool_input(&mut self.wire_blocks, index, &args)?;
                        Vec::new()
                    }
                    Some(Block::Text | Block::Raw) => Vec::new(),
                    None => return Err(format!("unknown or duplicate content block stop {index}")),
                }
            }
            "message_delta" => {
                if let Some(stop) = value["delta"]["stop_reason"].as_str() {
                    if self.stop_reason.is_some() {
                        return Err("duplicate Anthropic stop reason".into());
                    }
                    self.stop_reason = Some(match stop {
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        "refusal" => StopReason::Refusal,
                        "pause_turn" => StopReason::Paused,
                        _ => return Err(format!("unknown Anthropic stop reason {stop:?}")),
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
                let stop_reason = self
                    .stop_reason
                    .clone()
                    .ok_or_else(|| "Anthropic message_stop missing stop reason".to_string())?;
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
                } else if !self.blocks.is_empty() {
                    return Err("Anthropic message stopped with unfinished content blocks".into());
                }
                match &stop_reason {
                    StopReason::ToolUse if self.tool_ids.is_empty() => {
                        return Err("Anthropic tool_use stop has no tool calls".into());
                    }
                    StopReason::EndTurn | StopReason::Refusal | StopReason::Paused
                        if !self.tool_ids.is_empty() =>
                    {
                        return Err(format!(
                            "Anthropic {stop_reason:?} stop contradicts tool calls"
                        ));
                    }
                    _ => {}
                }
                if !truncated {
                    let mut blocks: Vec<_> = self.wire_blocks.iter().collect();
                    blocks.sort_by_key(|(index, _)| **index);
                    events.push(ProviderEvent::ResponseContent {
                        provider: "zai-anthropic".into(),
                        content: serde_json::Value::Array(
                            blocks.into_iter().map(|(_, block)| block.clone()).collect(),
                        ),
                    });
                }
                events.push(ProviderEvent::TurnComplete {
                    stop_reason,
                    usage: self.usage,
                });
                events
            }
            "error" => {
                self.completed = true;
                vec![super::error_body::stream_error_event(&value)]
            }
            _ => Vec::new(),
        };
        Ok(events)
    }

    fn finish(&mut self) -> Option<ProviderEvent> {
        if self.completed {
            None
        } else {
            Some(ProviderEvent::RetryableError(
                "stream ended before message_stop".into(),
            ))
        }
    }
}

fn append_wire_string(
    blocks: &mut HashMap<usize, serde_json::Value>,
    index: usize,
    key: &str,
    delta: &str,
) -> Result<(), String> {
    let value = blocks
        .get_mut(&index)
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| format!("Anthropic block {index} is not an object"))?
        .entry(key)
        .or_insert_with(|| serde_json::Value::String(String::new()));
    let serde_json::Value::String(value) = value else {
        return Err(format!("Anthropic block {index} {key} is not a string"));
    };
    value.push_str(delta);
    Ok(())
}

fn finish_wire_tool_input(
    blocks: &mut HashMap<usize, serde_json::Value>,
    index: usize,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    let block = blocks
        .get_mut(&index)
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| format!("Anthropic tool block {index} is missing"))?;
    let input = if arguments.is_empty() {
        block
            .get("input")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    } else {
        serde_json::from_str(arguments)
            .map_err(|error| format!("invalid Anthropic tool arguments: {error}"))?
    };
    if !input.is_object() {
        return Err("Anthropic tool arguments must be a JSON object".into());
    }
    block.insert("input".into(), input.clone());
    Ok(input)
}

fn required_zai_str<'a>(
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

fn required_index(value: &serde_json::Value) -> Result<usize, String> {
    let index = value
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "missing Anthropic content block index".to_string())?;
    usize::try_from(index).map_err(|_| "Anthropic content block index is too large".into())
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

    fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String> {
        if data == "[DONE]" {
            if self.completed {
                return Ok(Vec::new());
            }
            let stop_reason = self
                .stop_reason
                .clone()
                .ok_or_else(|| "OpenAI-compatible stream ended before finish_reason".to_string())?;
            self.completed = true;
            return Ok(vec![ProviderEvent::TurnComplete {
                stop_reason,
                usage: self.usage,
            }]);
        }
        if self.completed {
            return Err("OpenAI-compatible event arrived after completion".into());
        }
        let value = serde_json::from_str::<serde_json::Value>(data)
            .map_err(|error| format!("invalid OpenAI-compatible event JSON: {error}"))?;
        let mut events = Vec::new();
        if let Some(choices) = value.get("choices").and_then(serde_json::Value::as_array)
            && choices.len() > 1
        {
            return Err("OpenAI-compatible response contained multiple choices".into());
        }
        if self.stop_reason.is_some()
            && value["choices"]
                .as_array()
                .is_some_and(|choices| !choices.is_empty())
        {
            return Err("OpenAI-compatible event arrived after finish_reason".into());
        }
        if let Some(choice) = value["choices"].get(0) {
            let delta = &choice["delta"];
            if let Some(reasoning) = delta["reasoning_content"].as_str()
                && !reasoning.is_empty()
            {
                self.thinking_open = true;
                events.push(ProviderEvent::ThinkingDelta(reasoning.into()));
            }
            if let Some(text) = delta["content"].as_str()
                && !text.is_empty()
            {
                self.close_thinking(&mut events);
                events.push(ProviderEvent::TextDelta(text.into()));
            }
            if delta.get("tool_calls").is_some() && !delta["tool_calls"].is_array() {
                return Err("OpenAI-compatible tool_calls must be an array".into());
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                self.close_thinking(&mut events);
                for call in calls {
                    let index = call["index"]
                        .as_u64()
                        .ok_or_else(|| "missing OpenAI-compatible tool index".to_string())?
                        as usize;
                    let function = &call["function"];
                    let incoming_id = call["id"].as_str().filter(|id| !id.is_empty());
                    if let Some(id) = incoming_id
                        && self.calls.iter().any(|(other_index, (other_id, _, _))| {
                            *other_index != index && other_id == id
                        })
                    {
                        return Err(format!("duplicate OpenAI-compatible tool id {id:?}"));
                    }
                    let entry = self
                        .calls
                        .entry(index)
                        .or_insert_with(|| (String::new(), String::new(), String::new()));
                    if let Some(id) = incoming_id {
                        if !entry.0.is_empty() && entry.0 != id {
                            return Err(format!("OpenAI-compatible tool index {index} changed id"));
                        }
                        if entry.0.is_empty() {
                            entry.0 = id.into();
                        }
                    }
                    if let Some(name) = function["name"].as_str()
                        && !name.is_empty()
                    {
                        if !entry.1.is_empty() && entry.1 != name {
                            return Err(format!(
                                "OpenAI-compatible tool index {index} changed name"
                            ));
                        }
                        if entry.1.is_empty() {
                            if entry.0.is_empty() {
                                return Err("OpenAI-compatible tool name arrived before id".into());
                            }
                            entry.1 = name.into();
                            events.push(ProviderEvent::ToolCallStarted {
                                id: entry.0.clone(),
                                name: name.into(),
                                item_id: None,
                            });
                        }
                    }
                    if function.get("arguments").is_some() && !function["arguments"].is_string() {
                        return Err("OpenAI-compatible arguments must be a string".into());
                    }
                    if let Some(args) = function["arguments"].as_str()
                        && !args.is_empty()
                    {
                        if entry.0.is_empty() || entry.1.is_empty() {
                            return Err(
                                "OpenAI-compatible arguments arrived before tool start".into()
                            );
                        }
                        if entry.2.len().saturating_add(args.len()) > MAX_TOOL_ARGUMENT_BYTES {
                            return Err("OpenAI-compatible tool arguments exceed size limit".into());
                        }
                        entry.2.push_str(args);
                        events.push(ProviderEvent::ToolCallInputDelta {
                            id: entry.0.clone(),
                            delta: args.into(),
                        });
                    }
                }
            }
            if let Some(finish) = choice["finish_reason"].as_str() {
                if self.stop_reason.is_some() {
                    return Err("duplicate OpenAI-compatible finish reason".into());
                }
                self.close_thinking(&mut events);
                self.stop_reason = Some(match finish {
                    "stop" => StopReason::EndTurn,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    "content_filter" => StopReason::Refusal,
                    _ => {
                        return Err(format!(
                            "unknown OpenAI-compatible finish reason {finish:?}"
                        ));
                    }
                });
                match self.stop_reason.as_ref() {
                    Some(StopReason::ToolUse) if self.calls.is_empty() => {
                        return Err("OpenAI-compatible tool finish has no tool calls".into());
                    }
                    Some(StopReason::EndTurn | StopReason::Refusal) if !self.calls.is_empty() => {
                        return Err(format!(
                            "OpenAI-compatible {finish:?} finish contradicts tool calls"
                        ));
                    }
                    _ => {}
                }
                // Complete calls: parsed args when the model finished them,
                // null-input synthesis when truncated mid-arguments (event
                // contract: every Started call is Completed).
                let calls = std::mem::take(&mut self.calls);
                let mut ordered: Vec<_> = calls.into_iter().collect();
                ordered.sort_by_key(|(i, _)| *i);
                for (_, (id, name, args)) in ordered {
                    if id.is_empty() || name.is_empty() {
                        return Err("OpenAI-compatible tool call has empty id or name".into());
                    }
                    let input = if self.stop_reason == Some(StopReason::MaxTokens) {
                        serde_json::Value::Null
                    } else {
                        let input: serde_json::Value =
                            serde_json::from_str(&args).map_err(|error| {
                                format!("invalid OpenAI-compatible tool arguments: {error}")
                            })?;
                        if !input.is_object() {
                            return Err(
                                "OpenAI-compatible tool arguments must be a JSON object".into()
                            );
                        }
                        input
                    };
                    events.push(ProviderEvent::ToolCallCompleted { id, name, input });
                }
            }
        }
        // Mid-stream error payloads (chat-completions reports failures as
        // error chunks rather than terminating the HTTP response).
        if value["error"].is_object() {
            self.completed = true;
            return Ok(vec![super::error_body::stream_error_event(&value)]);
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
        if value.get("choices").is_none()
            && value.get("usage").is_none()
            && value.get("error").is_none()
        {
            return Err("OpenAI-compatible event missing choices, usage, or error".into());
        }
        Ok(events)
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
            Some(ProviderEvent::RetryableError(
                "stream ended before finish_reason".into(),
            ))
        }
    }
}

fn merge_usage(usage: &mut Usage, wire: &serde_json::Map<String, serde_json::Value>) {
    usage.input_token_accounting = Some(InputTokenAccounting::ExcludesCached);
    let get = |k: &str| wire.get(k).and_then(|v| v.as_u64()).unwrap_or_default();
    // OpenAI-style nested cached tokens (z.ai openai flavor).
    let nested_cached = wire
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or_default();
    if nested_cached > 0 {
        usage.cache_read_input_tokens = nested_cached;
    }
    let input = get("input_tokens").max(get("prompt_tokens"));
    if input > 0 {
        usage.input_tokens = input.saturating_sub(nested_cached);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_flavor_cached_input_is_normalized_out_of_prompt_total() {
        let mut usage = Usage::default();
        let wire = serde_json::json!({
            "prompt_tokens": 1_800,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 1_500}
        });
        merge_usage(&mut usage, wire.as_object().unwrap());
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.cache_read_input_tokens, 1_500);
        assert_eq!(usage.context_tokens(), 1_850);
    }
}
