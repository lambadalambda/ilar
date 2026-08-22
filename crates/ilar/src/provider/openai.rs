//! OpenAI Responses API provider.

use std::collections::HashMap;

use anyhow::Context as _;

use super::event::{ProviderEvent, StopReason};
use super::request::{Request, ToolDefinition, merge_options, resolve_model};
use super::transport::{self, EventMapper as TransportEventMapper, TransportResponse};
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
    prompt_cache_key: bool,
    session_headers: bool,
    token_url: String,
    http: reqwest::Client,
}

impl OpenAIProvider {
    /// `base_url` overrides the default `https://api.openai.com/v1`
    /// (proxies, gateways).
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        let prompt_cache_key = base_url.is_none();
        Self {
            auth: Auth::ApiKey(api_key),
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
            prompt_cache_key,
            // Codex-backend headers; the public API has no use for them.
            session_headers: false,
            token_url: format!("{}/oauth/token", crate::auth::AUTH_BASE),
            http: transport::streaming_client(),
        }
    }

    /// ChatGPT-account auth: Responses API through the ChatGPT backend.
    pub fn with_chatgpt_auth(store: crate::auth::AuthStore, base_url: Option<String>) -> Self {
        let prompt_cache_key = base_url.is_none();
        Self {
            auth: Auth::ChatGpt { store },
            base_url: base_url.unwrap_or_else(|| "https://chatgpt.com/backend-api/codex".into()),
            prompt_cache_key,
            session_headers: prompt_cache_key,
            token_url: format!("{}/oauth/token", crate::auth::AUTH_BASE),
            http: transport::streaming_client(),
        }
    }

    /// Test hook: point the refresh endpoint at a mock server.
    pub fn with_token_url_for_test(mut self, url: impl Into<String>) -> Self {
        self.token_url = url.into();
        self
    }

    /// Test hook: the unkeyed control arm of the live cache probe, now
    /// that sending the key is what production does.
    pub fn without_prompt_cache_key_for_test(mut self) -> Self {
        self.prompt_cache_key = false;
        self
    }

    /// The conversation's identity as headers, which is how the Codex
    /// backend pins a request to the shard holding its cached prefix.
    /// `prompt_cache_key` alone does not: measured over four alternating
    /// arms, 2/10 follow-up steps read a cache without these and 10/10
    /// with them. Only the Codex backend reads them, so only it gets them.
    fn session_headers(&self, cache_key: Option<&str>) -> Vec<(&'static str, String)> {
        let Some(cache_key) = cache_key.filter(|_| self.session_headers) else {
            return Vec::new();
        };
        vec![
            ("session-id", cache_key.to_string()),
            ("thread-id", cache_key.to_string()),
        ]
    }

    /// Test hook: the control arm for the session-affinity headers.
    pub fn without_session_headers_for_test(mut self) -> Self {
        self.session_headers = false;
        self
    }

    fn wire_body(&self, req: &Request) -> anyhow::Result<serde_json::Value> {
        let reasoning_summaries =
            crate::model::find(&req.model).is_some_and(|model| model.reasoning_summaries);
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
        if self.prompt_cache_key
            && let Some(cache_key) = &req.cache_key
        {
            body.insert("prompt_cache_key".into(), serde_json::json!(cache_key));
        }
        body.insert("stream".into(), serde_json::json!(true));
        merge_options(
            &mut body,
            &req.options,
            &[
                "model",
                "instructions",
                "input",
                "tools",
                "prompt_cache_key",
                "stream",
            ],
        )?;
        if reasoning_summaries {
            let reasoning = body
                .entry("reasoning")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(reasoning) = reasoning.as_object_mut() {
                reasoning
                    .entry("summary")
                    .or_insert_with(|| serde_json::json!("auto"));
            }
        }
        Ok(serde_json::Value::Object(body))
    }
}

