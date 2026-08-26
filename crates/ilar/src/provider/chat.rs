//! OpenAI-compatible chat-completions wire (`POST {base_url}/chat/completions`).
//!
//! One implementation serves every endpoint that speaks this dialect:
//! z.ai's coding-plan endpoint (see [`super::zai`]) and the
//! `[models.<name>]` entries a user points at their own server. What
//! differs between them is [`ChatDialect`] and nothing else — the body,
//! the message building, the SSE mapping and the transport glue are
//! shared, so a fix to any of them lands on both.

use std::collections::HashMap;

use super::event::{ProviderEvent, StopReason};
use super::mapper::{MapperCore, MapperLabels, merge_usage};
use super::request::{Request, merge_options, reserved_conflicts as conflicts, resolve_model};
use super::transport::{self, EventMapper as TransportEventMapper, TransportResponse};
use super::{EventStream, Provider};
use crate::session::{ChatMessage, ContentBlock, Role, Usage};

/// Body fields the wire itself owns: caller options may not overwrite
/// them, and a `[models.*]` entry that tries is refused when the config
/// is read rather than when the turn runs.
pub const RESERVED_OPTIONS: &[&str] = &["model", "messages", "tools", "stream", "stream_options"];

/// Which of a configured `options` table's keys a [`ChatDialect::custom`]
/// request would refuse, in the order the message lists them. The check
/// a config file makes at startup and the one the request makes at send
/// time are then the same check.
pub fn reserved_conflicts(options: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    conflicts(options, RESERVED_OPTIONS)
}

/// Everything that distinguishes one chat-completions endpoint from
/// another.
#[derive(Clone)]
pub struct ChatDialect {
    /// Prefix an ilar model id must carry to be served here.
    prefix: &'static str,
    base_url: String,
    /// Absent for keyless local servers: the request then carries no
    /// Authorization header at all, rather than an empty one.
    api_key: Option<String>,
    /// The id to put in the body's `model`, when it is not the ilar id's
    /// second half — a custom entry names its own wire id.
    wire_model: Option<String>,
    /// Image support, where the catalog is not the source of it.
    vision: Option<bool>,
    /// Body fields merged into every request for this endpoint
    /// (sampling: temperature, top_p …). Null when there are none.
    options: serde_json::Value,
    /// z.ai's `tool_stream`. Without it z.ai buffers the entire response
    /// server-side whenever tools are present (nothing streams until the
    /// whole turn is generated — verified against glm-5.3), which shows
    /// as minutes of dead air and gateway-timeout failures on long
    /// generations. No other server knows the field.
    tool_stream: bool,
}

impl ChatDialect {
    /// The z.ai endpoint: always keyed, vision from the catalog row,
    /// `tool_stream` on, wire id straight from the ilar id.
    pub(super) fn zai(api_key: String, base_url: String) -> Self {
        Self {
            prefix: "zai",
            base_url,
            api_key: Some(api_key),
            wire_model: None,
            vision: None,
            options: serde_json::Value::Null,
            tool_stream: true,
        }
    }

    /// A `[models.<name>]` endpoint: its own URL, its own wire id, a key
    /// only if one was configured, its own declared vision, and none of
    /// z.ai's body fields.
    pub fn custom(
        base_url: String,
        wire_model: String,
        api_key: Option<String>,
        vision: bool,
    ) -> Self {
        Self {
            prefix: crate::model::CUSTOM_PROVIDER,
            base_url,
            api_key,
            wire_model: Some(wire_model),
            vision: Some(vision),
            options: serde_json::Value::Null,
            tool_stream: false,
        }
    }

    /// Body fields merged into every request. Configuration screens them
    /// with [`reserved_conflicts`] when it reads them; the request path
    /// screens them again with the same list on the way out.
    pub fn with_options(mut self, options: serde_json::Value) -> Self {
        self.options = options;
        self
    }

    /// Reserved keys for this dialect: the shared set, plus z.ai's own
    /// field where it is used.
    fn reserved(&self) -> Vec<&'static str> {
        let mut reserved = RESERVED_OPTIONS.to_vec();
        if self.tool_stream {
            reserved.push("tool_stream");
        }
        reserved
    }
}

#[derive(Clone)]
pub struct ChatProvider {
    dialect: ChatDialect,
    http: reqwest::Client,
}

