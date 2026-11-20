//! Native GenerateContent wire for Gemini 4 Neon on OpenCode Zen.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, ensure};
use serde_json::{Value, json};

use super::opencode::ZEN_PREFIX;
use super::request::{Request, merge_options, resolve_model};
use super::transport::{self, Affinity, Credential, EventMapper, TransportResponse};
use super::{EventStream, ProviderEvent, StopReason};
use crate::session::{ContentBlock, ImageContent, InputTokenAccounting, Role, Usage, new_id};

const ZEN_GEMINI_BASE: &str = "https://opencode.ai/inference/google/v1beta";
const REPLAY_ITEM: &str = "opencode_gemini_function_call";

pub(super) fn is_replay_item(item: &Value) -> bool {
    item["type"].as_str() == Some(REPLAY_ITEM)
}

#[derive(Clone)]
pub(super) struct GeminiProvider {
    key: String,
    base_url: String,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub(super) fn new(key: String, base_url: Option<&str>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(transport::CONNECT_TIMEOUT)
            .read_timeout(transport::IDLE_TIMEOUT)
            // The configured gateway is the only destination for the key,
            // affinity headers and conversation body.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("valid Zen Gemini HTTP client");
        Self {
            key,
            base_url: base_url
                .unwrap_or(ZEN_GEMINI_BASE)
                .trim_end_matches('/')
                .into(),
            client,
        }
    }

    fn endpoint(&self, model: &str) -> anyhow::Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.base_url).context("invalid Zen Gemini base URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host().is_some()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.username().is_empty()
                && url.password().is_none(),
            "invalid Zen Gemini base URL"
        );
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid Zen Gemini base path"))?
            .pop_if_empty()
            .extend(["models", &format!("{model}:streamGenerateContent")]);
        url.set_query(Some("alt=sse"));
        Ok(url)
    }

    pub(super) fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        let (prefix, model) = resolve_model(&req.model)?;
        ensure!(prefix == ZEN_PREFIX, "expected {ZEN_PREFIX} model");
        ensure!(!self.key.trim().is_empty(), "missing OpenCode Zen API key");
        let body = request_body(&req)?;
        let mut request = self
            .client
            .post(self.endpoint(model)?)
            .bearer_auth(&self.key)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        for (name, value) in Affinity::OpenCode.headers(req.cache_key.as_deref()) {
            request = request.header(name, value);
        }
        let key = self.key.clone();
        Ok(transport::stream(
            async move {
                let response = request.send().await.map_err(transport::request_error)?;
                Ok(TransportResponse {
                    response,
                    secrets: vec![key],
                    provider: ZEN_PREFIX,
                    credential: Credential::ApiKey,
                })
            },
            GeminiStream::default(),
        ))
    }
}

enum ReplayedCall<'a> {
    Native { name: &'a str, id: Option<&'a str> },
    Text { name: &'a str },
}