/// One neutral message may map to zero (dropped thinking) or more wire items.
///
/// Items go back in the canonical shape the API returned them in, because
/// a replayed conversation is only cacheable if the server can rebuild the
/// same item graph: typed `message` items rather than bare role/content
/// pairs, and function calls carrying the item id a preceding reasoning
/// item refers to. `function_call_output.output` stays a plain string —
/// that is the canonical form for text results, arrays being for
/// multimodal ones.
fn wire_input_items(msg: &ChatMessage) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    // Group tool results with their role; text becomes message items.
    let mut text = String::new();
    let flush_text = |text: &mut String, items: &mut Vec<serde_json::Value>| {
        if !text.is_empty() {
            let (role, part) = match msg.role {
                Role::User => ("user", "input_text"),
                Role::Assistant => ("assistant", "output_text"),
            };
            items.push(serde_json::json!({
                "type": "message",
                "role": role,
                "content": [{"type": part, "text": std::mem::take(text)}],
            }));
        }
    };
    for block in &msg.content {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::Thinking { .. } => {} // reasoning items are server-managed
            ContentBlock::ReasoningSummary { .. } => {}
            ContentBlock::Diagnostic { .. } => {}
            ContentBlock::ProviderReplay { .. } => {}
            ContentBlock::Reasoning { item } => {
                flush_text(&mut text, &mut items);
                items.push(item.clone());
            }
            ContentBlock::ToolCall {
                id,
                name,
                input,
                item_id,
            } => {
                flush_text(&mut text, &mut items);
                let input = input
                    .is_object()
                    .then_some(input)
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let mut call = serde_json::json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": input.to_string(),
                });
                // Omitted for sessions recorded before the id was kept,
                // and for calls that never had one.
                if let Some(item_id) = item_id
                    && let Some(object) = call.as_object_mut()
                {
                    object.insert("id".into(), serde_json::json!(item_id));
                }
                items.push(call);
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

        let session_headers = self.session_headers(req.cache_key.as_deref());
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

        let http = self.http.clone();
        let token_url = self.token_url.clone();
        let send = async move {
            let mut current_token = token;
            let mut current_account = account;
            let mut refreshed = false;

            loop {
                let mut builder = http.post(&url).bearer_auth(&current_token).json(&body);
                if is_chatgpt {
                    builder = builder
                        .header("originator", "codex_cli_rs")
                        .header("OpenAI-Beta", "responses=experimental");
                    if let Some(account) = &current_account {
                        builder = builder.header("chatgpt-account-id", account);
                    }
                    for (name, value) in &session_headers {
                        builder = builder.header(*name, value);
                    }
                }
                let request = builder.build().map_err(transport::fatal)?;
                match http
                    .execute(request)
                    .await
                    .map_err(transport::request_error)
                {
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
                                    return Err(transport::fatal(format!(
                                        "token refresh failed: {e:#}"
                                    )));
                                }
                            }
                        }
                        return Ok(TransportResponse {
                            response,
                            secrets: vec![current_token],
                        });
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        Ok(transport::stream(send, EventMapper::default()))
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
    reasoning_items: HashMap<String, u64>,
    reasoning_summary: Option<ActiveReasoningSummary>,
    started_summaries: std::collections::HashSet<ReasoningSummaryKey>,
    closed_summaries: HashMap<ReasoningSummaryKey, ClosedReasoningSummary>,
    completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReasoningSummaryKey {
    item_id: String,
    output_index: u64,
    summary_index: u64,
}

struct ActiveReasoningSummary {
    key: ReasoningSummaryKey,
    text: String,
    text_done: bool,
}

#[derive(Clone)]
struct ClosedReasoningSummary {
    text: String,
    complete: bool,
}