impl ChatProvider {
    pub fn new(dialect: ChatDialect) -> Self {
        Self {
            dialect,
            http: transport::streaming_client(),
        }
    }

    /// Test accessor for the wire body (prefix-stability checks).
    pub fn wire_body_for_test(&self, req: &Request) -> serde_json::Value {
        self.wire_body(req).expect("wire body")
    }

    fn wire_body(&self, req: &Request) -> anyhow::Result<serde_json::Value> {
        let (provider, model_id) = resolve_model(&req.model)?;
        let expected = self.dialect.prefix;
        if provider != expected {
            anyhow::bail!("model provider mismatch: expected {expected}, got {provider}");
        }
        let mut body = serde_json::Map::new();
        body.insert(
            "model".into(),
            serde_json::json!(self.dialect.wire_model.as_deref().unwrap_or(model_id)),
        );
        let mut messages = Vec::new();
        if let Some(system) = &req.system_prompt {
            messages.push(serde_json::json!({
                "role": "system",
                "content": system,
            }));
        }
        let vision = self
            .dialect
            .vision
            .unwrap_or_else(|| crate::model::supports_vision(&req.model));
        messages.extend(
            req.messages
                .iter()
                .flat_map(|message| openai_message(message, vision)),
        );
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
        if self.dialect.tool_stream {
            body.insert("tool_stream".into(), serde_json::json!(true));
        }
        let reserved = self.dialect.reserved();
        // Configured first, then the request's own: a per-turn option
        // wins over a standing one for the same key.
        merge_options(&mut body, &self.dialect.options, &reserved)?;
        merge_options(&mut body, &req.options, &reserved)?;
        Ok(serde_json::Value::Object(body))
    }
}

impl Provider for ChatProvider {
    fn provider_prefix(&self) -> Option<&'static str> {
        Some(self.dialect.prefix)
    }

    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        let body = self.wire_body(&req)?;
        let mut request = self
            .http
            .post(format!("{}/chat/completions", self.dialect.base_url));
        if let Some(api_key) = &self.dialect.api_key {
            request = request.bearer_auth(api_key);
        }
        let request = request.json(&body).build()?;

        let http = self.http.clone();
        let secrets = self.dialect.api_key.clone().into_iter().collect::<Vec<_>>();
        let send = async move {
            let response = http
                .execute(request)
                .await
                .map_err(transport::request_error)?;
            Ok(TransportResponse { response, secrets })
        };
        Ok(transport::stream(send, OpenAiMapper::new()))
    }
}

/// What a model that cannot see is told stood where an image was.
const IMAGE_GAP: &str = "[image omitted: this model cannot view images]";

/// The named gap, on its own line when text precedes it.
fn push_image_gap(text: &mut String) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(IMAGE_GAP);
}

/// One image as the chat-completions part.
fn image_part(image: &crate::session::ImageContent) -> serde_json::Value {
    serde_json::json!({
        "type": "image_url",
        "image_url": {"url": image.data_url()},
    })
}

/// Text plus images as chat-completions `content`. Text-only content
/// stays the plain string it always was, so cached prefixes do not move;
/// images make it a parts array, text part first.
fn content_value(text: &str, image_parts: Vec<serde_json::Value>) -> serde_json::Value {
    if image_parts.is_empty() {
        if text.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(text)
        }
    } else {
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(serde_json::json!({"type": "text", "text": text}));
        }
        parts.extend(image_parts);
        serde_json::json!(parts)
    }
}

