//! OpenAI-compatible chat-completions wire (`POST {base_url}/chat/completions`).
//!
//! One implementation serves every endpoint that speaks this dialect:
//! z.ai's coding-plan endpoint (see [`super::zai`]), the OpenCode
//! gateways (see [`super::opencode`]) and the `[models.<name>]` entries
//! a user points at their own server. What
//! differs between them is [`ChatDialect`] and nothing else — the body,
//! the message building, the SSE mapping and the transport glue are
//! shared, so a fix to any of them lands on both.

use std::collections::HashMap;

use super::event::{ProviderEvent, StopReason};
use super::mapper::{MapperCore, MapperLabels, merge_usage};
use super::request::{Request, merge_options, reserved_conflicts as conflicts, resolve_model};
use super::transport::{self, Affinity, EventMapper as TransportEventMapper, TransportResponse};
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

/// How much of a model's thinking goes back to it on the wire.
///
/// `All` is the default and what OpenCode does: every assistant
/// message in the conversation carries its thinking, on the reading
/// that a model trained with its whole history of thought in context
/// is starved without it. `Turn` is the vendors' documented minimum —
/// the assistant messages after the last real prompt, where the Qwen
/// and GLM templates keep it and DeepSeek's docs say to drop the rest.
/// `Off` for a server that streams reasoning and refuses it as input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingReplay {
    #[default]
    All,
    Turn,
    Off,
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
    /// How much of the model's thinking goes back, where the
    /// configuration says. `None` is [`ThinkingReplay::All`] for a
    /// model the catalog says replays; a `[models.*]` or
    /// `[endpoints.*]` entry can narrow it to the current turn, or
    /// switch it off for a server that streams reasoning and refuses
    /// it as input — an OpenAI-strict validator rejects unknown
    /// assistant fields. Either way nothing goes back for a model the
    /// catalog says takes none.
    replay_thinking: Option<ThinkingReplay>,
    /// Body fields merged into every request for this endpoint
    /// (sampling: temperature, top_p …). Null when there are none.
    options: serde_json::Value,
    /// z.ai's `tool_stream`. Without it z.ai buffers the entire response
    /// server-side whenever tools are present (nothing streams until the
    /// whole turn is generated — verified against glm-5.3), which shows
    /// as minutes of dead air and gateway-timeout failures on long
    /// generations. No other server knows the field.
    tool_stream: bool,
    /// Headers the endpoint keys the conversation on, if any.
    affinity: Affinity,
}

impl ChatDialect {
    /// A cataloged, keyed endpoint: vision from the catalog row and the
    /// wire id straight from the ilar id. What varies is the prefix and
    /// whether the server knows z.ai's `tool_stream`.
    fn keyed(
        prefix: &'static str,
        api_key: String,
        base_url: String,
        tool_stream: bool,
        affinity: Affinity,
    ) -> Self {
        Self {
            prefix,
            base_url,
            api_key: Some(api_key),
            wire_model: None,
            vision: None,
            replay_thinking: None,
            options: serde_json::Value::Null,
            tool_stream,
            affinity,
        }
    }

    /// The z.ai endpoint, `tool_stream` on.
    pub(super) fn zai(api_key: String, base_url: String) -> Self {
        Self::keyed("zai", api_key, base_url, true, Affinity::None)
    }