fn request_body(req: &Request) -> anyhow::Result<Value> {
    let mut body = serde_json::Map::new();
    // Results use a function name, and the native id only when Gemini
    // supplied one. Neither is derived by parsing the local call id.
    let mut calls = HashMap::new();
    let mut contents = Vec::new();
    for message in &req.messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "model",
        };
        let mut parts = Vec::new();
        let mut pending = None;
        let mut native_batch = None;
        for block in &message.content {
            let replay = pending.take();
            match block {
                ContentBlock::Text { text, .. } if !text.is_empty() => {
                    parts.push(json!({"text": text}))
                }
                ContentBlock::Image { image } => {
                    parts.push(json!({"inlineData": inline_data(image)}))
                }
                ContentBlock::Reasoning { item } if is_replay_item(item) => {
                    // The state belongs to the immediately following tool
                    // block, even if the loop had to rename its local id.
                    pending = Some(item);
                }
                ContentBlock::ToolCall {
                    id, name, input, ..
                } => {
                    let args = input.as_object().cloned().unwrap_or_default();
                    let signature = replay.and_then(|item| thought_signature(&item["signature"]));
                    // A parallel batch needs a signature on its first call only.
                    // A history from another model has no signature; keep that
                    // whole batch and its results as text rather than spoofing one.
                    if !*native_batch.get_or_insert(signature.is_some()) {
                        calls.insert(id.as_str(), ReplayedCall::Text { name });
                        parts.push(
                            json!({"text": format!("Tool call {name} ({id}): {}", json!(args))}),
                        );
                        continue;
                    }
                    let mut part = json!({"functionCall": {"name": name, "args": args}});
                    let native_id = replay.and_then(|item| item["id"].as_str());
                    if let Some(native_id) = native_id {
                        part["functionCall"]["id"] = json!(native_id);
                    }
                    if let Some(signature) = signature {
                        part["thoughtSignature"] = json!(signature);
                    }
                    calls.insert(
                        id.as_str(),
                        ReplayedCall::Native {
                            name,
                            id: native_id,
                        },
                    );
                    parts.push(part);
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    images,
                } => {
                    let Some(call) = calls.get(tool_use_id.as_str()) else {
                        // A shortened transcript can lose the matching call.
                        // Do not send an unpaired functionResponse to Gemini.
                        continue;
                    };
                    let key = if *is_error { "error" } else { "output" };
                    let (name, native_id) = match call {
                        ReplayedCall::Native { name, id } => (name, id),
                        ReplayedCall::Text { name } => {
                            parts.push(json!({"text": format!("Tool {key} from {name} ({tool_use_id}):\n{content}")}));
                            parts.extend(
                                images
                                    .iter()
                                    .map(|image| json!({"inlineData": inline_data(image)})),
                            );
                            continue;
                        }
                    };
                    let mut response = json!({"name": name, "response": {key: content}});
                    if let Some(id) = native_id {
                        response["id"] = json!(id);
                    }
                    if !images.is_empty() {
                        let (parts, refs): (Vec<_>, Vec<_>) = images
                            .iter()
                            .enumerate()
                            .map(|(i, image)| {
                                let display_name = format!("image-{}", i + 1);
                                let mut blob = inline_data(image);
                                blob["displayName"] = json!(display_name);
                                (json!({"inlineData": blob}), json!({"$ref": display_name}))
                            })
                            .unzip();
                        response["parts"] = json!(parts);
                        response["response"]["images"] = json!(refs);
                    }
                    parts.push(json!({"functionResponse": response}));
                }
                _ => {}
            }
        }
        if !parts.is_empty() {
            contents.push(json!({"role": role, "parts": parts}));
        }
    }
    body.insert("contents".into(), json!(contents));
    if let Some(system) = &req.system_prompt {
        body.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": system}]}),
        );
    }
    if !req.tools.is_empty() {
        let declarations: Vec<_> = req
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parametersJsonSchema": tool.input_schema,
                })
            })
            .collect();
        body.insert(
            "tools".into(),
            json!([{"functionDeclarations": declarations}]),
        );
    }
    merge_options(
        &mut body,
        &req.options,
        &[
            "contents",
            "systemInstruction",
            "system_instruction",
            "tools",
        ],
    )?;
    Ok(Value::Object(body))
}

fn inline_data(image: &ImageContent) -> Value {
    json!({"mimeType": image.media_type, "data": image.data})
}

fn thought_signature(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|signature| !signature.trim().is_empty())
}

#[derive(Default)]
struct GeminiStream {
    call_ids: HashSet<String>,
    thinking: bool,
    finish_reason: Option<String>,
    usage: Usage,
}

impl GeminiStream {
    fn close_thinking(&mut self, events: &mut Vec<ProviderEvent>) {
        if std::mem::take(&mut self.thinking) {
            events.push(ProviderEvent::ThinkingCompleted);
        }
    }

    fn part(&mut self, part: &Value, events: &mut Vec<ProviderEvent>) -> Result<(), String> {
        if let Some(call) = part.get("functionCall") {
            self.close_thinking(events);
            // Neon returns a complete args object per part by default.
            // Never execute an incomplete call if a proxy enables native
            // partial argument streaming.
            if call["willContinue"] == true || !call["partialArgs"].is_null() {
                return Err("Gemini partial function arguments are not supported".into());
            }
            let name = call["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or("missing Gemini function name")?;
            let input = match call.get("args") {
                None => json!({}),
                Some(args) if args.is_object() => args.clone(),
                Some(_) => return Err("Gemini function args must be an object".into()),
            };
            let native_id = call["id"].as_str().filter(|id| !id.is_empty());
            let signature = thought_signature(&part["thoughtSignature"]);
            // One streamed response is one call batch, even when its
            // parallel calls arrive in separate SSE events.
            if self.call_ids.is_empty() && signature.is_none() {
                return Err("Gemini first function call is missing thoughtSignature".into());
            }
            let id = native_id.map_or_else(new_id, str::to_owned);
            if !self.call_ids.insert(id.clone()) {
                return Err("duplicate Gemini function call id".into());
            }
            if signature.is_some() || native_id.is_some() {
                events.push(ProviderEvent::ReasoningItem {
                    item: json!({
                        "type": REPLAY_ITEM, "signature": signature, "id": native_id,
                    }),
                });
            }
            events.push(ProviderEvent::ToolCallStarted {
                id: id.clone(),
                name: name.into(),
                item_id: None,
            });
            events.push(ProviderEvent::ToolCallCompleted {
                id,
                name: name.into(),
                input,
            });
        } else if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
            if part["thought"] == true {
                self.thinking = true;
                events.push(ProviderEvent::ThinkingDelta(text.into()));
            } else {
                self.close_thinking(events);
                events.push(ProviderEvent::TextDelta(text.into()));
            }
        }
        Ok(())
    }