/// Neutral -> OpenAI chat-completions wire. Tool results expand into
/// separate `role: "tool"` messages (the wire format requires it).
fn openai_message(msg: &ChatMessage, vision: bool) -> Vec<serde_json::Value> {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let mut content_text = String::new();
    let mut image_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => content_text.push_str(text),
            // Vision models get the real part; the placeholder keeps a
            // session with images usable on a text-only model.
            ContentBlock::Image { image } if vision => image_parts.push(image_part(image)),
            ContentBlock::Image { .. } => push_image_gap(&mut content_text),
            ContentBlock::Thinking { .. }
            | ContentBlock::ReasoningSummary { .. }
            | ContentBlock::Reasoning { .. }
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
                images,
                ..
            } => {
                // Same gating as a user image, read per request, so a
                // mid-session switch to a text-only model degrades to the
                // named gap instead of erroring.
                let mut result_text = content.clone();
                let result_parts = if vision {
                    images.iter().map(image_part).collect()
                } else {
                    for _ in images {
                        push_image_gap(&mut result_text);
                    }
                    Vec::new()
                };
                tool_results.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    // A result is a string even when it is empty; only
                    // images turn it into parts.
                    "content": if result_parts.is_empty() {
                        serde_json::json!(result_text)
                    } else {
                        content_value(&result_text, result_parts)
                    },
                }));
            }
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
        if !content_text.is_empty() || !image_parts.is_empty() {
            messages.push(serde_json::json!({
                "role": role,
                "content": content_value(&content_text, image_parts),
            }));
        }
        return messages;
    }
    if content_text.is_empty() && image_parts.is_empty() && tool_calls.is_empty() {
        return Vec::new();
    }
    let mut value = serde_json::Map::new();
    value.insert("role".into(), serde_json::json!(role));
    value.insert("content".into(), content_value(&content_text, image_parts));
    if !tool_calls.is_empty() {
        value.insert("tool_calls".into(), serde_json::json!(tool_calls));
    }
    vec![serde_json::Value::Object(value)]
}

const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;

/// The ledger key for a wire index: chat-completions addresses tool calls
/// by tool-call index, which is also the order truncation completes them
/// in.
fn block_key(index: usize) -> String {
    index.to_string()
}

/// A tool-call index the wire has started addressing, before (and while)
/// it is a call in the ledger.
///
/// Chat-completions promises no order within the dribble: a live GLM-4.6V
/// stream opened index 0 with an arguments-only chunk and named the call
/// afterwards. Fragments that arrive that early are held until the name
/// starts the call, then replay as deltas in arrival order — so the
/// consumer sees one event sequence whichever order the wire used.
#[derive(Default)]
struct StagedCall {
    id: String,
    /// Every argument byte this index has sent, replayed or not — what
    /// the completion parses, and what the size ceiling counts.
    arguments: String,
    /// Lengths of the fragments that arrived before the call entered the
    /// ledger. Nothing else is appended until it does, so they are the
    /// leading bytes of `arguments`: enough to replay each fragment as
    /// the delta the wire sent it as.
    early: Vec<usize>,
}

/// OpenAI-compatible chat-completions event mapping.
struct OpenAiMapper {
    /// Terminal state and the tool-call ledger, keyed by the tool-call
    /// index the wire addresses deltas by.
    core: MapperCore,
    usage: Usage,
    stop_reason: Option<StopReason>,
    /// ledger key -> staged call. Chat-completions dribbles a call's
    /// identity in over several chunks, so an index is staged here until
    /// its name arrives and the call enters the ledger.
    calls: HashMap<String, StagedCall>,
    /// Reasoning deltas seen since the last block boundary; chat-completions
    /// has no explicit boundary, so reasoning "completes" when content or a
    /// tool call arrives.
    thinking_open: bool,
}

impl OpenAiMapper {
    fn new() -> Self {
        Self {
            core: MapperCore::new(MapperLabels {
                flavor: "OpenAI-compatible",
                terminal: "completion",
                expected: "finish_reason",
            }),
            usage: Usage::default(),
            stop_reason: None,
            calls: HashMap::new(),
            thinking_open: false,
        }
    }

    /// Close an open reasoning run (chat-completions has no explicit
    /// boundary; reasoning ends when content/tool calls/finish arrive).
    fn close_thinking(&mut self, events: &mut Vec<ProviderEvent>) {
        if self.thinking_open {
            self.thinking_open = false;
            events.push(ProviderEvent::ThinkingCompleted);
        }
    }

    /// The lowest index the wire staged but never named, if any: it sent
    /// an id, or arguments, or both, and never the name that starts a
    /// call. Keys are [`block_key`] output, so the parse back to a number
    /// always succeeds; an unparsable one would just report last.
    fn unnamed(&self) -> Option<(&String, &StagedCall)> {
        self.calls
            .iter()
            .filter(|(key, _)| !self.core.has_key(key))
            .min_by_key(|(key, _)| key.parse::<usize>().unwrap_or(usize::MAX))
    }