impl TransportEventMapper for EventMapper {
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
            "response.reasoning_summary_part.added" => {
                let key = reasoning_summary_key(&value)?;
                if !self.started_summaries.insert(key.clone())
                    || self.closed_summaries.contains_key(&key)
                {
                    return Err("duplicate OpenAI reasoning summary part".into());
                }
                if self.reasoning_items.get(&key.item_id) != Some(&key.output_index) {
                    return Err("OpenAI reasoning summary referenced an unannounced item".into());
                }
                if self.reasoning_summary.is_some() {
                    return Err("OpenAI reasoning summaries overlapped".into());
                }
                let text = reasoning_summary_part_text(&value)?;
                self.reasoning_summary = Some(ActiveReasoningSummary {
                    key,
                    text: text.into(),
                    text_done: false,
                });
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![ProviderEvent::ReasoningSummaryDelta(text.into())]
                }
            }
            "response.reasoning_summary_text.delta" => {
                let key = reasoning_summary_key(&value)?;
                if !self.started_summaries.contains(&key)
                    || self.closed_summaries.contains_key(&key)
                {
                    return Err("OpenAI reasoning summary delta arrived outside its part".into());
                }
                let delta = required_string(&value, "delta", "OpenAI reasoning summary delta")?;
                let Some(summary) = self
                    .reasoning_summary
                    .as_mut()
                    .filter(|summary| summary.key == key)
                else {
                    return Err("OpenAI reasoning summary delta referenced the wrong part".into());
                };
                if summary.text_done {
                    return Err(
                        "OpenAI reasoning summary delta arrived after text completion".into(),
                    );
                }
                summary.text.push_str(delta);
                if delta.is_empty() {
                    Vec::new()
                } else {
                    vec![ProviderEvent::ReasoningSummaryDelta(delta.into())]
                }
            }
            "response.reasoning_summary_text.done" => {
                let key = reasoning_summary_key(&value)?;
                if !self.started_summaries.contains(&key)
                    || self.closed_summaries.contains_key(&key)
                {
                    return Err("OpenAI reasoning summary text completed outside its part".into());
                }
                let text = required_string(&value, "text", "OpenAI reasoning summary text")?;
                let Some(summary) = self
                    .reasoning_summary
                    .as_mut()
                    .filter(|summary| summary.key == key)
                else {
                    return Err("OpenAI reasoning summary text referenced the wrong part".into());
                };
                if summary.text_done {
                    return Err("duplicate OpenAI reasoning summary text completion".into());
                }
                let mut events = Vec::new();
                if summary.text.is_empty() && !text.is_empty() {
                    summary.text.push_str(text);
                    events.push(ProviderEvent::ReasoningSummaryDelta(text.into()));
                } else if summary.text != text {
                    return Err("OpenAI reasoning summary changed at completion".into());
                }
                summary.text_done = true;
                events
            }
            "response.reasoning_summary_part.done" => {
                let key = reasoning_summary_key(&value)?;
                if !self.started_summaries.contains(&key)
                    || self.closed_summaries.contains_key(&key)
                {
                    return Err("duplicate OpenAI reasoning summary part completion".into());
                }
                let text = reasoning_summary_part_text(&value)?;
                let Some(mut summary) = self
                    .reasoning_summary
                    .take()
                    .filter(|summary| summary.key == key)
                else {
                    return Err("OpenAI reasoning summary part completion mismatch".into());
                };
                let mut events = Vec::new();
                if summary.text.is_empty() && !text.is_empty() {
                    summary.text.push_str(text);
                    events.push(ProviderEvent::ReasoningSummaryDelta(text.into()));
                } else if summary.text != text {
                    return Err("OpenAI reasoning summary part changed at completion".into());
                }
                let status = value
                    .get("status")
                    .map(|status| {
                        status
                            .as_str()
                            .ok_or_else(|| "invalid OpenAI reasoning summary status".to_string())
                    })
                    .transpose()?;
                let complete = match status {
                    None if summary.text_done => {
                        events.push(ProviderEvent::ReasoningSummaryCompleted);
                        true
                    }
                    None => {
                        return Err(
                            "OpenAI reasoning summary part completed before its text".into()
                        );
                    }
                    Some("incomplete") => false,
                    Some(status) => {
                        return Err(format!(
                            "unsupported OpenAI reasoning summary status {status:?}"
                        ));
                    }
                };
                self.closed_summaries.insert(
                    key,
                    ClosedReasoningSummary {
                        text: summary.text,
                        complete,
                    },
                );
                events
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
                    let item_id_for_event = item_id.clone();
                    self.calls.insert(item_id, (call_id.clone(), name.clone()));
                    self.pending.push(call_id.clone());
                    self.tool_call_seen = true;
                    vec![ProviderEvent::ToolCallStarted {
                        id: call_id,
                        name,
                        item_id: Some(item_id_for_event),
                    }]
                } else if item_type == "reasoning" {
                    let item_id = required_str(item, "id", "OpenAI reasoning item id")?;
                    let output_index =
                        required_u64(&value, "output_index", "OpenAI reasoning output index")?;
                    if self
                        .reasoning_items
                        .insert(item_id.into(), output_index)
                        .is_some()
                    {
                        return Err(format!("duplicate OpenAI reasoning item {item_id:?}"));
                    }
                    Vec::new()
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
                    let item_id = required_str(item, "id", "OpenAI completed reasoning item id")?;
                    if !self.completed_items.insert(item_id.into()) {
                        return Err(format!(
                            "duplicate completed OpenAI reasoning item {item_id:?}"
                        ));
                    }
                    let output_index =
                        required_u64(&value, "output_index", "OpenAI reasoning output index")?;
                    if self.reasoning_items.remove(item_id) != Some(output_index) {
                        return Err("OpenAI completed reasoning item mismatched its start".into());
                    }
                    let summaries = item
                        .get("summary")
                        .and_then(serde_json::Value::as_array)
                        .ok_or_else(|| {
                            "missing OpenAI completed reasoning summaries".to_string()
                        })?;
                    let expected = self
                        .closed_summaries
                        .iter()
                        .filter(|(key, _)| key.item_id == item_id)
                        .map(|(key, summary)| (key.clone(), summary.clone()))
                        .collect::<Vec<_>>();
                    if summaries.len() != expected.len() {
                        return Err("OpenAI completed reasoning summaries changed count".into());
                    }
                    let item_incomplete = item.get("status").and_then(serde_json::Value::as_str)
                        == Some("incomplete");
                    for (key, expected_summary) in &expected {
                        let summary = summaries
                            .get(key.summary_index as usize)
                            .filter(|summary| summary.is_object())
                            .ok_or_else(|| {
                                "missing OpenAI completed reasoning summary".to_string()
                            })?;
                        if required_str(summary, "type", "OpenAI reasoning summary type")?
                            != "summary_text"
                            || required_string(
                                summary,
                                "text",
                                "OpenAI completed reasoning summary text",
                            )? != expected_summary.text
                        {
                            return Err("OpenAI completed reasoning summary changed content".into());
                        }
                        if !expected_summary.complete && !item_incomplete {
                            return Err(
                                "incomplete OpenAI reasoning summary closed in a completed item"
                                    .into(),
                            );
                        }
                    }
                    for (key, _) in expected {
                        self.closed_summaries.remove(&key);
                    }
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
                if self.reasoning_summary.is_some()
                    || !self.reasoning_items.is_empty()
                    || !self.closed_summaries.is_empty()
                {
                    return Err("OpenAI response completed with unfinished reasoning state".into());
                }
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
                vec![super::error_body::stream_error_event(&value)]
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
            Some(ProviderEvent::RetryableError(
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

fn required_string<'a>(
    value: &'a serde_json::Value,
    key: &str,
    label: &str,
) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("missing or invalid {label}"))
}