    /// An OpenCode gateway (Zen or Go): the prefix is the gateway's, so
    /// one dialect serves both, none of z.ai's body fields are sent, and
    /// every request names its session.
    pub(super) fn opencode(prefix: &'static str, api_key: String, base_url: String) -> Self {
        Self::keyed(prefix, api_key, base_url, false, Affinity::OpenCode)
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
            replay_thinking: None,
            options: serde_json::Value::Null,
            tool_stream: false,
            affinity: Affinity::None,
        }
    }

    /// How much thinking to send back, when the configuration says;
    /// `None` leaves it at [`ThinkingReplay::All`] for a model the
    /// catalog says replays, which for a configured server is every one
    /// that streamed any.
    pub fn with_replay_thinking(mut self, replay: Option<ThinkingReplay>) -> Self {
        self.replay_thinking = replay;
        self
    }

    /// The same wire under another prefix: a discovered endpoint's
    /// models are addressed as `<endpoint>/<id>`, and the request path
    /// checks a model's prefix against its dialect's.
    pub fn with_prefix(mut self, prefix: &'static str) -> Self {
        self.prefix = prefix;
        self
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

    /// How much thinking goes back, from `[general]` — the answer for
    /// a dialect whose own configuration gave none. A `[models.*]` or
    /// `[endpoints.*]` entry that said so keeps its own; a keyed
    /// endpoint has no entry to say it in.
    pub fn with_thinking_replay(mut self, replay: ThinkingReplay) -> Self {
        self.dialect.replay_thinking.get_or_insert(replay);
        self
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
        // Thinking goes back only for a model that takes it — the
        // model check is the wire's own, not only the persist step's: a
        // log written before the split, or under another model, may
        // carry thinking this one must not see — and as much of it as
        // the configuration says. `Turn` is the assistant messages
        // after the last real prompt; a tool result rides a user-role
        // message here and is not a prompt.
        let mode = if crate::model::replays_thinking(&req.model) {
            self.dialect.replay_thinking.unwrap_or_default()
        } else {
            ThinkingReplay::Off
        };
        let last_prompt = req.messages.iter().rposition(is_prompt);
        // One spelling per request, and the newest the log holds: the
        // model answering now is the one that streamed the latest
        // thought, and a request that mixed two names — after a switch
        // from a `reasoning` model to a `reasoning_content` one — would
        // hand the new model a field it has never seen. The one request
        // right after such a switch still speaks the old name; the
        // first reply settles it.
        let spelling = req
            .messages
            .iter()
            .rev()
            .flat_map(|message| message.content.iter().rev())
            .find_map(|block| match block {
                ContentBlock::Thinking { field, .. } => Some(field.unwrap_or_default()),
                _ => None,
            })
            .unwrap_or_default();
        messages.extend(
            req.messages
                .iter()
                .enumerate()
                .flat_map(|(index, message)| {
                    let replay = match mode {
                        ThinkingReplay::All => true,
                        ThinkingReplay::Turn => last_prompt.is_some_and(|last| index > last),
                        ThinkingReplay::Off => false,
                    };
                    openai_message(message, vision, replay.then_some(spelling))
                }),
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
        for (name, value) in self.dialect.affinity.headers(req.cache_key.as_deref()) {
            request = request.header(name, value);
        }
        let request = request.json(&body).build()?;

        let http = self.http.clone();
        let secrets = self.dialect.api_key.clone().into_iter().collect::<Vec<_>>();
        let provider = self.dialect.prefix;
        let send = async move {
            let response = http
                .execute(request)
                .await
                .map_err(transport::request_error)?;
            Ok(TransportResponse {
                response,
                secrets,
                provider,
                credential: transport::Credential::ApiKey,
            })
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

/// A message the person (or a steer) sent, as opposed to a tool result,
/// which rides a user-role message on the neutral side.
fn is_prompt(msg: &ChatMessage) -> bool {
    msg.role == Role::User
        && msg
            .content
            .iter()
            .any(|block| !matches!(block, ContentBlock::ToolResult { .. }))
}

/// Neutral -> OpenAI chat-completions wire. Tool results expand into
/// separate `role: "tool"` messages (the wire format requires it). With
/// `replay_thinking` naming a field, an assistant message's thinking
/// goes along under it; which messages, and which field, the caller
/// decides (`replay_thinking` in the configuration, and the newest
/// spelling in the log).
fn openai_message(
    msg: &ChatMessage,
    vision: bool,
    replay_thinking: Option<crate::session::ReasoningField>,
) -> Vec<serde_json::Value> {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let mut content_text = String::new();
    let mut thinking = String::new();
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
            // Persisted as thinking only where the model takes it back
            // (`StepAccumulator::content_blocks`); a local diagnostic
            // is the thinking of a model that does not.
            ContentBlock::Thinking { text, .. } if replay_thinking.is_some() => {
                // One run of thought per block; several go back as
                // paragraphs rather than glued into one word.
                if !thinking.is_empty() {
                    thinking.push_str("\n\n");
                }
                thinking.push_str(text);
            }
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
    let with_thinking = |mut value: serde_json::Map<String, serde_json::Value>| {
        if let Some(field) = replay_thinking
            && !thinking.is_empty()
        {
            value.insert(field.name().into(), serde_json::json!(thinking));
        }
        serde_json::Value::Object(value)
    };
    if !tool_results.is_empty() {
        let mut messages = Vec::new();
        if !tool_calls.is_empty() {
            let mut value = serde_json::Map::new();
            value.insert("role".into(), serde_json::json!(role));
            value.insert("content".into(), serde_json::Value::Null);
            value.insert("tool_calls".into(), serde_json::json!(tool_calls));
            messages.push(with_thinking(value));
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
    vec![with_thinking(value)]
}

/// The neutral stop reason for a chat-completions `finish_reason`.
fn stop_reason_for(finish: &str) -> Result<StopReason, String> {
    Ok(match finish {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => {
            return Err(format!(
                "unknown OpenAI-compatible finish reason {finish:?}"
            ));
        }
    })
}

/// Whether a chat-completions delta says anything at all: a reasoning or
/// content fragment, or a tool-call fragment. Role and empty strings are
/// the wire clearing its throat, not a message. The fields are the ones
/// the mapper reads in `map`; a new spelling is added in both places.
fn carries_payload(delta: &serde_json::Value) -> bool {
    let non_empty = |field: &str| delta[field].as_str().is_some_and(|text| !text.is_empty());
    non_empty("reasoning_content")
        || non_empty("reasoning")
        || non_empty("content")
        || delta
            .get("tool_calls")
            .is_some_and(|calls| !calls.is_null())
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

/// What a piece of the content stream turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum Think {
    Thinking(String),
    /// The leading block closed; whatever follows is text.
    Ended,
    Text(String),
}

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// A `<think>…</think>` block at the head of the content stream.
///
/// MiniMax M3 (and older Qwen builds) put their reasoning in the
/// content itself rather than in a `reasoning_content` delta, so
/// without this a transcript opens with the model's notes to itself
/// and the fold that hides thinking never sees them. Only a *leading*
/// block counts: a model that quotes the tag later in its answer is
/// writing about it, not thinking out loud.
///
/// A tag may arrive split across deltas, so the undecided head is held
/// until it can be told apart — and, inside the block, a tail that
/// could still become `</think>` is held the same way.
#[derive(Debug, Default)]
struct LeadingThink {
    state: ThinkState,
    held: String,
    /// The block just closed and no answer has been seen yet: the
    /// blank line a model puts between its thinking and its answer is
    /// a separator, not the first line of the answer — and it may
    /// arrive in a delta of its own.
    opening: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum ThinkState {
    /// Nothing yet that says whether this stream opens with a block.
    #[default]
    Undecided,
    Inside,
    /// Decided, either way: the rest of the stream is text.
    Done,
}

impl LeadingThink {
    fn push(&mut self, delta: &str) -> Vec<Think> {
        let mut out = Vec::new();
        self.held.push_str(delta);
        if self.state == ThinkState::Undecided {
            let trimmed = self.held.trim_start();
            if let Some(rest) = trimmed.strip_prefix(OPEN) {
                self.state = ThinkState::Inside;
                self.held = rest.to_string();
            } else if trimmed.is_empty() || OPEN.starts_with(trimmed) {
                // Still could become the tag; nothing to say yet.
                return out;
            } else {
                self.state = ThinkState::Done;
                let head = std::mem::take(&mut self.held);
                out.extend(self.answer(head));
                return out;
            }
        }
        if self.state == ThinkState::Inside {
            match self.held.find(CLOSE) {
                Some(at) => {
                    let thought = self.held[..at].to_string();
                    let after = self.held[at + CLOSE.len()..].to_string();
                    self.held.clear();
                    self.state = ThinkState::Done;
                    self.opening = true;
                    if !thought.is_empty() {
                        out.push(Think::Thinking(thought));
                    }
                    out.push(Think::Ended);
                    out.extend(self.answer(after));
                }
                None => {
                    // Hold back only what could still open the close tag.
                    let keep = partial_tag_at_end(&self.held, CLOSE);
                    let thought: String = self.held[..self.held.len() - keep].to_string();
                    self.held = self.held[self.held.len() - keep..].to_string();
                    if !thought.is_empty() {
                        out.push(Think::Thinking(thought));
                    }
                }
            }
            return out;
        }
        let rest = std::mem::take(&mut self.held);
        out.extend(self.answer(rest));
        out
    }

    /// Text after the block, with the separator the model left between
    /// its thinking and its answer taken off the front. Line breaks
    /// only: an answer that opens with indented code keeps its indent.
    fn answer(&mut self, text: String) -> Option<Think> {
        let text = if self.opening {
            text.trim_start_matches(['\n', '\r']).to_string()
        } else {
            text
        };
        if text.is_empty() {
            return None;
        }
        self.opening = false;
        Some(Think::Text(text))
    }

    /// The content stream is over — a tool call, a finish, the end.
    /// Whatever is still held is what it is, tag-shaped or not.
    fn flush(&mut self) -> Vec<Think> {
        let held = std::mem::take(&mut self.held);
        let state = std::mem::replace(&mut self.state, ThinkState::Done);
        match state {
            ThinkState::Inside if held.is_empty() => vec![Think::Ended],
            ThinkState::Inside => vec![Think::Thinking(held), Think::Ended],
            _ => self.answer(held).into_iter().collect(),
        }
    }
}

/// How many bytes at the end of `text` are a proper prefix of `tag` —
/// what has to be held back because the next delta may complete it.
/// The whole tag is not a candidate: a complete one was already found.
fn partial_tag_at_end(text: &str, tag: &str) -> usize {
    (1..=tag.len().saturating_sub(1).min(text.len()))
        .rev()
        .find(|n| text.is_char_boundary(text.len() - n) && text[text.len() - n..] == tag[..*n])
        .unwrap_or(0)
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
    /// Whether the response's reasoning spelling has been reported: it
    /// is said once, on the first reasoning delta, and only when it is
    /// the `reasoning` spelling rather than the default.
    spelling_reported: bool,
    /// A model that thinks out loud in the content stream.
    leading_think: LeadingThink,
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
            spelling_reported: false,
            leading_think: LeadingThink::default(),
        }
    }

    /// Turn what the content stream amounted to into events, so a
    /// `<think>` block at its head lands where reasoning lands.
    fn push_content(&mut self, pieces: Vec<Think>, events: &mut Vec<ProviderEvent>) {
        for piece in pieces {
            match piece {
                Think::Thinking(text) => {
                    self.thinking_open = true;
                    events.push(ProviderEvent::ThinkingDelta(text));
                }
                Think::Ended => self.close_thinking(events),
                Think::Text(text) => {
                    self.close_thinking(events);
                    events.push(ProviderEvent::TextDelta(text));
                }
            }
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
        // Moonshot's Kimi (behind OpenCode Zen) sends its usage in a
        // trailing chunk that repeats the finish reason with an empty
        // delta. Nothing in it is content, so it is the usage chunk it is;
        // a chunk that carries content or a call after the finish is
        // still the protocol violation it always was.
        let choice = value["choices"].get(0);
        if let Some(stop_reason) = &self.stop_reason
            && let Some(choice) = choice
        {
            if carries_payload(&choice["delta"]) {
                return Err("OpenAI-compatible event arrived after finish_reason".into());
            }
            if let Some(finish) = choice["finish_reason"].as_str()
                && stop_reason_for(finish)? != *stop_reason
            {
                return Err("duplicate OpenAI-compatible finish reason".into());
            }
        } else if let Some(choice) = choice {
            let delta = &choice["delta"];
            // `reasoning_content` is the DeepSeek/z.ai spelling; the
            // OpenRouter-style servers behind OpenCode Zen (Kimi,
            // Nemotron, Ling) send the same deltas as `reasoning`.
            let reasoning = delta["reasoning_content"]
                .as_str()
                .map(|text| (text, crate::session::ReasoningField::ReasoningContent))
                .or_else(|| {
                    delta["reasoning"]
                        .as_str()
                        .map(|text| (text, crate::session::ReasoningField::Reasoning))
                });
            if let Some((reasoning, field)) = reasoning
                && !reasoning.is_empty()
            {
                // The name it came under is the name it goes back
                // under; said once, and only for the other spelling.
                if !self.spelling_reported {
                    self.spelling_reported = true;
                    if field != crate::session::ReasoningField::ReasoningContent {
                        events.push(ProviderEvent::ThinkingField(field));
                    }
                }
                self.thinking_open = true;
                events.push(ProviderEvent::ThinkingDelta(reasoning.into()));
            }
            if let Some(text) = delta["content"].as_str()
                && !text.is_empty()
            {
                let pieces = self.leading_think.push(text);
                self.push_content(pieces, &mut events);
            }
            // DeepSeek spells an absent field as `null` on every delta
            // (`"reasoning_content":null,"tool_calls":null` around a
            // content fragment); null is absence, not a malformed list.
            if !delta["tool_calls"].is_null() && !delta["tool_calls"].is_array() {
                return Err("OpenAI-compatible tool_calls must be an array".into());
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                // An empty list is not a call: some servers send one
                // beside ordinary content, and ending the content
                // stream on it would cut a tag in half.
                if !calls.is_empty() {
                    let pieces = self.leading_think.flush();
                    self.push_content(pieces, &mut events);
                    self.close_thinking(&mut events);
                }
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
                                    return Err(format!(
                                        "OpenAI-compatible tool call {index} named {name:?} \
                                         with no id: this server omits the call id the wire \
                                         uses to pair a call with its result"
                                    ));
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
                // A turn that stops mid-tag still said what it held.
                let pieces = self.leading_think.flush();
                self.push_content(pieces, &mut events);
                self.close_thinking(&mut events);
                let stop_reason = stop_reason_for(finish)?;
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
        let wire = openai_message(&image_message(), true, None);
        assert_eq!(wire.len(), 1);
        let content = wire[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );

        // No vision: plain string with the named gap.
        let wire = openai_message(&image_message(), false, None);
        let content = wire[0]["content"].as_str().unwrap();
        assert!(content.contains("[image omitted"), "{content}");

        // Text-only stays the plain string it always was.
        let wire = openai_message(&ChatMessage::user_text("hi"), true, None);
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

    /// A model that thinks in its content stream: the block at the head
    /// is reasoning, and only there — the same tag further along is the
    /// model writing about tags.
    #[test]
    fn a_leading_think_block_is_thinking_however_it_is_split() {
        let pieces = |deltas: &[&str]| {
            let mut think = LeadingThink::default();
            let mut all: Vec<Think> = deltas.iter().flat_map(|d| think.push(d)).collect();
            all.extend(think.flush());
            all
        };
        let thinking = |text: &str| Think::Thinking(text.into());
        let text = |t: &str| Think::Text(t.into());

        // The shape MiniMax M3 sends.
        assert_eq!(
            pieces(&["<think>plan", "</think>\n\nhi"]),
            [thinking("plan"), Think::Ended, text("hi")]
        );
        // Both tags split across deltas, one character at a time.
        let split: Vec<&str> = vec![
            "<", "th", "ink", ">", "a", "b", "<", "/th", "ink", ">", "answer",
        ];
        assert_eq!(
            pieces(&split),
            [thinking("a"), thinking("b"), Think::Ended, text("answer")]
        );
        // Leading whitespace before the tag, and an empty block.
        assert_eq!(
            pieces(&["\n<think></think>done"]),
            [Think::Ended, text("done")]
        );
        // The separator between thinking and answer is a separator
        // wherever it falls, including a delta of its own.
        assert_eq!(
            pieces(&["<think>plan", "</think>", "\n\n", "hi"]),
            [thinking("plan"), Think::Ended, text("hi")]
        );
        // No block at all: text is text, from the first delta on.
        assert_eq!(
            pieces(&["Hello", " there"]),
            [text("Hello"), text(" there")]
        );
        // A tag the model writes *about*, mid-answer, is left alone.
        assert_eq!(
            pieces(&["The tag ", "<think> is how", " it marks reasoning."]),
            [
                text("The tag "),
                text("<think> is how"),
                text(" it marks reasoning.")
            ]
        );
        // Something that starts like the tag and is not it.
        assert_eq!(
            pieces(&["<thinking about it>"]),
            [text("<thinking about it>")]
        );
        // A close tag with nothing open is a close tag the model typed.
        assert_eq!(pieces(&["</think>done"]), [text("</think>done")]);
        // An answer that opens with indented code keeps its indent:
        // only the line break between thought and answer comes off.
        assert_eq!(
            pieces(&["<think>plan</think>\n\n    indented"]),
            [thinking("plan"), Think::Ended, text("    indented")]
        );
        assert_eq!(partial_tag_at_end("", CLOSE), 0);
        assert_eq!(
            partial_tag_at_end("x</thi", CLOSE),
            5,
            "the longest partial"
        );
        assert_eq!(partial_tag_at_end("nothing", CLOSE), 0);
        // A turn that stops inside the block still says what it held.
        assert_eq!(
            pieces(&["<think>half a thought"]),
            [thinking("half a thought"), Think::Ended]
        );
        // And one that stops on a fragment that never became a tag.
        assert_eq!(pieces(&["<thi"]), [text("<thi")]);
    }

    /// The spelling a response's thinking arrives under is reported
    /// once, and only for the other one: `reasoning` is announced ahead
    /// of its first delta, `reasoning_content` announces nothing.
    #[test]
    fn the_reasoning_spelling_is_reported_once_and_only_when_it_differs() {
        use crate::session::ReasoningField;
        let usual =
            r#"{"choices":[{"index":0,"finish_reason":null,"delta":{"reasoning_content":"hm"}}]}"#;
        let again =
            r#"{"choices":[{"index":0,"finish_reason":null,"delta":{"reasoning_content":"m"}}]}"#;
        let done = r#"{"choices":[{"index":0,"finish_reason":"stop","delta":{}}]}"#;
        let events = openai_stream(&[usual, again, done]).unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ThinkingField(_))),
            "{events:?}"
        );

        let other = r#"{"choices":[{"index":0,"finish_reason":null,"delta":{"reasoning":"hm"}}]}"#;
        let more = r#"{"choices":[{"index":0,"finish_reason":null,"delta":{"reasoning":"m"}}]}"#;
        let events = openai_stream(&[other, more, done]).unwrap();
        assert_eq!(
            events[0],
            ProviderEvent::ThinkingField(ReasoningField::Reasoning)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderEvent::ThinkingField(_)))
                .count(),
            1,
            "{events:?}"
        );
    }

    /// DeepSeek spells every absent field as `null` on every delta.
    /// Seen live on deepseek-v4-flash behind OpenCode Go, 2026-09-18:
    /// the mapper read `"tool_calls":null` as a malformed list and
    /// failed a turn the model had answered.
    #[test]
    fn a_null_tool_calls_field_is_an_absent_one() {
        let chunk = r#"{"choices":[{"index":0,"finish_reason":null,"delta":{"role":null,"content":"Done.","reasoning_content":null,"tool_calls":null}}]}"#;
        let done = r#"{"choices":[{"index":0,"finish_reason":"stop","delta":{}}]}"#;
        let events = openai_stream(&[chunk, done]).expect("a content delta");
        assert!(
            matches!(&events[0], ProviderEvent::TextDelta(text) if text == "Done."),
            "{events:?}"
        );
        // A trailer that spells absence the same way is still a bare
        // trailer, not a violation.
        let trailer = r#"{"choices":[{"index":0,"finish_reason":"stop","delta":{"content":null,"reasoning_content":null,"tool_calls":null}}]}"#;
        openai_stream(&[chunk, trailer]).expect("a null-spelled trailer");
    }

    /// MiniMax M3 puts its reasoning in the content stream. On the wire
    /// it comes out where reasoning comes out, so the transcript opens
    /// with the answer and the fold hides the thinking.
    #[test]
    fn a_model_that_thinks_in_its_content_is_read_as_thinking() {
        let content = |text: &str| {
            format!(
                r#"{{"choices":[{{"index":0,"finish_reason":null,"delta":{{"content":"{text}"}}}}]}}"#
            )
        };
        let done = r#"{"choices":[{"index":0,"finish_reason":"stop","delta":{}}]}"#;
        let events = openai_stream(&[
            &content("<think>plan"),
            &content("</think>"),
            &content("\\n\\nhi"),
            done,
        ])
        .expect("a think block");
        assert!(
            matches!(&events[0], ProviderEvent::ThinkingDelta(text) if text == "plan"),
            "{events:?}"
        );
        assert_eq!(events[1], ProviderEvent::ThinkingCompleted, "{events:?}");
        assert!(
            matches!(&events[2], ProviderEvent::TextDelta(text) if text == "hi"),
            "{events:?}"
        );
        assert!(
            !events.iter().any(
                |event| matches!(event, ProviderEvent::TextDelta(text) if text.contains("think"))
            ),
            "the tag never reaches the transcript: {events:?}"
        );

        // Both tags split across deltas, on the wire.
        let split: Vec<String> = ["<th", "ink>plan", "</th", "ink>hi"]
            .iter()
            .map(|piece| content(piece))
            .collect();
        let mut chunks: Vec<&str> = split.iter().map(String::as_str).collect();
        chunks.push(done);
        let events = openai_stream(&chunks).expect("a split think block");
        assert!(
            matches!(&events[0], ProviderEvent::ThinkingDelta(text) if text == "plan"),
            "{events:?}"
        );
        assert_eq!(events[1], ProviderEvent::ThinkingCompleted, "{events:?}");
        assert!(
            matches!(&events[2], ProviderEvent::TextDelta(text) if text == "hi"),
            "{events:?}"
        );

        // A tool call ends the content stream: the block closes before
        // the call, and an empty `tool_calls` list does not end it.
        let empty_calls = r#"{"choices":[{"index":0,"finish_reason":null,"delta":{"content":null,"tool_calls":[]}}]}"#;
        let call = r#"{"choices":[{"index":0,"finish_reason":null,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{}"}}]}}]}"#;
        let stopped = r#"{"choices":[{"index":0,"finish_reason":"tool_calls","delta":{}}]}"#;
        let events = openai_stream(&[
            &content("<think>plan"),
            empty_calls,
            &content(" more"),
            call,
            stopped,
        ])
        .expect("a think block before a call");
        let thoughts: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ThinkingDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thoughts, ["plan", " more"], "{events:?}");
        let ended = events
            .iter()
            .position(|event| event == &ProviderEvent::ThinkingCompleted);
        let started = events
            .iter()
            .position(|event| matches!(event, ProviderEvent::ToolCallStarted { .. }));
        assert!(
            ended < started,
            "thinking closes before the call: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::TextDelta(_))),
            "an unclosed block never leaks as text: {events:?}"
        );
    }

    /// By default thinking goes back on every assistant message, the
    /// way OpenCode sends it; `turn` narrows that to the assistant
    /// messages after the last real prompt, and `off` sends none. A
    /// tool result is not a prompt: it rides a user-role message here,
    /// and counting it would strip the thinking of the very step that
    /// produced the call. A block that arrived as `reasoning` goes back
    /// as `reasoning`.
    #[test]
    fn thinking_goes_back_as_configured_and_under_its_own_name() {
        let thought = |text: &str, rest: Vec<ContentBlock>| ChatMessage {
            role: Role::Assistant,
            content: std::iter::once(ContentBlock::Thinking {
                text: text.into(),
                field: None,
            })
            .chain(rest)
            .collect(),
        };
        let call = ContentBlock::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "x"}),
            item_id: None,
        };
        let result = ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "the file".into(),
                is_error: false,
                images: Vec::new(),
            }],
        };
        let text = |text: &str| ContentBlock::Text { text: text.into() };
        let mut request = Request::with_model("zai/glm-4.7");
        request.messages = vec![
            ChatMessage::user_text("earlier"),
            thought("old plan", vec![text("done earlier")]),
            ChatMessage::user_text("now"),
            thought("plan", vec![call.clone()]),
            result.clone(),
            thought("more", vec![text("answer")]),
        ];
        let dialect = || ChatDialect::zai("k".into(), "http://zai.test".into());
        let assistants = |body: &serde_json::Value| -> Vec<serde_json::Value> {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "assistant")
                .cloned()
                .collect()
        };

        // The default: whole history.
        let all = assistants(&ChatProvider::new(dialect()).wire_body_for_test(&request));
        assert_eq!(all.len(), 3);
        assert_eq!(all[0]["reasoning_content"], "old plan");
        assert_eq!(all[1]["reasoning_content"], "plan");
        assert_eq!(all[1]["tool_calls"][0]["id"], "c1");
        assert_eq!(all[2]["reasoning_content"], "more");
        assert_eq!(all[2]["content"], "answer");

        // `turn`: the current turn only.
        let turn = ChatProvider::new(dialect().with_replay_thinking(Some(ThinkingReplay::Turn)));
        let turn = assistants(&turn.wire_body_for_test(&request));
        assert!(
            turn[0].get("reasoning_content").is_none(),
            "an earlier turn's thinking went back under `turn`: {}",
            turn[0]
        );
        assert_eq!(turn[1]["reasoning_content"], "plan");
        assert_eq!(turn[2]["reasoning_content"], "more");

        // `off`: none, however the log reads.
        let off = ChatProvider::new(dialect().with_replay_thinking(Some(ThinkingReplay::Off)));
        let off = off.wire_body_for_test(&request);
        assert!(
            !off["messages"].to_string().contains("reasoning"),
            "{}",
            off["messages"]
        );

        // Two runs of thought in one message go back as paragraphs, and
        // the whole request speaks the newest spelling in the log.
        request.messages[5] = ChatMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "more".into(),
                    field: Some(crate::session::ReasoningField::Reasoning),
                },
                text("between"),
                ContentBlock::Thinking {
                    text: "still more".into(),
                    field: Some(crate::session::ReasoningField::Reasoning),
                },
                text("answer"),
            ],
        };
        let provider = ChatProvider::new(dialect());
        let body = provider.wire_body_for_test(&request);
        let last = body["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["reasoning"], "more\n\nstill more");
        assert!(last.get("reasoning_content").is_none(), "{last}");
        let first = &body["messages"].as_array().unwrap()[1];
        assert_eq!(first["role"], "assistant");
        assert_eq!(first["reasoning"], "old plan", "{first}");

        // The same conversation with the thinking already made local —
        // what a model that takes none back persists — sends none.
        let local = |message: &ChatMessage| ChatMessage {
            role: message.role,
            content: message
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Thinking { text, .. } => ContentBlock::Diagnostic {
                        text: text.clone(),
                        kind: crate::session::DiagnosticKind::Local,
                    },
                    block => block.clone(),
                })
                .collect(),
        };
        request.messages = request.messages.iter().map(local).collect();
        let body = provider.wire_body_for_test(&request);
        assert!(
            !body["messages"].to_string().contains("reasoning_content"),
            "{}",
            body["messages"]
        );
    }
}