    fn unnamed_error(key: &str) -> String {
        format!("OpenAI-compatible tool index {key} never received a name")
    }
}

impl TransportEventMapper for OpenAiMapper {
    fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String> {
        if data == "[DONE]" {
            if self.core.is_completed() {
                return Ok(Vec::new());
            }
            let stop_reason = self
                .stop_reason
                .clone()
                .ok_or_else(|| "OpenAI-compatible stream ended before finish_reason".to_string())?;
            self.core.complete();
            return Ok(vec![ProviderEvent::TurnComplete {
                stop_reason,
                usage: self.usage,
            }]);
        }
        self.core.guard_open()?;
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
                    let key = block_key(index);
                    let function = &call["function"];
                    let incoming_id = call["id"].as_str().filter(|id| !id.is_empty());
                    if let Some(id) = incoming_id
                        && self
                            .calls
                            .iter()
                            .any(|(other, staged)| other != &key && staged.id == id)
                    {
                        return Err(format!("duplicate OpenAI-compatible tool id {id:?}"));
                    }
                    let entry = self.calls.entry(key.clone()).or_default();
                    if let Some(id) = incoming_id {
                        if !entry.id.is_empty() && entry.id != id {
                            return Err(format!("OpenAI-compatible tool index {index} changed id"));
                        }
                        if entry.id.is_empty() {
                            entry.id = id.into();
                        }
                    }
                    if let Some(name) = function["name"].as_str()
                        && !name.is_empty()
                    {
                        match self.core.call(&key) {
                            Some(started) if started.name != name => {
                                return Err(format!(
                                    "OpenAI-compatible tool index {index} changed name"
                                ));
                            }
                            Some(_) => {}
                            None => {
                                if entry.id.is_empty() {
                                    return Err(
                                        "OpenAI-compatible tool name arrived before id".into()
                                    );
                                }
                                // The wire index is the order truncation
                                // completes the calls in.
                                self.core.start(key.clone(), index, entry.id.clone(), name);
                                events.push(ProviderEvent::ToolCallStarted {
                                    id: entry.id.clone(),
                                    name: name.into(),
                                    item_id: None,
                                });
                                // Arguments the wire sent before the name:
                                // the call exists now, so they stream in
                                // arrival order, ahead of anything later.
                                let mut offset = 0;
                                for length in std::mem::take(&mut entry.early) {
                                    let end = offset + length;
                                    events.push(ProviderEvent::ToolCallInputDelta {
                                        id: entry.id.clone(),
                                        delta: entry.arguments[offset..end].into(),
                                    });
                                    offset = end;
                                }
                            }
                        }
                    }
                    if function.get("arguments").is_some() && !function["arguments"].is_string() {
                        return Err("OpenAI-compatible arguments must be a string".into());
                    }
                    if let Some(args) = function["arguments"].as_str()
                        && !args.is_empty()
                    {
                        if entry.arguments.len().saturating_add(args.len())
                            > MAX_TOOL_ARGUMENT_BYTES
                        {
                            return Err("OpenAI-compatible tool arguments exceed size limit".into());
                        }
                        entry.arguments.push_str(args);
                        if self.core.call(&key).is_none() {
                            // No name yet, so no call to attach this to:
                            // it replays as a delta once one arrives.
                            entry.early.push(args.len());
                        } else {
                            events.push(ProviderEvent::ToolCallInputDelta {
                                id: entry.id.clone(),
                                delta: args.into(),
                            });
                        }
                    }
                }
            }
            if let Some(finish) = choice["finish_reason"].as_str() {
                if self.stop_reason.is_some() {
                    return Err("duplicate OpenAI-compatible finish reason".into());
                }
                self.close_thinking(&mut events);
                let stop_reason = match finish {
                    "stop" => StopReason::EndTurn,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    "content_filter" => StopReason::Refusal,
                    _ => {
                        return Err(format!(
                            "unknown OpenAI-compatible finish reason {finish:?}"
                        ));
                    }
                };
                self.stop_reason = Some(stop_reason.clone());
                // An index the wire staged but never named is not a call
                // this mapper can complete — checked before the stop
                // reason, whose "no tool calls" complaint would hide it.
                if let Some((key, _)) = self.unnamed() {
                    return Err(Self::unnamed_error(key));
                }
                self.core
                    .validate_stop(&stop_reason, stop_reason == StopReason::Refusal)?;
                // Complete calls: parsed args when the model finished them,
                // null-input synthesis when truncated mid-arguments (event
                // contract: every Started call is Completed).
                if stop_reason == StopReason::MaxTokens {
                    events.extend(self.core.truncated_completions());
                } else {
                    for call in self.core.take_open() {
                        let args = self
                            .calls
                            .get(&call.key)
                            .map(|staged| staged.arguments.as_str())
                            .unwrap_or_default();
                        let input = self.core.parse_tool_input(args)?;
                        events.push(ProviderEvent::ToolCallCompleted {
                            id: call.id,
                            name: call.name,
                            input,
                        });
                    }
                }
                self.calls.clear();
            }
        }
        // Mid-stream error payloads (chat-completions reports failures as
        // error chunks rather than terminating the HTTP response).
        if value["error"].is_object() {
            self.core.complete();
            return Ok(vec![super::error_body::stream_error_event(&value)]);
        }
        if value["usage"].is_object() {
            merge_usage(&mut self.usage, &value["usage"]);
            // Guard: some compat servers attach usage to every chunk;
            // TurnComplete must fire exactly once.
            if !self.core.is_completed() && self.stop_reason.is_some() {
                events.push(ProviderEvent::TurnComplete {
                    stop_reason: self.stop_reason.clone().unwrap_or(StopReason::EndTurn),
                    usage: self.usage,
                });
                self.core.complete();
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
        // Arguments buffered under an index the stream never named, then
        // EOF: whether a fragment beat the connection drop is timing, not
        // malformedness — retry like any other cut stream. A *complete*
        // stream (finish_reason arrived) that never named the index is
        // the hard error, raised in `map`.
        if let Some((key, _)) = self
            .unnamed()
            .filter(|(_, staged)| !staged.early.is_empty())
        {
            return Some(ProviderEvent::RetryableError(Self::unnamed_error(key)));
        }
        // Stream ended after finish_reason but without a usage chunk: the
        // turn is complete, only its accounting is short.
        if let Some(stop_reason) = self
            .stop_reason
            .clone()
            .filter(|_| !self.core.is_completed())
        {
            self.core.complete();
            return Some(ProviderEvent::TurnComplete {
                stop_reason,
                usage: self.usage,
            });
        }
        self.core.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_message() -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "what is this?".into(),
                },
                ContentBlock::Image {
                    image: crate::session::ImageContent {
                        media_type: "image/png".into(),
                        data: "aGVsbG8=".into(),
                    },
                },
            ],
        }
    }

    #[test]
    fn vision_models_get_real_image_parts_and_text_models_a_named_gap() {
        // Vision: one message, text + image_url parts.
        let wire = openai_message(&image_message(), true);
        assert_eq!(wire.len(), 1);
        let content = wire[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );

        // No vision: plain string with the named gap.
        let wire = openai_message(&image_message(), false);
        let content = wire[0]["content"].as_str().unwrap();
        assert!(content.contains("[image omitted"), "{content}");

        // Text-only stays the plain string it always was.
        let wire = openai_message(&ChatMessage::user_text("hi"), true);
        assert_eq!(wire[0]["content"], "hi");
    }

    /// One chat-completions stream through the mapper: the wire chunks in
    /// order, plus whatever the end of the stream synthesizes.
    fn openai_stream(chunks: &[&str]) -> Result<Vec<ProviderEvent>, String> {
        let mut mapper = OpenAiMapper::new();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(mapper.map(chunk)?);
        }
        events.extend(mapper.finish());
        Ok(events)
    }

    /// A tool-call index opened by an arguments-only chunk — no id, no
    /// name — exactly as a live GLM-4.6V stream sent it.
    const OPEN_ARGS: &str = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"type":"function","function":{"arguments":"{\"path\":"}}]},"finish_reason":null}]}"#;
    const REST_ARGS: &str = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"type":"function","function":{"arguments":"\"x\"}"}}]},"finish_reason":null}]}"#;
    const NAME: &str = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read"}}]},"finish_reason":null}]}"#;
    const NAME_AND_REST: &str = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"\"x\"}"}}]},"finish_reason":null}]}"#;
    const FINISH: &str = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;

    /// Chat-completions dribbles a call's identity in over several chunks
    /// and promises no order: arguments can arrive before the name that
    /// starts the call. However the wire splits them, the consumer sees
    /// the same events in the same order.
    #[test]
    fn openai_arguments_before_the_name_stream_like_arguments_after_it() {
        let expected = vec![
            ProviderEvent::ToolCallStarted {
                id: "call_1".into(),
                name: "read".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallInputDelta {
                id: "call_1".into(),
                delta: "{\"path\":".into(),
            },
            ProviderEvent::ToolCallInputDelta {
                id: "call_1".into(),
                delta: "\"x\"}".into(),
            },
            ProviderEvent::ToolCallCompleted {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "x"}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ];

        // Name first: the ordering the wire usually uses.
        assert_eq!(
            openai_stream(&[NAME, OPEN_ARGS, REST_ARGS, FINISH]).unwrap(),
            expected
        );
        // Arguments first, name last: the buffered fragments replay in
        // arrival order the moment the call starts.
        assert_eq!(
            openai_stream(&[OPEN_ARGS, REST_ARGS, NAME, FINISH]).unwrap(),
            expected
        );
        // The captured shape: an arguments-only chunk, then one chunk
        // carrying the id, the name and the rest of the arguments.
        assert_eq!(
            openai_stream(&[OPEN_ARGS, NAME_AND_REST, FINISH]).unwrap(),
            expected
        );
    }

    /// Buffering is not forgiveness: an index that only ever sent
    /// arguments is malformed, and the diagnostic names it.
    #[test]
    fn openai_arguments_that_never_get_a_name_are_an_error() {
        // The stream ends without a finish reason: a cut connection may
        // have beaten the naming chunk, so this retries.
        let events = openai_stream(&[OPEN_ARGS]).unwrap();
        assert!(
            matches!(events.as_slice(), [ProviderEvent::RetryableError(error)] if error.contains("index 0")),
            "{events:?}"
        );
        // The stream reaches its finish reason with the index still unnamed.
        let error = openai_stream(&[OPEN_ARGS, FINISH]).expect_err("never named");
        assert!(error.contains("index 0"), "{error}");

        // An index that staged nothing but an id is a stream cut short,
        // not a malformed one: that stays retryable.
        let id_only = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1"}]}}]}"#;
        let events = openai_stream(&[id_only]).unwrap();
        assert!(
            matches!(events.as_slice(), [ProviderEvent::RetryableError(_)]),
            "{events:?}"
        );
    }

    /// The argument-size ceiling applies to fragments held before the
    /// name arrives too, or a nameless index would be an unbounded buffer.
    #[test]
    fn openai_buffered_arguments_still_respect_the_size_limit() {
        let chunk = |args: &str| {
            format!(
                r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"function":{{"arguments":"{args}"}}}}]}}}}]}}"#
            )
        };
        let half = "a".repeat(MAX_TOOL_ARGUMENT_BYTES / 2 + 1);
        let error = openai_stream(&[&chunk(&half), &chunk(&half)]).expect_err("over the limit");
        assert!(error.contains("exceed size limit"), "{error}");
    }

    /// The dialect is the only difference between the two endpoints, and
    /// it shows up in exactly these fields.
    #[test]
    fn a_custom_dialect_omits_zai_body_fields_and_names_its_own_wire_model() {
        let zai = ChatProvider::new(ChatDialect::zai("k".into(), "http://zai.test".into()));
        let custom = ChatProvider::new(ChatDialect::custom(
            "http://local.test".into(),
            "llama3.3:70b".into(),
            None,
            false,
        ));
        let zai_body = zai.wire_body_for_test(&Request::with_model("zai/glm-4.7"));
        let custom_body = custom.wire_body_for_test(&Request::with_model("custom/llama"));

        assert_eq!(zai_body["model"], "glm-4.7");
        assert_eq!(zai_body["tool_stream"], true);
        assert_eq!(custom_body["model"], "llama3.3:70b");
        assert!(custom_body.get("tool_stream").is_none(), "{custom_body}");
        // Everything else is the same wire.
        for key in ["messages", "tools", "stream", "stream_options"] {
            assert_eq!(zai_body[key], custom_body[key], "{key}");
        }
    }
}