fn reasoning_summary_key(value: &serde_json::Value) -> Result<ReasoningSummaryKey, String> {
    Ok(ReasoningSummaryKey {
        item_id: required_str(value, "item_id", "OpenAI reasoning item id")?.into(),
        output_index: required_u64(value, "output_index", "OpenAI reasoning output index")?,
        summary_index: required_u64(value, "summary_index", "OpenAI reasoning summary index")?,
    })
}

fn reasoning_summary_part_text(value: &serde_json::Value) -> Result<&str, String> {
    let part = value
        .get("part")
        .filter(|part| part.is_object())
        .ok_or_else(|| "missing OpenAI reasoning summary part".to_string())?;
    if required_str(part, "type", "OpenAI reasoning summary part type")? != "summary_text" {
        return Err("unsupported OpenAI reasoning summary part type".into());
    }
    required_string(part, "text", "OpenAI reasoning summary part text")
}

fn required_u64(value: &serde_json::Value, field: &str, label: &str) -> Result<u64, String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| format!("missing or invalid {label}"))
}

fn wire_usage(usage: &serde_json::Value) -> Usage {
    let detail = |field: &str| {
        usage["input_tokens_details"][field]
            .as_u64()
            .or_else(|| usage["prompt_tokens_details"][field].as_u64())
            .unwrap_or_default()
    };
    let cached = detail("cached_tokens");
    // Reported from GPT-5.6 on. Without it every request looked like it
    // wrote nothing, which made "the prefix was never cached" and "the
    // prefix was cached somewhere this request could not reach"
    // indistinguishable.
    let written = detail("cache_write_tokens");
    let input = usage["input_tokens"]
        .as_u64()
        .or_else(|| usage["prompt_tokens"].as_u64())
        .unwrap_or_default();
    Usage {
        // Both are subsets of the reported input total, so the remainder
        // is what was neither read from nor written to the cache. Splitting
        // them out keeps the cost identical — models with no separate write
        // price bill writes at the input rate — while making the split
        // visible.
        input_tokens: input.saturating_sub(cached).saturating_sub(written),
        output_tokens: usage["output_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64())
            .unwrap_or_default(),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: written,
        input_token_accounting: Some(InputTokenAccounting::ExcludesCached),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from OpenAI's prompt-caching guide, verbatim: reads and
    /// writes are both carved out of the input total, and what remains is
    /// neither. Context and cost must come out unchanged by the split.
    #[test]
    fn cache_writes_are_reported_and_do_not_inflate_the_total() {
        let usage = wire_usage(&serde_json::json!({
            "input_tokens": 2_600,
            "input_tokens_details": {
                "cached_tokens": 2_000,
                "cache_write_tokens": 400
            }
        }));
        assert_eq!(usage.cache_read_input_tokens, 2_000);
        assert_eq!(usage.cache_creation_input_tokens, 400);
        assert_eq!(usage.input_tokens, 200);
        assert_eq!(usage.context_tokens(), 2_600);

        // A model that reports no writes still accounts the same way.
        let silent = wire_usage(&serde_json::json!({
            "input_tokens": 2_600,
            "input_tokens_details": {"cached_tokens": 2_000}
        }));
        assert_eq!(silent.cache_creation_input_tokens, 0);
        assert_eq!(silent.input_tokens, 600);
        assert_eq!(silent.context_tokens(), 2_600);
    }

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

    #[test]
    fn cached_input_accepts_chat_completions_usage_shape() {
        let usage = wire_usage(&serde_json::json!({
            "prompt_tokens": 1_800,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 1_500}
        }));
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_read_input_tokens, 1_500);
    }

    /// A replayed conversation is only cacheable if the server can
    /// rebuild the item graph it handed us, so items go back in the shape
    /// they came in: typed `message` items, and function calls carrying
    /// the item id that a preceding reasoning item refers to. Sending
    /// bare `{role, content}` pairs and anonymous calls left the backend
    /// synthesizing identity per request, and cache reads collapsed as
    /// soon as a step appended more than a couple of calls.
    #[test]
    fn replayed_items_keep_the_shape_and_identity_the_api_gave_them() {
        let message = wire_input_items(&ChatMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "on it".into(),
                },
                ContentBlock::ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "Cargo.toml"}),
                    item_id: Some("fc_1".into()),
                },
            ],
        });

        assert_eq!(message[0]["type"], "message");
        assert_eq!(message[0]["role"], "assistant");
        // Assistant text is `output_text`; user text is `input_text`.
        assert_eq!(message[0]["content"][0]["type"], "output_text");
        assert_eq!(message[0]["content"][0]["text"], "on it");
        assert_eq!(message[1]["type"], "function_call");
        assert_eq!(message[1]["id"], "fc_1");
        assert_eq!(message[1]["call_id"], "call_1");

        let user = wire_input_items(&ChatMessage::user_text("hello"));
        assert_eq!(user[0]["content"][0]["type"], "input_text");

        // A session recorded before item ids were captured still replays;
        // it simply has no id to send.
        let legacy = wire_input_items(&ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "call_2".into(),
                name: "read".into(),
                input: serde_json::json!({}),
                item_id: None,
            }],
        });
        assert!(legacy[0].get("id").is_none(), "{:?}", legacy[0]);
    }

    /// The call id pairs a call with its result; the item id names the
    /// call itself. Conflating them would break tool results.
    #[test]
    fn a_tool_result_still_references_the_call_id() {
        let items = wire_input_items(&ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "ok".into(),
                is_error: false,
            }],
        });
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "call_1");
        // Text output is a plain string; arrays are the multimodal form.
        assert_eq!(items[0]["output"], "ok");
    }

    /// `prompt_cache_key` influences routing but does not pin it. The
    /// Codex backend takes the conversation's identity from headers, and
    /// without them a request lands wherever — measured at 2/10 follow-up
    /// steps reading a cache, against 10/10 with them.
    #[test]
    fn the_codex_backend_gets_the_conversation_identity_as_headers() {
        let chatgpt = OpenAIProvider::with_chatgpt_auth(
            crate::auth::AuthStore::open(std::path::PathBuf::from("unused")),
            None,
        );
        assert_eq!(
            chatgpt.session_headers(Some("session-123")),
            vec![
                ("session-id", "session-123".to_string()),
                ("thread-id", "session-123".to_string()),
            ]
        );
        // Nothing to pin to.
        assert!(chatgpt.session_headers(None).is_empty());

        // The public API has no use for them, and a gateway may reject
        // headers it does not know — same rule as `prompt_cache_key`.
        let api_key = OpenAIProvider::new("test".into(), None);
        assert!(api_key.session_headers(Some("session-123")).is_empty());
        let gateway = OpenAIProvider::with_chatgpt_auth(
            crate::auth::AuthStore::open(std::path::PathBuf::from("unused")),
            Some("https://gateway.example/codex".into()),
        );
        assert!(gateway.session_headers(Some("session-123")).is_empty());
    }

    #[test]
    fn cache_key_is_mapped_to_openai_prompt_cache_key() {
        let provider = OpenAIProvider::new("test".into(), None);
        let mut request = Request::with_model("openai/gpt-5.2");
        request.cache_key = Some("session-123".into());

        let body = provider.wire_body(&request).unwrap();

        assert_eq!(body["prompt_cache_key"], "session-123");
    }

    /// The Codex backend accepts the field — a live probe established that
    /// much — and it is the only session-affinity lever the API offers.
    /// The probe could not measure a routing *benefit*, but a null result
    /// over two samples is not a reason to withhold it.
    #[test]
    fn chatgpt_backend_receives_the_prompt_cache_key() {
        let provider = OpenAIProvider::with_chatgpt_auth(
            crate::auth::AuthStore::open(std::path::PathBuf::from("unused")),
            None,
        );
        let mut request = Request::with_model("openai/gpt-5.2");
        request.cache_key = Some("session-123".into());

        let body = provider.wire_body(&request).unwrap();

        assert_eq!(body["prompt_cache_key"], "session-123");
    }

    /// Both auth paths follow one rule: the documented endpoint gets the
    /// field, a gateway that may reject unknown fields does not.
    #[test]
    fn a_custom_chatgpt_endpoint_omits_the_prompt_cache_key() {
        let provider = OpenAIProvider::with_chatgpt_auth(
            crate::auth::AuthStore::open(std::path::PathBuf::from("unused")),
            Some("https://gateway.example/codex".into()),
        );
        let mut request = Request::with_model("openai/gpt-5.2");
        request.cache_key = Some("session-123".into());

        let body = provider.wire_body(&request).unwrap();

        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn custom_openai_endpoint_does_not_receive_prompt_cache_key_by_default() {
        let provider =
            OpenAIProvider::new("test".into(), Some("https://gateway.example/v1".into()));
        let mut request = Request::with_model("openai/gpt-5.2");
        request.cache_key = Some("session-123".into());

        let body = provider.wire_body(&request).unwrap();

        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn consecutive_requests_keep_the_openai_prefix_byte_stable() {
        let provider = OpenAIProvider::new("test".into(), None);
        let mut first = Request::with_model("openai/gpt-5.2");
        first.system_prompt = Some("stable instructions".into());
        first.cache_key = Some("session-123".into());
        first.options = serde_json::json!({"reasoning": {"effort": "high"}});
        first.tools = vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        first.messages = vec![
            crate::session::ChatMessage::user_text("first"),
            crate::session::ChatMessage {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Reasoning {
                        item: serde_json::json!({
                            "type": "reasoning",
                            "id": "rs_1",
                            "encrypted_content": "synthetic"
                        }),
                    },
                    ContentBlock::ToolCall {
                        id: "call_1".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": "Cargo.toml"}),
                        item_id: None,
                    },
                ],
            },
            crate::session::ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "contents".into(),
                    is_error: false,
                }],
            },
        ];
        let mut second = first.clone();
        second
            .messages
            .push(crate::session::ChatMessage::user_text("second"));

        let first = provider.wire_body(&first).unwrap();
        let second = provider.wire_body(&second).unwrap();

        for key in [
            "model",
            "instructions",
            "tools",
            "reasoning",
            "prompt_cache_key",
        ] {
            assert_eq!(
                serde_json::to_vec(&first[key]).unwrap(),
                serde_json::to_vec(&second[key]).unwrap(),
                "unstable {key}"
            );
        }
        let first_input = first["input"].as_array().unwrap();
        let second_input = second["input"].as_array().unwrap();
        assert_eq!(
            serde_json::to_vec(&second_input[..first_input.len()]).unwrap(),
            serde_json::to_vec(first_input).unwrap()
        );
    }

    #[test]
    fn reasoning_summaries_are_requested_only_for_reasoning_models() {
        let supports = |id| crate::model::find(id).unwrap().reasoning_summaries;
        assert!(supports("openai/gpt-5.6-sol"));
        assert!(supports("openai/gpt-5.2"));
        assert!(supports("openai/o3"));
        assert!(!supports("openai/gpt-5.2-chat-latest"));
        assert!(!supports("openai/gpt-4o"));
    }

    #[test]
    fn explicit_reasoning_options_are_preserved() {
        let provider = OpenAIProvider::new("test".into(), None);
        let mut reasoning = Request::with_model("openai/gpt-5.2");
        reasoning.options = serde_json::json!({
            "reasoning": {"effort": "high", "summary": "detailed"}
        });
        let body = provider.wire_body(&reasoning).unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "detailed");

        let non_reasoning = provider
            .wire_body(&Request::with_model("openai/gpt-4o"))
            .unwrap();
        assert!(non_reasoning.get("reasoning").is_none());
    }
}