    fn terminal(&self) -> ProviderEvent {
        let stop_reason = match self.finish_reason.as_deref() {
            Some("STOP") if !self.call_ids.is_empty() => StopReason::ToolUse,
            Some("STOP") => StopReason::EndTurn,
            Some("MAX_TOKENS") => StopReason::MaxTokens,
            Some(
                "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII"
                | "MODEL_ARMOR",
            ) => StopReason::Refusal,
            Some(reason) => return ProviderEvent::Error(format!("Gemini stopped with {reason}")),
            None => {
                return ProviderEvent::RetryableError(
                    "Gemini stream ended before a finish reason".into(),
                );
            }
        };
        ProviderEvent::TurnComplete {
            stop_reason,
            usage: self.usage,
        }
    }
}

impl EventMapper for GeminiStream {
    fn map(&mut self, data: &str) -> Result<Vec<ProviderEvent>, String> {
        if data == "[DONE]" {
            return Ok(vec![self.terminal()]);
        }
        let value: Value = serde_json::from_str(data).map_err(|err| err.to_string())?;
        if !value["error"].is_null() {
            return Err("Gemini returned a stream error".into());
        }
        if let Some(usage) = value["usageMetadata"].as_object() {
            let tokens = |key| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            let cache = tokens("cachedContentTokenCount");
            self.usage = Usage {
                input_tokens: tokens("promptTokenCount").saturating_sub(cache),
                cache_read_input_tokens: cache,
                output_tokens: tokens("candidatesTokenCount")
                    .saturating_add(tokens("thoughtsTokenCount")),
                input_token_accounting: Some(InputTokenAccounting::ExcludesCached),
                ..Usage::default()
            };
        }
        if let Some(reason) = value["promptFeedback"]["blockReason"].as_str()
            && reason != "BLOCK_REASON_UNSPECIFIED"
        {
            self.finish_reason = Some("SAFETY".into());
        }
        let mut events = Vec::new();
        if let Some(candidates) = value["candidates"].as_array() {
            for candidate in candidates {
                if candidate["index"].as_u64().unwrap_or(0) != 0 {
                    continue;
                }
                if let Some(parts) = candidate["content"]["parts"].as_array() {
                    if !parts.is_empty() && self.finish_reason.is_some() {
                        return Err("Gemini sent content after its finish reason".into());
                    }
                    for part in parts {
                        self.part(part, &mut events)?;
                    }
                }
                if let Some(reason) = candidate["finishReason"]
                    .as_str()
                    .filter(|reason| *reason != "FINISH_REASON_UNSPECIFIED")
                {
                    if self
                        .finish_reason
                        .as_deref()
                        .is_some_and(|old| old != reason)
                    {
                        return Err("Gemini changed its finish reason".into());
                    }
                    self.close_thinking(&mut events);
                    self.finish_reason = Some(reason.into());
                }
            }
        }
        // Native SSE ends at EOF and may send usage after the finish reason.
        Ok(events)
    }

    fn finish(&mut self) -> Option<ProviderEvent> {
        Some(self.terminal())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ChatMessage;

    #[test]
    fn neon_function_response_references_each_image_by_display_name() {
        let req = Request {
            messages: vec![
                ChatMessage {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Reasoning {
                            item: json!({"type": REPLAY_ITEM, "signature": "c2ln"}),
                        },
                        ContentBlock::ToolCall {
                            id: "call".into(),
                            name: "read".into(),
                            input: json!({}),
                            item_id: None,
                        },
                    ],
                },
                ChatMessage {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call".into(),
                        content: "two images".into(),
                        is_error: false,
                        images: vec![
                            ImageContent::png(b"first"),
                            ImageContent::new("image/jpeg", b"second"),
                        ],
                    }],
                },
            ],
            ..Request::with_model("opencode/gemini-4-neon")
        };
        let body = request_body(&req).unwrap();
        let result = &body["contents"][1]["parts"][0]["functionResponse"];
        assert_eq!(result["response"]["output"], "two images");
        let images = result["parts"].as_array().unwrap();
        let refs = result["response"]["images"]
            .as_array()
            .expect("references in the response");
        assert_eq!(refs.len(), 2);
        assert_eq!(images.len(), 2);
        for ((image, reference), (mime, bytes)) in images
            .iter()
            .zip(refs)
            .zip([("image/png", "Zmlyc3Q="), ("image/jpeg", "c2Vjb25k")])
        {
            let blob = &image["inlineData"];
            let display_name = blob["displayName"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap();
            assert_eq!(*reference, json!({"$ref": display_name}));
            assert_eq!(blob["mimeType"], mime);
            assert_eq!(blob["data"], bytes);
        }
        assert_ne!(refs[0], refs[1]);
    }
}
