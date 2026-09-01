//! One user turn: provider call(s) + tool execution until the model
//! stops calling tools. Pure state machine — persists via the session
//! store, publishes to the event channel, never touches a UI.

use anyhow::Result;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

/// The turn declined before appending anything to the session.
///
/// `run_turn` has fallible steps ahead of the prompt append — the writer
/// acquire, variant options, provider resolution, the pre-prompt event
/// appends — and a caller that folded queued steer messages into the
/// prompt needs to know whether that prompt ever reached the log. An
/// error carrying this marker provably appended nothing: the caller may
/// restore its queue instead of counting the messages as delivered.
/// Attached as anyhow context, so downcasting to the underlying error —
/// the `io::Error` `WouldBlock` of a held writer lease in particular —
/// keeps working through it.
#[derive(Debug)]
pub struct TurnNeverStarted(anyhow::Error);

impl TurnNeverStarted {
    /// Mark an error from the pre-append stretch of `run_turn`. A
    /// transparent wrapper, not a context layer: it displays as the
    /// error it wraps and its `source` skips that error's own display
    /// layer, so neither `{error}` nor `{error:#}` ever shows the
    /// bookkeeping — or the same message twice. The skipped layer may
    /// BE the typed root (a bare io::Error), so cause classification
    /// goes through [`TurnNeverStarted::causes`], never the outer
    /// chain.
    fn mark(error: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(TurnNeverStarted(error))
    }

    /// The wrapped error's full cause chain, for callers that
    /// classify what declined the turn (the router's WouldBlock
    /// retry).
    pub fn causes(&self) -> impl Iterator<Item = &(dyn std::error::Error + 'static)> {
        self.0.chain()
    }
}

impl std::fmt::Display for TurnNeverStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TurnNeverStarted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Skip the wrapped error's own display layer — this wrapper
        // already showed it — and continue with its causes.
        self.0.chain().nth(1)
    }
}

use crate::agent::event::{LoopEvent, LoopEventSender};
use crate::provider::{ProviderEvent, ProviderResolver, Request, StopReason};
use crate::session::{ContentBlock, DiagnosticKind, SessionEvent, SessionStore, Usage, new_id};
use crate::tools::ToolRegistry;
use crate::tools::executor::{CallOutcome, ToolCall, execute_calls_observed};
use chrono::Utc;

/// A message the user sends while a turn is running, delivered to the
/// model at the next step boundary rather than after the turn ends.
///
/// Text and its attachments travel as one unit, for the same reason a
/// stashed prompt does: steering is exactly when a screenshot is most
/// useful — "no, look at this" — and delivering the words without the
/// picture would be a message the model cannot act on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Steer {
    pub text: String,
    pub images: Vec<crate::session::ImageContent>,
}

impl Steer {
    /// Nothing for the model to read. An attachment counts as content,
    /// so only a blank message with nothing attached is blank.
    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }
}

impl From<String> for Steer {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl From<&str> for Steer {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

/// Unbounded on purpose: a steer that blocks the UI thread would defeat
/// the point, and the volume is bounded by how fast a person types.
pub type SteerSender = tokio::sync::mpsc::UnboundedSender<Steer>;
pub type SteerReceiver = tokio::sync::mpsc::UnboundedReceiver<Steer>;

pub fn steer_channel() -> (SteerSender, SteerReceiver) {
    tokio::sync::mpsc::unbounded_channel()
}

/// The session's most recent compaction, if it has ever been compacted.
/// Only its identity matters (see
/// [`crate::tools::SeenFiles::forget_after_compaction`]) — and unlike a
/// count it survives the replay checkpoint, which keeps only the last
/// compaction in a loaded session's window.
fn last_compaction(session: &crate::session::Session) -> Option<&str> {
    session.events().iter().rev().find_map(|event| match event {
        SessionEvent::Compaction { id, .. } => Some(id.as_str()),
        _ => None,
    })
}

/// Take everything pending without waiting.
fn drain_steers(steer: Option<&mut SteerReceiver>) -> Vec<Steer> {
    let mut pending = Vec::new();
    if let Some(steer) = steer {
        while let Ok(message) = steer.try_recv() {
            if !message.is_blank() {
                pending.push(message);
            }
        }
    }
    pending
}

/// Loop tuning knobs.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// Max provider calls per user turn. A runaway-loop backstop, not a
    /// working limit — long-thinking models (glm-5.3 at max effort)
    /// legitimately need hundreds on big tasks.
    pub max_iterations: usize,
    /// Number of transient provider failures retried before surfacing the
    /// final error. Retries only happen before response content arrives.
    pub max_provider_retries: usize,
    /// Initial retry delay. Each subsequent retry doubles this delay.
    pub provider_retry_base_delay: std::time::Duration,
    /// Upper bound for any individual exponential-backoff delay.
    pub provider_retry_max_delay: std::time::Duration,
    /// Context window in tokens; compaction triggers above
    /// `context_limit * compaction_threshold`. None uses the resolver's
    /// model-specific default, or disables compaction if it has none.
    pub context_limit: Option<u64>,
    pub compaction_threshold: f64,
    /// Compact before this turn regardless of the threshold (user-requested).
    pub force_compaction: bool,
    /// How often a turn waiting on tools touches its live scratch, so a
    /// supervisor can tell a long tool run from a dead process. A value
    /// rather than the constant so a test can observe the behaviour in
    /// milliseconds instead of sleeping through the shipped interval —
    /// the same trade `serve`'s `WatchConfig` makes with its polls.
    pub live_heartbeat: std::time::Duration,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: 1_000,
            max_provider_retries: 3,
            provider_retry_base_delay: std::time::Duration::from_millis(500),
            provider_retry_max_delay: std::time::Duration::from_secs(30),
            context_limit: None,
            compaction_threshold: 0.85,
            force_compaction: false,
            live_heartbeat: crate::session::SCRATCH_HEARTBEAT,
        }
    }
}

/// How a user turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Model finished with no tool calls.
    Completed,
    /// User aborted (Esc): stream cancelled, running tools cancelled,
    /// partial transcript persisted.
    Aborted,
    /// Hit the max-iterations guard.
    MaxIterations,
}

/// Accumulated blocks from one provider call.
#[derive(Default)]
struct StepAccumulator {
    content: Vec<ContentBlock>,
    thinking_open: Option<usize>,
    reasoning_summary_open: Option<usize>,
    tool_indices: std::collections::HashMap<String, usize>,
    completed_calls: std::collections::HashSet<String>,
    /// Tool-call ids that already got a ToolStarted announcement.
    announced_calls: std::collections::HashMap<String, String>,
    tool_input_scanners: std::collections::HashMap<String, PartialPathInput>,
    tool_received_bytes: std::collections::HashMap<String, u64>,
    published_arguments: std::collections::HashMap<String, String>,
    usage: Usage,
    stop_reason: Option<StopReason>,
}

impl StepAccumulator {
    fn content_blocks(&self) -> Vec<ContentBlock> {
        self.content
            .iter()
            .filter_map(|block| match block {
                // Thinking is never replayed to a provider, so it is
                // persisted as what it is to a reader: a diagnostic.
                ContentBlock::Thinking { text } => Some(ContentBlock::Diagnostic {
                    text: text.clone(),
                    kind: DiagnosticKind::Local,
                }),
                ContentBlock::ReasoningSummary {
                    completed: false, ..
                } => None,
                block => Some(block.clone()),
            })
            .collect()
    }

    fn push_text(&mut self, delta: String) {
        self.thinking_open = None;
        match self.content.last_mut() {
            Some(ContentBlock::Text { text }) => text.push_str(&delta),
            _ => self.content.push(ContentBlock::Text { text: delta }),
        }
    }

    fn push_thinking(&mut self, delta: String) {
        let index = match self.thinking_open {
            Some(index) => index,
            None => {
                self.content.push(ContentBlock::Thinking {
                    text: String::new(),
                });
                let index = self.content.len() - 1;
                self.thinking_open = Some(index);
                index
            }
        };
        if let ContentBlock::Thinking { text, .. } = &mut self.content[index] {
            text.push_str(&delta);
        }
    }

    /// Closes the open thinking block: the next thinking delta starts a
    /// new one rather than extending this thought.
    fn complete_thinking(&mut self) {
        self.thinking_open = None;
    }

    fn push_reasoning_summary(&mut self, delta: String) {
        self.thinking_open = None;
        let index = match self.reasoning_summary_open {
            Some(index) => index,
            None => {
                self.content.push(ContentBlock::ReasoningSummary {
                    text: String::new(),
                    completed: false,
                });
                let index = self.content.len() - 1;
                self.reasoning_summary_open = Some(index);
                index
            }
        };
        if let ContentBlock::ReasoningSummary { text, .. } = &mut self.content[index] {
            text.push_str(&delta);
        }
    }

    fn complete_reasoning_summary(&mut self) {
        if let Some(index) = self.reasoning_summary_open.take()
            && let ContentBlock::ReasoningSummary { completed, .. } = &mut self.content[index]
        {
            *completed = true;
        }
    }

    fn push_reasoning(&mut self, item: serde_json::Value) {
        self.thinking_open = None;
        self.content.push(ContentBlock::Reasoning { item });
    }

    fn start_tool_call(
        &mut self,
        id: String,
        name: String,
        item_id: Option<String>,
    ) -> Result<(), String> {
        self.thinking_open = None;
        if id.is_empty() || name.is_empty() {
            return Err("tool call id and name must not be empty".into());
        }
        if self.tool_indices.contains_key(&id) {
            return Err(format!("duplicate tool call id {id:?}"));
        }
        self.content.push(ContentBlock::ToolCall {
            id: id.clone(),
            name,
            input: serde_json::Value::Null,
            item_id,
        });
        self.tool_indices.insert(id, self.content.len() - 1);
        Ok(())
    }

    fn complete_tool_call(
        &mut self,
        id: String,
        name: String,
        input: serde_json::Value,
    ) -> Result<(), String> {
        if self.completed_calls.contains(&id) {
            return Err(format!("duplicate completion for tool call {id:?}"));
        }
        if !input.is_null() && !input.is_object() {
            return Err(format!(
                "tool call {id:?} arguments must be an object or null"
            ));
        }
        if let Some(index) = self.tool_indices.get(&id).copied() {
            if let ContentBlock::ToolCall {
                name: started_name, ..
            } = &self.content[index]
                && started_name != &name
            {
                return Err(format!("tool call {id:?} changed name before completion"));
            }
        } else {
            return Err(format!("completion references unknown tool call {id:?}"));
        }
        if let Some(index) = self.tool_indices.get(&id).copied() {
            let completed_id = id.clone();
            // The item id arrives with the announcement, not the
            // completion, so completion must not drop it.
            let item_id = match &self.content[index] {
                ContentBlock::ToolCall { item_id, .. } => item_id.clone(),
                _ => None,
            };
            self.content[index] = ContentBlock::ToolCall {
                id,
                name,
                input,
                item_id,
            };
            self.completed_calls.insert(completed_id);
        }
        Ok(())
    }

    fn push_tool_input_delta(&mut self, id: &str, delta: &str) -> Result<ToolInputUpdate, String> {
        if id.is_empty() || !self.tool_indices.contains_key(id) || self.completed_calls.contains(id)
        {
            return Err(format!(
                "tool argument delta references unknown call {id:?}"
            ));
        }
        let received = self.tool_received_bytes.entry(id.to_string()).or_default();
        *received = received.saturating_add(delta.len() as u64);

        // The scanner looks for a streamed `path`, which is exactly the
        // summary these two tools publish; every other tool summarises
        // other keys and gets its arguments at completion.
        let previewable = matches!(
            self.announced_calls.get(id).map(String::as_str),
            Some("write" | "edit")
        );
        if !previewable || self.published_arguments.contains_key(id) {
            return Ok(ToolInputUpdate {
                arguments: None,
                received_bytes: *received,
            });
        }

        let scanner = self.tool_input_scanners.entry(id.to_string()).or_default();
        let Some(path) = scanner.push(delta) else {
            return Ok(ToolInputUpdate {
                arguments: None,
                received_bytes: *received,
            });
        };
        self.tool_input_scanners.remove(id);
        let name = self
            .announced_calls
            .get(id)
            .cloned()
            .expect("previewable implies an announced call");
        let arguments = summarize_tool_input(&name, &serde_json::json!({"path": path}));
        self.published_arguments
            .insert(id.to_string(), arguments.clone());
        Ok(ToolInputUpdate {
            arguments: Some(arguments),
            received_bytes: *received,
        })
    }

    fn arguments_changed(&mut self, id: &str, arguments: &str) -> bool {
        if self.published_arguments.get(id).map(String::as_str) == Some(arguments) {
            return false;
        }
        self.published_arguments
            .insert(id.to_string(), arguments.to_string());
        true
    }

    fn validate_terminal(&self, stop_reason: &StopReason) -> Result<(), String> {
        let has_calls = !self.tool_indices.is_empty();
        let calls = self.tool_calls();
        if calls.iter().any(|(_, _, _, completed)| !*completed) {
            return Err("terminal response contains uncompleted tool calls".into());
        }
        if stop_reason != &StopReason::MaxTokens
            && calls.iter().any(|(_, _, input, _)| input.is_null())
        {
            return Err("null tool arguments require a max_tokens stop reason".into());
        }
        match stop_reason {
            StopReason::ToolUse if !has_calls => {
                Err("tool_use stop reason requires at least one tool call".into())
            }
            StopReason::ToolUse
                if self.completed_calls.len() != self.tool_indices.len()
                    || self
                        .tool_calls()
                        .iter()
                        .any(|(_, _, input, _)| !input.is_object()) =>
            {
                Err("tool_use stop reason requires completed object arguments".into())
            }
            StopReason::EndTurn | StopReason::Refusal | StopReason::Stopped if has_calls => Err(
                format!("{stop_reason:?} stop reason contradicts streamed tool calls"),
            ),
            _ => Ok(()),
        }
    }

    fn tool_calls(&self) -> Vec<(&String, &String, &serde_json::Value, bool)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall {
                    id, name, input, ..
                } => Some((id, name, input, self.completed_calls.contains(id))),
                _ => None,
            })
            .collect()
    }
}

/// Persist a step that ended in failure the way the provider-error path
/// does: whatever the user already watched stream, with the failure
/// recorded as a diagnostic, then a synthetic error result for every
/// announced tool call — an unanswered tool_use poisons the transcript.
/// Ends with the reserved terminal event, so a consumer watching only the
/// channel sees the turn end rather than a bare close; the caller then
/// returns the error.
async fn persist_failed_step(
    session: &mut crate::session::Session,
    events: &mut LoopEventSender,
    cancel: &CancellationToken,
    model: &str,
    acc: &StepAccumulator,
    usage: Usage,
    message: &str,
) -> Result<()> {
    let mut blocks = acc.content_blocks();
    blocks.push(ContentBlock::Diagnostic {
        text: format!("turn error: {message}"),
        kind: DiagnosticKind::TurnError,
    });
    session.append(SessionEvent::AssistantMessage {
        id: new_id(),
        model: model.to_string(),
        content: blocks,
        usage,
        stop_reason: "error".into(),
        ts: Utc::now(),
    })?;
    let result = format!("provider error before execution: {message}");
    // Every announced call is one of these: the announcement only
    // happens after `start_tool_call` pushed the block this reads back,
    // and nothing removes a block. So answering the accumulated calls
    // answers everything the frontend was told about.
    let calls = acc.tool_calls();
    for (id, name, _, _) in &calls {
        session.append(SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: (*id).clone(),
            content: result.clone(),
            is_error: true,
            images: Vec::new(),
            child_session_id: None,
            state: None,
            ts: Utc::now(),
        })?;
        events
            .publish(
                LoopEvent::ToolFinished {
                    id: (*id).clone(),
                    name: (*name).clone(),
                    is_error: true,
                    result: crate::text::bounded_detail(&result),
                    child_session_id: None,
                },
                cancel,
            )
            .await;
    }
    // A failed turn ends like an aborted one for anybody watching the
    // channel — the same outcome every caller synthesizes from the
    // error it is about to get back.
    events.publish_terminal(LoopEvent::TurnDone {
        outcome: TurnOutcome::Aborted,
    });
    Ok(())
}

const MAX_TOOL_ARGUMENT_SUMMARY_CHARS: usize = 512;
const MAX_STREAMED_PATH_BYTES: usize = 4 * 1024;
const MAX_STREAMED_JSON_DEPTH: usize = 64;

#[derive(Default)]
struct PartialPathInput {
    state: PartialJsonState,
}

struct ToolInputUpdate {
    arguments: Option<String>,
    received_bytes: u64,
}

enum ToolExecutionLifecycle {
    Started {
        id: String,
        started: std::time::Instant,
    },
    Completed {
        id: String,
    },
}

/// Record the tool a step is now running in the live scratch. Only the
/// start: the finish rides with the result, which is where the turn
/// already knows whether it worked.
fn note_execution_start(
    live: &mut crate::session::LiveScratch,
    lifecycle: &ToolExecutionLifecycle,
    executing: &std::collections::HashMap<String, (String, String)>,
) {
    if let ToolExecutionLifecycle::Started { id, .. } = lifecycle
        && let Some((name, summary)) = executing.get(id)
    {
        live.tool_started(id, name, summary);
    }
}

fn tool_execution_loop_event(
    lifecycle: ToolExecutionLifecycle,
    received_bytes: &std::collections::HashMap<String, u64>,
) -> LoopEvent {
    match lifecycle {
        ToolExecutionLifecycle::Started { id, started } => LoopEvent::ToolExecutionStarted {
            received_bytes: received_bytes.get(&id).copied().unwrap_or(0),
            id,
            started,
        },
        ToolExecutionLifecycle::Completed { id } => LoopEvent::ToolExecutionCompleted { id },
    }
}

#[derive(Default)]
enum PartialJsonState {
    #[default]
    Start,
    BeforeKey,
    Key {
        raw: Vec<u8>,
        escaped: bool,
        overflow: bool,
    },
    AfterKey {
        wanted: bool,
    },
    BeforeValue {
        wanted: bool,
    },
    Path {
        raw: Vec<u8>,
        escaped: bool,
        overflow: bool,
    },
    Skip {
        stack: Vec<u8>,
        in_string: bool,
        escaped: bool,
        scalar: bool,
    },
    AfterValue,
    Done,
    Invalid,
}

impl PartialPathInput {
    fn push(&mut self, delta: &str) -> Option<String> {
        for &byte in delta.as_bytes() {
            let state = std::mem::replace(&mut self.state, PartialJsonState::Invalid);
            self.state = match state {
                PartialJsonState::Start if byte.is_ascii_whitespace() => PartialJsonState::Start,
                PartialJsonState::Start if byte == b'{' => PartialJsonState::BeforeKey,
                PartialJsonState::BeforeKey if byte.is_ascii_whitespace() => {
                    PartialJsonState::BeforeKey
                }
                PartialJsonState::BeforeKey if byte == b'}' => PartialJsonState::Done,
                PartialJsonState::BeforeKey if byte == b'"' => PartialJsonState::Key {
                    raw: vec![byte],
                    escaped: false,
                    overflow: false,
                },
                PartialJsonState::Key {
                    mut raw,
                    escaped,
                    mut overflow,
                } => {
                    push_bounded_byte(&mut raw, byte, &mut overflow);
                    if escaped {
                        PartialJsonState::Key {
                            raw,
                            escaped: false,
                            overflow,
                        }
                    } else if byte == b'\\' {
                        PartialJsonState::Key {
                            raw,
                            escaped: true,
                            overflow,
                        }
                    } else if byte == b'"' {
                        let wanted = !overflow
                            && serde_json::from_slice::<String>(&raw)
                                .is_ok_and(|key| key == "path");
                        PartialJsonState::AfterKey { wanted }
                    } else {
                        PartialJsonState::Key {
                            raw,
                            escaped: false,
                            overflow,
                        }
                    }
                }
                PartialJsonState::AfterKey { wanted } if byte.is_ascii_whitespace() => {
                    PartialJsonState::AfterKey { wanted }
                }
                PartialJsonState::AfterKey { wanted } if byte == b':' => {
                    PartialJsonState::BeforeValue { wanted }
                }
                PartialJsonState::BeforeValue { wanted } if byte.is_ascii_whitespace() => {
                    PartialJsonState::BeforeValue { wanted }
                }
                PartialJsonState::BeforeValue { wanted: true } if byte == b'"' => {
                    PartialJsonState::Path {
                        raw: vec![byte],
                        escaped: false,
                        overflow: false,
                    }
                }
                PartialJsonState::BeforeValue { .. } => skip_value(byte),
                PartialJsonState::Path {
                    mut raw,
                    escaped,
                    mut overflow,
                } => {
                    push_bounded_byte(&mut raw, byte, &mut overflow);
                    if escaped {
                        PartialJsonState::Path {
                            raw,
                            escaped: false,
                            overflow,
                        }
                    } else if byte == b'\\' {
                        PartialJsonState::Path {
                            raw,
                            escaped: true,
                            overflow,
                        }
                    } else if byte == b'"' {
                        if !overflow && let Ok(path) = serde_json::from_slice::<String>(&raw) {
                            self.state = PartialJsonState::Done;
                            return Some(path);
                        }
                        PartialJsonState::AfterValue
                    } else {
                        PartialJsonState::Path {
                            raw,
                            escaped: false,
                            overflow,
                        }
                    }
                }
                PartialJsonState::Skip {
                    stack,
                    in_string: true,
                    escaped,
                    scalar,
                } => {
                    if escaped {
                        PartialJsonState::Skip {
                            stack,
                            in_string: true,
                            escaped: false,
                            scalar,
                        }
                    } else if byte == b'\\' {
                        PartialJsonState::Skip {
                            stack,
                            in_string: true,
                            escaped: true,
                            scalar,
                        }
                    } else if byte == b'"' {
                        if stack.is_empty() {
                            PartialJsonState::AfterValue
                        } else {
                            PartialJsonState::Skip {
                                stack,
                                in_string: false,
                                escaped: false,
                                scalar,
                            }
                        }
                    } else {
                        PartialJsonState::Skip {
                            stack,
                            in_string: true,
                            escaped: false,
                            scalar,
                        }
                    }
                }
                PartialJsonState::Skip {
                    stack,
                    in_string: false,
                    escaped: false,
                    scalar: true,
                } => match byte {
                    b',' if stack.is_empty() => PartialJsonState::BeforeKey,
                    b'}' if stack.is_empty() => PartialJsonState::Done,
                    _ => PartialJsonState::Skip {
                        stack,
                        in_string: false,
                        escaped: false,
                        scalar: true,
                    },
                },
                PartialJsonState::Skip {
                    mut stack,
                    in_string: false,
                    escaped: false,
                    scalar: false,
                } => match byte {
                    b'"' => PartialJsonState::Skip {
                        stack,
                        in_string: true,
                        escaped: false,
                        scalar: false,
                    },
                    b'{' if stack.len() < MAX_STREAMED_JSON_DEPTH => {
                        stack.push(b'}');
                        PartialJsonState::Skip {
                            stack,
                            in_string: false,
                            escaped: false,
                            scalar: false,
                        }
                    }
                    b'[' if stack.len() < MAX_STREAMED_JSON_DEPTH => {
                        stack.push(b']');
                        PartialJsonState::Skip {
                            stack,
                            in_string: false,
                            escaped: false,
                            scalar: false,
                        }
                    }
                    b'{' | b'[' => PartialJsonState::Invalid,
                    closing if stack.last() == Some(&closing) => {
                        stack.pop();
                        if stack.is_empty() {
                            PartialJsonState::AfterValue
                        } else {
                            PartialJsonState::Skip {
                                stack,
                                in_string: false,
                                escaped: false,
                                scalar: false,
                            }
                        }
                    }
                    b'}' | b']' => PartialJsonState::Invalid,
                    _ => PartialJsonState::Skip {
                        stack,
                        in_string: false,
                        escaped: false,
                        scalar: false,
                    },
                },
                PartialJsonState::AfterValue if byte.is_ascii_whitespace() => {
                    PartialJsonState::AfterValue
                }
                PartialJsonState::AfterValue if byte == b',' => PartialJsonState::BeforeKey,
                PartialJsonState::AfterValue if byte == b'}' => PartialJsonState::Done,
                PartialJsonState::Done => PartialJsonState::Done,
                PartialJsonState::Invalid => PartialJsonState::Invalid,
                _ => PartialJsonState::Invalid,
            };
        }
        None
    }
}

fn push_bounded_byte(raw: &mut Vec<u8>, byte: u8, overflow: &mut bool) {
    if raw.len() < MAX_STREAMED_PATH_BYTES {
        raw.push(byte);
    } else {
        *overflow = true;
    }
}

fn skip_value(byte: u8) -> PartialJsonState {
    match byte {
        b'"' => PartialJsonState::Skip {
            stack: Vec::new(),
            in_string: true,
            escaped: false,
            scalar: false,
        },
        b'{' => PartialJsonState::Skip {
            stack: vec![b'}'],
            in_string: false,
            escaped: false,
            scalar: false,
        },
        b'[' => PartialJsonState::Skip {
            stack: vec![b']'],
            in_string: false,
            escaped: false,
            scalar: false,
        },
        _ => PartialJsonState::Skip {
            stack: Vec::new(),
            in_string: false,
            escaped: false,
            scalar: true,
        },
    }
}

/// Bounded, redacted tool input summary suitable for persisted UI replay.
pub fn summarize_tool_input(name: &str, input: &serde_json::Value) -> String {
    let string = |key: &str| {
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(collapse_whitespace)
    };
    let summary = match name {
        // Whether it was detached is part of what ran: a background
        // command that summarised like a foreground one read as a turn
        // waiting on something it had already let go of.
        "bash" => string("command").map(|command| {
            let command = redact_command(&command);
            match input
                .get("run_in_background")
                .and_then(serde_json::Value::as_bool)
            {
                // Ahead of the command, not behind it: the summary is
                // capped, and a long command would eat the one word
                // this arm exists to add.
                Some(true) => format!("background · {command}"),
                _ => command,
            }
        }),
        // Which service, not just which verb: four actions read the
        // same when the name is missing.
        "service" => {
            let action = string("action").unwrap_or_else(|| "service".into());
            let command = string("command").map(|command| redact_command(&command));
            Some(match (string("name"), command) {
                (Some(name), Some(command)) => format!("{action} {name} · {command}"),
                (Some(name), None) => format!("{action} {name}"),
                (None, Some(command)) => format!("{action} · {command}"),
                (None, None) => action,
            })
        }
        // The id first: it names the task, and a long message would
        // otherwise push it past the summary's cap.
        "task_message" => string("task_id").map(|task_id| match string("message") {
            Some(message) => format!("{task_id} · {message}"),
            None => task_id,
        }),
        // An array-only input summarised to nothing at all.
        "todo" => Some(
            match input.get("todos").and_then(serde_json::Value::as_array) {
                Some(todos) => {
                    let done = todos
                        .iter()
                        .filter(|todo| {
                            todo.get("status").and_then(serde_json::Value::as_str)
                                == Some("completed")
                        })
                        .count();
                    format!("{} todos · {done} done", todos.len())
                }
                None => "read the list".into(),
            },
        ),
        "read" => string("path").map(|path| {
            let offset = input.get("offset").and_then(serde_json::Value::as_u64);
            let limit = input.get("limit").and_then(serde_json::Value::as_u64);
            match (offset, limit) {
                (Some(offset), Some(limit)) => format!("{path}:{offset}+{limit}"),
                (Some(offset), None) => format!("{path}:{offset}"),
                _ => path,
            }
        }),
        "write" | "edit" => string("path"),
        "grep" => string("pattern").map(|pattern| match string("path") {
            Some(path) => format!("/{pattern}/ · {path}"),
            None => format!("/{pattern}/"),
        }),
        "glob" => string("pattern"),
        "task" => summarize_task_input(input).map(|(description, agent, model)| match model {
            Some(model) => format!("{description} · {agent} @ {model}"),
            None => format!("{description} · {agent}"),
        }),
        _ => None,
    }
    // Whatever an arm could not make sense of falls back to the
    // arguments themselves rather than to a blank row: an input the
    // model got half right still says which thing it was about.
    .or_else(|| generic_summary(input))
    .unwrap_or_default();
    summary
        .chars()
        .take(MAX_TOOL_ARGUMENT_SUMMARY_CHARS)
        .collect()
}

/// Keys that say *which* thing a call acted on, most identifying
/// first. Only three arguments fit in a summary, and serde_json hands
/// an object's keys over in alphabetical order here, so without this
/// the room went to whichever key sorted first — which is how a
/// service lost its name and a task id lost its place to a message.
const IDENTIFYING_KEYS: &[&str] = &[
    "action",
    "name",
    "id",
    "taskid",
    "sessionid",
    "path",
    "file",
    "pattern",
    "query",
    "url",
    "command",
    "cmd",
    "description",
];

/// What a tool with no summary of its own says: its arguments, the
/// identifying ones first and the rest alphabetically behind them.
fn generic_summary(input: &serde_json::Value) -> Option<String> {
    let values = input.as_object()?;
    if values.is_empty() {
        // Honest, and different from "we could not summarise this".
        return Some("no arguments".into());
    }
    let mut keys = values.keys().collect::<Vec<_>>();
    keys.sort_by_key(|key| {
        let normalized = normalized_key(key);
        (
            IDENTIFYING_KEYS
                .iter()
                .position(|candidate| *candidate == normalized)
                .unwrap_or(IDENTIFYING_KEYS.len()),
            key.as_str(),
        )
    });
    Some(
        keys.into_iter()
            .filter_map(|key| {
                let value = summarized_value(key, values.get(key)?)?;
                Some(format!("{key}={value}"))
            })
            .take(3)
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

fn summarized_value(key: &str, value: &serde_json::Value) -> Option<String> {
    let redacted = redacted_argument(key, value);
    match redacted.as_ref().unwrap_or(value) {
        serde_json::Value::String(text) => Some(collapse_whitespace(text)),
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => Some(value.to_string()),
        // A list said nothing at all before; how long it is, at least,
        // is something.
        serde_json::Value::Array(items) => Some(format!(
            "{} item{}",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        )),
        _ => None,
    }
}

/// (description, agent, explicit model override) from task-tool input.
pub fn summarize_task_input(input: &serde_json::Value) -> Option<(String, String, Option<String>)> {
    let bounded = |key: &str, limit| {
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(collapse_whitespace)
            .map(|value| value.chars().take(limit).collect::<String>())
    };
    Some((
        bounded("description", 256)?,
        bounded("subagent_type", 128)?,
        bounded("model", 128),
    ))
}

/// The whole input, pretty-printed for the expanded row — secrets
/// gone, by key name and by shell command alike, exactly as
/// [`summarize_tool_input`] does it one line up.
pub fn tool_argument_detail(_name: &str, input: &serde_json::Value) -> String {
    crate::text::bounded_detail(&tool_argument_input(input))
}

/// The one redaction policy for one argument: a value under a
/// sensitive name goes entirely, a shell command keeps its shape
/// without its secrets, a URL keeps everything but its credential, and
/// anything else is not this function's to rewrite (`None`). Both the
/// one-line summary and the expanded detail read it here — two copies
/// of exactly this rule drifting apart is what published a `service`
/// command that a `bash` one had redacted.
fn redacted_argument(key: &str, value: &serde_json::Value) -> Option<serde_json::Value> {
    if sensitive_key(key) {
        return Some(serde_json::Value::String("<redacted>".into()));
    }
    let text = value.as_str()?;
    if shell_command_argument(key) {
        return Some(serde_json::Value::String(redact_command(text)));
    }
    // A credentialed URL is a secret under *any* key — `url`, `msg`, a
    // prompt — which is exactly why no key predicate catches it.
    redact_url_credentials(text).map(serde_json::Value::String)
}

fn redact(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    let redacted = redacted_argument(key, value).unwrap_or_else(|| redact(value));
                    (key.clone(), redacted)
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(redact).collect())
        }
        _ => value.clone(),
    }
}

/// The same, unbounded: what a surface needs when it has to *parse* the
/// input rather than only show it. An `edit` renders as a diff, and a
/// diff of a payload cut at 16 KiB is a diff of invalid JSON — nothing
/// at all. Whoever displays this bounds it; whoever reads it gets what
/// the model actually sent, which is what replay reads from the log.
pub fn tool_argument_input(input: &serde_json::Value) -> String {
    let input = redact(input);
    serde_json::to_string_pretty(&input).unwrap_or_else(|_| input.to_string())
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A key name reduced to its letters and digits, lowercased, so
/// `api_key`, `apiKey` and `API-KEY` are one name.
fn normalized_key(key: &str) -> String {
    key.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether this argument's value is a command line handed to a shell.
///
/// Keyed on the argument, not on the tool: `bash` and `service` both
/// run theirs through [`crate::tools::process::shell_command`], and the
/// two sites that redact them had already drifted apart once while
/// naming `bash` literally — a `service` command with a token in it was
/// published verbatim. Anything that names an argument `command` is
/// treated as a command line, which costs nothing when it is not one
/// (redaction only rewrites tokens that look like secrets) and covers
/// the next tool that takes one the day it is added.
fn shell_command_argument(key: &str) -> bool {
    matches!(
        normalized_key(key).as_str(),
        "command" | "cmd" | "commandline" | "shellcommand"
    )
}

fn sensitive_key(key: &str) -> bool {
    let normalized = normalized_key(key);
    [
        "token",
        "secret",
        "password",
        "authorization",
        "apikey",
        "privatekey",
        "credential",
        "cookie",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

/// A command line with its secrets removed: the tokens that follow a
/// sensitive flag or header, and the ones that announce themselves.
/// Public because a command is named in more places than a tool row —
/// a background job carries its command in the notification it ends
/// in, which is persisted as text nothing redacts later.
pub fn redact_command(command: &str) -> String {
    redact_command_collecting(command, &mut Vec::new())
}

/// The same pass, keeping every value it hid. The caller that redacts
/// a *result* needs to know what the arguments' secrets were: a tool
/// that echoes its command back (`service`'s confirmation does)
/// republishes verbatim what the argument display just redacted.
fn redact_command_collecting(command: &str, secrets: &mut Vec<String>) -> String {
    let mut redact_next = false;
    let mut allow_authorization_scheme = false;
    command
        .split_whitespace()
        .map(|token| {
            if redact_next {
                let normalized = token.trim_matches(['\'', '"', ',']);
                if allow_authorization_scheme
                    && (normalized.eq_ignore_ascii_case("bearer")
                        || normalized.eq_ignore_ascii_case("basic"))
                {
                    allow_authorization_scheme = false;
                    return token.to_string();
                }
                redact_next = false;
                allow_authorization_scheme = false;
                secrets.push(normalized.to_string());
                return "<redacted>".to_string();
            }
            let normalized = token.trim_matches(['\'', '"', ',']);
            if normalized.starts_with("sk-")
                || normalized.starts_with("ghp_")
                || normalized.starts_with("github_pat_")
            {
                secrets.push(normalized.to_string());
                return "<redacted>".to_string();
            }
            let lower = normalized.to_ascii_lowercase();
            if let Some(position) = lower.find("authorization:") {
                let value = lower[position + "authorization:".len()..].trim();
                if value.is_empty() {
                    redact_next = true;
                    allow_authorization_scheme = true;
                    return token.to_string();
                }
                if value == "bearer" || value == "basic" {
                    redact_next = true;
                    return token.to_string();
                }
                // Sliced from `normalized`, not `lower`: the secret has
                // to match the original casing to be found in an echo.
                secrets.push(
                    normalized[position + "authorization:".len()..]
                        .trim()
                        .to_string(),
                );
                return "Authorization:<redacted>".to_string();
            }
            let (key, value) = token.split_once('=').unwrap_or((token, ""));
            let key_name = key.trim_start_matches('-');
            let key_is_label = !value.is_empty() || key.starts_with('-') || key.ends_with(':');
            if key_is_label && sensitive_key(key_name) {
                if value.is_empty() {
                    redact_next = true;
                    return token.to_string();
                }
                secrets.push(value.trim_matches(['\'', '"', ',']).to_string());
                return format!("{key}=<redacted>");
            }
            if normalized.eq_ignore_ascii_case("bearer") {
                redact_next = true;
                return token.to_string();
            }
            if let Some(redacted) = redact_url_credentials(token) {
                return redacted;
            }
            token.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `scheme://user:secret@host` with the credential replaced — the whole
/// userinfo, since the username is usually the account the secret
/// opens. `None` when nothing changed. Requires `://` directly ahead of
/// the credential, so an email or a bare `user:pass@host` in prose is
/// never touched, and a plain URL has no `user:secret@` to match.
fn redact_url_credentials(text: &str) -> Option<String> {
    let mut redacted = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text;
    while let Some(position) = rest.find("://") {
        let after = position + "://".len();
        redacted.push_str(&rest[..after]);
        rest = &rest[after..];
        // The authority ends where a path, query, fragment or plain
        // prose begins — and at any character a URL authority cannot
        // contain. Minified JSON is one whitespace-free run: without
        // the delimiter set, `{"host":"https://api.io","user":"bob@x"}`
        // reads `api.io","user":"bob` as userinfo and this pass would
        // corrupt the value and invent a credential.
        let authority_end = rest
            .find(|c: char| {
                matches!(
                    c,
                    '/' | '?'
                        | '#'
                        | '"'
                        | '\''
                        | ','
                        | ';'
                        | '{'
                        | '}'
                        | '['
                        | ']'
                        | '('
                        | ')'
                        | '<'
                        | '>'
                        | '\\'
                        | '`'
                ) || c.is_whitespace()
            })
            .unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@')
            && let Some((user, secret)) = rest[..at].split_once(':')
            && !user.is_empty()
            && !secret.is_empty()
        {
            redacted.push_str("<redacted>@");
            rest = &rest[at + 1..];
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    redacted.push_str(rest);
    Some(redacted)
}

/// A result pass will chase at most this many argument secrets, and
/// none shorter than this: replacing a two-character "secret"
/// throughout a result mangles more than it protects.
const MAX_RESULT_SECRETS: usize = 16;
const MIN_RESULT_SECRET_CHARS: usize = 4;

/// Every secret the display-side argument redaction would hide for this
/// input: values under sensitive keys, and whatever [`redact_command`]
/// strips from a shell command, nested objects and arrays included.
fn collect_argument_secrets(input: &serde_json::Value, secrets: &mut Vec<String>) {
    match input {
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                match value {
                    serde_json::Value::String(text) => {
                        if sensitive_key(key) {
                            secrets.push(text.clone());
                        } else if shell_command_argument(key) {
                            redact_command_collecting(text, secrets);
                        }
                    }
                    _ => collect_argument_secrets(value, secrets),
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_argument_secrets(value, secrets);
            }
        }
        _ => {}
    }
}

/// Display-side redaction for a tool result body: the secrets the
/// call's own arguments were hiding must not resurface because the tool
/// echoed them back, and a URL credential is scrubbed wherever it
/// appears, since results quote URLs the arguments never carried.
/// Display-only, like every redaction here — the persisted ToolResult
/// and the provider request keep the raw text.
pub fn redact_tool_result(input: &serde_json::Value, result: &str) -> String {
    let mut secrets = Vec::new();
    collect_argument_secrets(input, &mut secrets);
    secrets.retain(|secret| secret.chars().count() >= MIN_RESULT_SECRET_CHARS);
    // Longest first, so a secret that contains another is replaced
    // whole rather than left with a recognisable remainder.
    secrets.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    secrets.dedup();
    secrets.truncate(MAX_RESULT_SECRETS);
    let mut text = std::borrow::Cow::Borrowed(result);
    for secret in &secrets {
        if text.contains(secret.as_str()) {
            text = std::borrow::Cow::Owned(text.replace(secret.as_str(), "<redacted>"));
        }
    }
    match redact_url_credentials(&text) {
        Some(redacted) => redacted,
        None => text.into_owned(),
    }
}

/// Run one user turn to completion.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    resolver: &dyn ProviderResolver,
    registry: &ToolRegistry,
    store: &SessionStore,
    session_id: &str,
    user_input: &str,
    images: &[crate::session::ImageContent],
    system_prompt: Option<&str>,
    config: LoopConfig,
    events: LoopEventSender,
    cancel: CancellationToken,
    tool_ctx: crate::tools::ToolContext,
    steer: Option<SteerReceiver>,
) -> Result<TurnOutcome> {
    run_turn_inner(
        resolver,
        registry,
        store,
        session_id,
        TurnStart::User(user_input, images),
        system_prompt,
        config,
        events,
        cancel,
        tool_ctx,
        steer,
    )
    .await
}

/// Continue from the session's accumulated transcript without appending a
/// synthetic user message. Used to resume a provider call after a failed turn.
#[allow(clippy::too_many_arguments)]
pub async fn resume_turn(
    resolver: &dyn ProviderResolver,
    registry: &ToolRegistry,
    store: &SessionStore,
    session_id: &str,
    system_prompt: Option<&str>,
    config: LoopConfig,
    events: LoopEventSender,
    cancel: CancellationToken,
    tool_ctx: crate::tools::ToolContext,
    steer: Option<SteerReceiver>,
) -> Result<TurnOutcome> {
    run_turn_inner(
        resolver,
        registry,
        store,
        session_id,
        TurnStart::Continue,
        system_prompt,
        config,
        events,
        cancel,
        tool_ctx,
        steer,
    )
    .await
}

/// Answer or cancel the session's pending question and continue without a
/// synthetic user message.
#[allow(clippy::too_many_arguments)]
pub async fn resume_pending_question(
    resolver: &dyn ProviderResolver,
    registry: &ToolRegistry,
    store: &SessionStore,
    session_id: &str,
    response: crate::question::QuestionResponse,
    system_prompt: Option<&str>,
    config: LoopConfig,
    events: LoopEventSender,
    cancel: CancellationToken,
    tool_ctx: crate::tools::ToolContext,
    steer: Option<SteerReceiver>,
) -> Result<TurnOutcome> {
    run_turn_inner(
        resolver,
        registry,
        store,
        session_id,
        TurnStart::Resume(response),
        system_prompt,
        config,
        events,
        cancel,
        tool_ctx,
        steer,
    )
    .await
}

enum TurnStart<'a> {
    User(&'a str, &'a [crate::session::ImageContent]),
    Continue,
    Resume(crate::question::QuestionResponse),
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_inner(
    resolver: &dyn ProviderResolver,
    registry: &ToolRegistry,
    store: &SessionStore,
    session_id: &str,
    start: TurnStart<'_>,
    system_prompt: Option<&str>,
    config: LoopConfig,
    mut events: LoopEventSender,
    cancel: CancellationToken,
    mut tool_ctx: crate::tools::ToolContext,
    mut steer: Option<SteerReceiver>,
) -> Result<TurnOutcome> {
    // Everything up to the prompt append is marked `TurnNeverStarted`:
    // a failure here provably wrote nothing, and the callers that fold
    // queued steers into the prompt key their restore on that marker.
    let mut session = store
        .acquire_writer(session_id)
        .map_err(|error| TurnNeverStarted::mark(error.into()))?
        .load()
        .map_err(|error| TurnNeverStarted::mark(error.into()))?;
    let model = session.effective_model();
    let variant = session.effective_variant();
    let request_options = crate::model::variant_options(&model, variant.as_deref())
        .map_err(TurnNeverStarted::mark)?;
    let provider = resolver
        .resolve_provider(&model)
        .map_err(TurnNeverStarted::mark)?;
    match start {
        TurnStart::User(user_input, images) => {
            if session.pending_question().is_some() {
                return Err(TurnNeverStarted::mark(anyhow::anyhow!(
                    "session has a pending question; use resume_pending_question before starting a new turn"
                )));
            }
            if let Some(parent_tool_call_id) = tool_ctx.call_id.as_ref() {
                session
                    .append(SessionEvent::SubagentInvocation {
                        id: new_id(),
                        parent_tool_call_id: parent_tool_call_id.clone(),
                        ts: Utc::now(),
                    })
                    .map_err(|error| TurnNeverStarted::mark(error.into()))?;
            } else if tool_ctx.depth == 0
                && let Ok(Some(snapshot)) =
                    crate::checkpoint::snapshot(&tool_ctx.cwd, session_id).await
            {
                // Root turns only: the parent's checkpoint covers the
                // workspace a child shares, and the absence of a
                // `call_id` is not a root test on its own — a routed
                // notification runs a child session's turn under a
                // call id of its own making. A failed snapshot (or a
                // non-git cwd) never blocks the turn.
                session
                    .append(SessionEvent::Checkpoint {
                        id: new_id(),
                        commit: snapshot.commit,
                        head: snapshot.head,
                        ts: Utc::now(),
                    })
                    .map_err(|error| TurnNeverStarted::mark(error.into()))?;
            }
            // The prompt append is the marker's boundary — and a
            // failure OF the append is on the never-started side: the
            // line either missed the log or tore, and the reader skips
            // an uncommitted tail. Marking it means a rare torn write
            // re-delivers folded-in steers rather than losing them.
            session
                .append(SessionEvent::UserMessage {
                    id: new_id(),
                    text: user_input.to_string(),
                    images: images.to_vec(),
                    ts: Utc::now(),
                })
                .map_err(|error| TurnNeverStarted::mark(error.into()))?;
        }
        TurnStart::Continue => {
            if tool_ctx.depth != 0 {
                anyhow::bail!("only a root agent can resume a failed turn");
            }
            if session.pending_question().is_some() {
                anyhow::bail!(
                    "session has a pending question; use resume_pending_question before resuming"
                );
            }
            if session.transcript().is_empty() {
                anyhow::bail!("session has no conversation to resume");
            }
        }
        TurnStart::Resume(response) => {
            if tool_ctx.depth != 0 {
                anyhow::bail!("only a root agent can resume a pending question");
            }
            let pending = session
                .pending_question()
                .ok_or_else(|| anyhow::anyhow!("session has no valid pending question"))?;
            response.validate(&pending.request)?;
            session.append(SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: pending.tool_call_id,
                content: serde_json::to_string(&response)?,
                is_error: false,
                images: Vec::new(),
                child_session_id: None,
                state: None,
                ts: Utc::now(),
            })?;
        }
    }
    // The live scratch, from here to the end of the turn: batched deltas
    // for anything tailing the store while this turn runs. Constructed
    // from what `run_turn` already holds, silent about every failure, and
    // deleted by its own drop guard on every path out of this function —
    // see `session::live`.
    let mut live = crate::session::LiveScratch::start(store, session_id);
    events.publish(LoopEvent::TurnStarted, &cancel).await;

    let tools = registry.definitions();

    // Providers reject on input size, not the whole window, so the
    // threshold is measured against the input cap.
    let context_limit = config
        .context_limit
        .or_else(|| resolver.compaction_limit(&model))
        .or_else(|| resolver.context_limit(&model));
    // A forced compaction must run even when the model has no known
    // context limit (the limit only feeds the threshold check).
    let compaction_limit = context_limit.or_else(|| config.force_compaction.then_some(0));
    if let Some(limit) = compaction_limit
        && let Some(summary) = crate::compaction::compact_if_needed_locked(
            provider.as_provider(),
            &model,
            &mut session,
            crate::compaction::CompactionOptions {
                context_limit: limit,
                threshold: config.compaction_threshold,
                force: config.force_compaction,
                cut: crate::compaction::CompactionCut::TurnBoundary,
                system_prompt,
                tools: &tools,
                cancel: &cancel,
            },
        )
        .await?
    {
        events
            .publish(
                LoopEvent::Compacted {
                    context_tokens: crate::compaction::estimate_tokens_with_request(
                        &session,
                        system_prompt,
                        &tools,
                    ),
                    summary,
                },
                &cancel,
            )
            .await;
    }

    // A compaction — this turn's own, or one performed between turns —
    // truncated the model's memory of what the workspace's files say, so
    // everything it had read stops counting as read: the first edit
    // afterwards has to look at the file again.
    tool_ctx
        .seen_files
        .forget_after_compaction(last_compaction(&session));

    tool_ctx.session_id = session_id.to_string();
    // The model bound above is the one every result of this turn is
    // written for — including a subagent's, whose session (not its
    // parent's) is the one loaded here.
    tool_ctx.vision = crate::model::supports_vision(&model);
    tool_ctx.output_tail = Some(events.output_tail_sink());

    let mut iterations = 0;
    // Provider-generated call ids are globally unique in a session. Keeping
    // the completed ids reserved prevents a resumed model response from
    // replaying an already-applied side effect (and keeps JSONL valid).
    let mut seen_tool_call_ids: std::collections::HashSet<String> = session
        .events()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::AssistantMessage { content, .. } => Some(content),
            _ => None,
        })
        .flat_map(|content| content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    // Steers drained at the end of a step, waiting to be appended at the
    // top of the next one.
    let mut pending_steers: Vec<Steer> = Vec::new();
    while iterations < config.max_iterations {
        if cancel.is_cancelled() {
            events.publish_terminal(LoopEvent::TurnDone {
                outcome: TurnOutcome::Aborted,
            });
            return Ok(TurnOutcome::Aborted);
        }

        // Deliver anything the user sent while this turn was running. The
        // top of the loop is a settled step — the previous step's tool
        // results are already appended — so a user message here cannot
        // land between an assistant message's tool calls and their
        // results and break the pairing.
        pending_steers.extend(drain_steers(steer.as_mut()));
        if !pending_steers.is_empty() {
            for Steer { text, images } in pending_steers.drain(..) {
                // The confirmation goes first, because it is what the
                // reader keys "delivered" on: a cancellation landing in
                // this window used to eat the `Steered` after the append
                // had happened, and the reader — hearing nothing — sent
                // the same message again. Publishing first makes the two
                // agree. Nothing is appended for a steer the reader was
                // never told about, so it comes back, once.
                if !events
                    .publish(
                        LoopEvent::Steered {
                            text: text.clone(),
                            images: images.clone(),
                        },
                        &cancel,
                    )
                    .await
                {
                    break;
                }
                // Whatever was attached rides along: the model sees the
                // picture on its next step, exactly as it would on a
                // fresh turn.
                session.append(SessionEvent::UserMessage {
                    id: new_id(),
                    text,
                    images,
                    ts: Utc::now(),
                })?;
            }
            // New instructions get a fresh step budget rather than
            // inheriting whatever the interrupted work had left.
            iterations = 0;
        }

        // A single agentic turn can outgrow the window on its own, so the
        // threshold is re-checked before every step, not only at turn
        // start.
        if iterations > 0
            && let Some(limit) = context_limit
            && let Some(summary) = crate::compaction::compact_if_needed_locked(
                provider.as_provider(),
                &model,
                &mut session,
                crate::compaction::CompactionOptions {
                    context_limit: limit,
                    threshold: config.compaction_threshold,
                    force: false,
                    cut: crate::compaction::CompactionCut::ActiveHistory,
                    system_prompt,
                    tools: &tools,
                    cancel: &cancel,
                },
            )
            .await?
        {
            // Same as at turn start: the summary replaced the file
            // contents the model was working from.
            tool_ctx
                .seen_files
                .forget_after_compaction(last_compaction(&session));
            events
                .publish(
                    LoopEvent::Compacted {
                        context_tokens: crate::compaction::estimate_tokens_with_request(
                            &session,
                            system_prompt,
                            &tools,
                        ),
                        summary,
                    },
                    &cancel,
                )
                .await;
        }

        let request = Request {
            model: model.clone(),
            system_prompt: system_prompt.map(String::from),
            messages: session.transcript(),
            tools: tools.clone(),
            cache_key: Some(session_id.to_string()),
            options: request_options.clone(),
        };

        let mut provider_retries = 0;
        let (acc, aborted, errored) = loop {
            let mut stream = provider.as_provider().stream(request.clone())?;
            let mut acc = StepAccumulator::default();
            let mut aborted = false;
            let mut errored: Option<String> = None;
            let mut retryable_error = false;
            let mut received_response = false;

            loop {
                let next = tokio::select! {
                    next = stream.next() => next,
                    _ = cancel.cancelled() => {
                        aborted = true;
                        break;
                    }
                };
                let Some(event) = next else { break };
                if !matches!(
                    &event,
                    ProviderEvent::Error(_) | ProviderEvent::RetryableError(_)
                ) {
                    received_response = true;
                }
                match event {
                    ProviderEvent::TextDelta(t) => {
                        live.text(&t);
                        events
                            .publish(LoopEvent::TextDelta(t.clone()), &cancel)
                            .await;
                        acc.push_text(t);
                    }
                    ProviderEvent::ThinkingDelta(t) => {
                        live.thinking(&t);
                        events
                            .publish(LoopEvent::ThinkingDelta(t.clone()), &cancel)
                            .await;
                        acc.push_thinking(t);
                    }
                    ProviderEvent::ThinkingCompleted => {
                        // The accumulator closes the block; the scratch
                        // marks the same boundary, because a streaming
                        // reader has only the deltas and would otherwise
                        // run this thought into the next one.
                        live.thinking_break();
                        acc.complete_thinking();
                    }
                    ProviderEvent::ReasoningSummaryDelta(summary) => {
                        live.thinking(&summary);
                        events
                            .publish(LoopEvent::ReasoningSummaryDelta(summary.clone()), &cancel)
                            .await;
                        acc.push_reasoning_summary(summary);
                    }
                    ProviderEvent::ReasoningSummaryCompleted => {
                        live.thinking_break();
                        events
                            .publish(LoopEvent::ReasoningSummaryCompleted, &cancel)
                            .await;
                        acc.complete_reasoning_summary();
                    }
                    ProviderEvent::ReasoningItem { item } => {
                        acc.push_reasoning(item);
                    }
                    ProviderEvent::ToolCallStarted { id, name, item_id } => {
                        // The duplicate check reads the log, and a read
                        // that fails is a failure of *this step*, not of
                        // the process: raised with `?` it skipped
                        // `persist_failed_step`, so the text the user had
                        // already watched stream was never written, the
                        // announced tools never closed, and no `TurnDone`
                        // was published. Every sibling failure goes
                        // through `errored`; so does this one.
                        let duplicate = if seen_tool_call_ids.contains(&id) {
                            Ok(true)
                        } else {
                            session.contains_tool_call_id(&id)
                        };
                        match duplicate {
                            Ok(true) => {
                                errored = Some(format!(
                                    "duplicate tool call id {id:?} already exists in this session"
                                ));
                                break;
                            }
                            Ok(false) => {}
                            Err(error) => {
                                errored = Some(format!(
                                    "could not check tool call id {id:?} against the session: {error:#}"
                                ));
                                break;
                            }
                        }
                        if let Err(error) = acc.start_tool_call(id.clone(), name.clone(), item_id) {
                            errored = Some(error);
                            break;
                        }
                        seen_tool_call_ids.insert(id.clone());
                        acc.announced_calls.insert(id.clone(), name.clone());
                        events
                            .publish(LoopEvent::ToolStarted { id, name }, &cancel)
                            .await;
                    }
                    ProviderEvent::ToolCallInputDelta { id, delta } => {
                        match acc.push_tool_input_delta(&id, &delta) {
                            Ok(update) => {
                                events.publish_tool_input_progress(&id, update.received_bytes);
                                if let Some(arguments) = update.arguments {
                                    events
                                        .publish(
                                            LoopEvent::ToolArguments { id, arguments },
                                            &cancel,
                                        )
                                        .await;
                                }
                            }
                            Err(error) => {
                                errored = Some(error);
                                break;
                            }
                        }
                    }
                    ProviderEvent::ToolCallCompleted { id, name, input } => {
                        if let Err(error) =
                            acc.complete_tool_call(id.clone(), name.clone(), input.clone())
                        {
                            errored = Some(error);
                            break;
                        }
                        let arguments = summarize_tool_input(&name, &input);
                        // The scratch names the call as soon as its
                        // arguments are known: that is when a streaming
                        // row can say what the tool is about to do.
                        live.tool_started(&id, &name, &arguments);
                        if acc.arguments_changed(&id, &arguments) {
                            events
                                .publish(
                                    LoopEvent::ToolArguments {
                                        id: id.clone(),
                                        arguments,
                                    },
                                    &cancel,
                                )
                                .await;
                        }
                        if name == "task"
                            && let Some((description, agent, model)) = summarize_task_input(&input)
                        {
                            events
                                .publish(
                                    LoopEvent::SubagentConfigured {
                                        id: id.clone(),
                                        description,
                                        agent,
                                        model,
                                    },
                                    &cancel,
                                )
                                .await;
                        }
                        events
                            .publish(
                                LoopEvent::ToolInputComplete {
                                    id,
                                    // Unbounded on purpose: a surface
                                    // that renders an edit as a diff has
                                    // to parse this, and a cut at 16 KiB
                                    // is invalid JSON — which is why a
                                    // large edit showed a diff on replay
                                    // (which reads the raw input) and a
                                    // wall of JSON live. Every consumer
                                    // bounds it for display already.
                                    arguments: tool_argument_input(&input),
                                },
                                &cancel,
                            )
                            .await;
                    }
                    ProviderEvent::TurnComplete { stop_reason, usage } => {
                        acc.stop_reason = Some(stop_reason.clone());
                        acc.usage = usage;
                        break;
                    }
                    ProviderEvent::Error(message) => {
                        errored = Some(message);
                        break;
                    }
                    ProviderEvent::RetryableError(message) => {
                        retryable_error = true;
                        errored = Some(message);
                        break;
                    }
                }
            }
            drop(stream); // abort the underlying request

            // A stream that ended without TurnComplete or Error is a broken
            // provider/connection — treat it as a transient error, not a clean step.
            if errored.is_none() && acc.stop_reason.is_none() && !aborted {
                retryable_error = true;
            }
            let errored = errored
                .or_else(|| {
                    (acc.stop_reason.is_none() && !aborted)
                        .then(|| "stream ended before completion".to_string())
                })
                .or_else(|| {
                    acc.stop_reason
                        .as_ref()
                        .and_then(|stop_reason| acc.validate_terminal(stop_reason).err())
                });

            if retryable_error
                && !received_response
                && provider_retries < config.max_provider_retries
                && let Some(message) = errored.as_ref()
            {
                let multiplier = 1_u32
                    .checked_shl(provider_retries.min(31) as u32)
                    .unwrap_or(u32::MAX);
                let delay = config
                    .provider_retry_base_delay
                    .saturating_mul(multiplier)
                    .min(config.provider_retry_max_delay);
                provider_retries += 1;
                events
                    .publish(
                        LoopEvent::ProviderRetry {
                            attempt: provider_retries,
                            max_retries: config.max_provider_retries,
                            delay,
                            error: message.clone(),
                        },
                        &cancel,
                    )
                    .await;
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => {
                        events.publish_terminal(LoopEvent::TurnDone {
                            outcome: TurnOutcome::Aborted,
                        });
                        return Ok(TurnOutcome::Aborted);
                    }
                }
                continue;
            }

            break (acc, aborted, errored);
        };

        let step_usage = acc.usage;

        if let Some(message) = errored {
            // Persist the partial step so the UI's already-shown deltas
            // don't evaporate from the transcript — and record the error
            // itself so failures stay diagnosable from the session log
            // (see meta/issues: provider decode errors were lost before).
            persist_failed_step(
                &mut session,
                &mut events,
                &cancel,
                &model,
                &acc,
                step_usage,
                &message,
            )
            .await?;
            anyhow::bail!(message);
        }

        if aborted {
            // Persist the partial assistant message so the session is
            // resumable...
            let blocks = acc.content_blocks();
            if !blocks.is_empty() {
                session.append(SessionEvent::AssistantMessage {
                    id: new_id(),
                    model: model.clone(),
                    content: blocks,
                    usage: step_usage,
                    stop_reason: "aborted".into(),
                    ts: Utc::now(),
                })?;
            }
            // ...and answer every announced tool call with a synthetic
            // error result: an unanswered tool_use poisons the transcript
            // (providers 400 on tool_use without tool_result). Nothing is
            // published for them: `publish` abandons a cancelled turn's
            // events, and only the terminal event has reserved capacity.
            for (id, _, _, _) in acc.tool_calls() {
                session.append(SessionEvent::ToolResult {
                    id: new_id(),
                    tool_use_id: id.clone(),
                    content: "aborted before execution".into(),
                    is_error: true,
                    images: Vec::new(),
                    child_session_id: None,
                    state: None,
                    ts: Utc::now(),
                })?;
            }
            events.publish_terminal(LoopEvent::TurnDone {
                outcome: TurnOutcome::Aborted,
            });
            return Ok(TurnOutcome::Aborted);
        }

        iterations += 1;

        // Persist the completed assistant message.
        let blocks = acc.content_blocks();
        let had_tool_calls = !acc.tool_indices.is_empty();
        let stop_reason = acc
            .stop_reason
            .clone()
            .map(|r| match r {
                StopReason::EndTurn => "end_turn".to_string(),
                StopReason::ToolUse => "tool_use".to_string(),
                StopReason::MaxTokens => "max_tokens".to_string(),
                StopReason::Refusal => "refusal".to_string(),
                StopReason::Stopped => "stopped".to_string(),
            })
            .unwrap_or_else(|| "unknown".into());
        if !blocks.is_empty() {
            session.append(SessionEvent::AssistantMessage {
                id: new_id(),
                model: model.clone(),
                content: blocks,
                usage: step_usage,
                stop_reason: stop_reason.clone(),
                ts: Utc::now(),
            })?;
        }
        // The step is on the main stream now, so the scratch copy of it
        // is worse than useless: it resets, and a reader drops whatever
        // it was showing in favour of the committed event.
        live.commit();
        events
            .publish(
                LoopEvent::StepComplete {
                    stop_reason: stop_reason.clone(),
                    usage: step_usage,
                    model: model.clone(),
                },
                &cancel,
            )
            .await;
        if cancel.is_cancelled() {
            // Same synthetic answers as the abort above, for the same
            // reason: this step's calls are never going to run.
            for (id, _, _, _) in acc.tool_calls() {
                session.append(SessionEvent::ToolResult {
                    id: new_id(),
                    tool_use_id: id.clone(),
                    content: "aborted before execution".into(),
                    is_error: true,
                    images: Vec::new(),
                    child_session_id: None,
                    state: None,
                    ts: Utc::now(),
                })?;
            }
            events.publish_terminal(LoopEvent::TurnDone {
                outcome: TurnOutcome::Aborted,
            });
            return Ok(TurnOutcome::Aborted);
        }

        if !had_tool_calls {
            // The model is done, but the user may have said something
            // since. Let the drain decide: asking the channel whether it
            // is empty would count messages the drain then discards,
            // reopening the turn with nothing to add and appending a
            // second assistant message with no user message between.
            pending_steers = drain_steers(steer.as_mut());
            if !pending_steers.is_empty() {
                continue;
            }
            events.publish_terminal(LoopEvent::TurnDone {
                outcome: TurnOutcome::Completed,
            });
            return Ok(TurnOutcome::Completed);
        }

        // Never execute incomplete or null-input calls. Keep result order
        // aligned with the streamed call order.
        let ordered_calls = acc.tool_calls();

        let question_calls = ordered_calls
            .iter()
            .filter(|(_, name, _, _)| name.as_str() == crate::question::QUESTION_TOOL_NAME)
            .count();
        if question_calls > 0 && ordered_calls.len() != 1 {
            for (id, name, _, _) in ordered_calls {
                let content = "question must be the sole tool call in a provider step".to_string();
                session.append(SessionEvent::ToolResult {
                    id: new_id(),
                    tool_use_id: id.clone(),
                    content: content.clone(),
                    is_error: true,
                    images: Vec::new(),
                    child_session_id: None,
                    state: None,
                    ts: Utc::now(),
                })?;
                events
                    .publish(
                        LoopEvent::ToolFinished {
                            id: id.clone(),
                            name: name.clone(),
                            is_error: true,
                            result: content.clone(),
                            child_session_id: None,
                        },
                        &cancel,
                    )
                    .await;
            }
            continue;
        }

        if question_calls == 1 {
            let (id, name, input, completed) = ordered_calls[0];
            let parsed = if !completed || input.is_null() {
                Err("question call was incomplete or had invalid arguments".to_string())
            } else if tool_ctx.depth != 0 {
                Err("questions are available only to the root agent".to_string())
            } else if registry.question_sender().is_none() {
                Err("question capability is not attached to this registry".to_string())
            } else {
                serde_json::from_value::<crate::question::QuestionRequest>(input.clone())
                    .map_err(|error| format!("invalid question request: {error}"))
                    .and_then(|request| {
                        crate::question::validate_request(&request)
                            .map_err(|error| format!("invalid question request: {error}"))?;
                        Ok(request)
                    })
            };

            let response = match parsed {
                Ok(request) => {
                    let sender = registry.question_sender().expect("checked above").clone();
                    loop {
                        let (reply, receive) = tokio::sync::oneshot::channel();
                        let prompt = crate::question::QuestionPrompt {
                            session_id: session_id.to_string(),
                            tool_call_id: id.clone(),
                            request: request.clone(),
                            reply,
                        };
                        let delivered = tokio::select! {
                            delivered = sender.send(prompt) => delivered,
                            _ = cancel.cancelled() => {
                                events.publish_terminal(LoopEvent::TurnDone { outcome: TurnOutcome::Aborted });
                                return Ok(TurnOutcome::Aborted);
                            }
                        };
                        if delivered.is_err() {
                            break Err("question frontend is unavailable".to_string());
                        }
                        let received = tokio::select! {
                            response = receive => response,
                            _ = cancel.cancelled() => {
                                events.publish_terminal(LoopEvent::TurnDone { outcome: TurnOutcome::Aborted });
                                return Ok(TurnOutcome::Aborted);
                            }
                        };
                        let response = match received {
                            Ok(response) => response,
                            Err(_) => break Err("question frontend dropped its reply".to_string()),
                        };
                        if response.validate(&request).is_ok() {
                            break serde_json::to_string(&response)
                                .map_err(|error| error.to_string());
                        }
                        // Invalid frontend data is never persisted or sent to the provider;
                        // ask again on a fresh one-shot reply path.
                    }
                }
                Err(error) => Err(error),
            };
            let (content, is_error) = match response {
                Ok(content) => (content, false),
                Err(error) => (error, true),
            };
            session.append(SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: id.clone(),
                content: content.clone(),
                is_error,
                images: Vec::new(),
                child_session_id: None,
                state: None,
                ts: Utc::now(),
            })?;
            events
                .publish(
                    LoopEvent::ToolFinished {
                        id: id.clone(),
                        name: name.clone(),
                        is_error,
                        result: crate::text::bounded_detail(&content),
                        child_session_id: None,
                    },
                    &cancel,
                )
                .await;
            continue;
        }

        let calls: Vec<ToolCall> = ordered_calls
            .iter()
            .filter(|(_, _, input, completed)| *completed && !input.is_null())
            .map(|(id, name, input, _)| ToolCall {
                id: (*id).clone(),
                name: (*name).clone(),
                input: (*input).clone(),
            })
            .collect();
        // What each executing call is, for the marker the scratch writes
        // when it starts: after the commit above the scratch is empty, so
        // this is what tells a supervisor the turn is off running `bash:
        // cargo test` rather than sitting idle. The summaries are the
        // ones already published for these calls — one string, computed
        // once, wherever it is shown.
        let executing: std::collections::HashMap<String, (String, String)> = calls
            .iter()
            .map(|call| {
                let summary = acc.published_arguments.get(&call.id).cloned();
                (
                    call.id.clone(),
                    (call.name.clone(), summary.unwrap_or_default()),
                )
            })
            .collect();
        let mut call_ctx = tool_ctx.clone();
        call_ctx.cancel = cancel.clone();
        let received_bytes = acc.tool_received_bytes.clone();
        let (lifecycle_tx, mut lifecycle_rx) = tokio::sync::mpsc::unbounded_channel();
        let completed_tx = lifecycle_tx.clone();
        let execution = execute_calls_observed(
            calls,
            |name| registry.get(name),
            call_ctx,
            cancel.clone(),
            move |id, _name| {
                let _ = lifecycle_tx.send(ToolExecutionLifecycle::Started {
                    id,
                    started: std::time::Instant::now(),
                });
            },
            move |id, _name| {
                let _ = completed_tx.send(ToolExecutionLifecycle::Completed { id });
            },
        );
        tokio::pin!(execution);
        // Tools are where a turn goes quiet for minutes at a time, and
        // this loop is already the place it waits — so the heartbeat is
        // one more branch of the select rather than a task to own,
        // cancel, and reason about against the drop guard. The first
        // tick is consumed here because the marker written a moment ago
        // is a fresher timestamp than any touch could be.
        let mut heartbeat = tokio::time::interval(config.live_heartbeat);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.tick().await;
        let outcomes = loop {
            tokio::select! {
                biased;
                Some(lifecycle) = lifecycle_rx.recv() => {
                    note_execution_start(&mut live, &lifecycle, &executing);
                    events
                        .publish(tool_execution_loop_event(lifecycle, &received_bytes), &cancel)
                        .await;
                }
                _ = heartbeat.tick() => live.touch(),
                outcomes = &mut execution => {
                    while let Ok(lifecycle) = lifecycle_rx.try_recv() {
                        note_execution_start(&mut live, &lifecycle, &executing);
                        events
                            .publish(tool_execution_loop_event(lifecycle, &received_bytes), &cancel)
                            .await;
                    }
                    break outcomes;
                },
            }
        };
        let mut outcomes = outcomes.into_iter();
        for (id, name, input, completed) in ordered_calls {
            let outcome = if completed && !input.is_null() {
                outcomes.next().expect("one outcome per executable call")
            } else {
                CallOutcome {
                    id: id.clone(),
                    name: name.clone(),
                    output: crate::tools::ToolOutput::error(
                        "tool call was incomplete or had invalid arguments",
                    ),
                    cancelled: false,
                }
            };
            let mut output = outcome.output;
            if let Some(error) = invalid_tool_state(&outcome.name, &output) {
                output.content = error;
                output.is_error = true;
                output.discard_session_state();
            }
            let is_error = output.is_error;
            let child_session_id = output.child_session_id().map(str::to_string);
            let state = output.session_state().cloned();
            let content = std::mem::take(&mut output.content);
            let images = output.take_images();
            // The payload never renders; the live row names it, exactly as
            // the restored transcript will from the stored event. Redacted
            // before bounding — a bound could cut a secret in half and
            // leave the recognisable remainder — and only for display:
            // the appended event below keeps the raw content.
            let result = format!(
                "{}{}",
                crate::text::bounded_detail(&redact_tool_result(input, &content)),
                crate::image::markers(&images)
            );
            session.append(SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: outcome.id.clone(),
                content,
                is_error,
                images,
                child_session_id,
                state,
                ts: Utc::now(),
            })?;
            output.commit_session_state();
            live.tool_finished(&outcome.id, !is_error);
            events
                .publish(
                    LoopEvent::ToolFinished {
                        id: outcome.id,
                        name: outcome.name,
                        is_error,
                        result,
                        child_session_id: output.child_session_id().map(str::to_string),
                    },
                    &cancel,
                )
                .await;
        }
    }

    if cancel.is_cancelled() {
        events.publish_terminal(LoopEvent::TurnDone {
            outcome: TurnOutcome::Aborted,
        });
        return Ok(TurnOutcome::Aborted);
    }
    events.publish_terminal(LoopEvent::TurnDone {
        outcome: TurnOutcome::MaxIterations,
    });
    Ok(TurnOutcome::MaxIterations)
}

fn invalid_tool_state(tool_name: &str, output: &crate::tools::ToolOutput) -> Option<String> {
    let state = output.session_state()?;
    if output.is_error {
        return Some("tool error cannot update session state".into());
    }
    match state {
        crate::session::SessionState::TodoList { list } => {
            if tool_name != "todo" {
                return Some(format!(
                    "tool {tool_name:?} cannot update the session todo list"
                ));
            }
            list.validate()
                .err()
                .map(|error| format!("invalid todo state: {error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn url_redaction_leaves_minified_json_alone() {
        let json = r#"{"host":"https://api.io","user":"bob@x.io"}"#;
        assert_eq!(super::redact_url_credentials(json), None, "no credential here");
        let real = "fetch https://alice:tok3nvalue@git.example.com/repo";
        let redacted = super::redact_url_credentials(real).expect("a real credential");
        assert!(redacted.contains("https://<redacted>@git.example.com"), "{redacted}");
    }

    use super::*;

    #[test]
    fn tool_argument_summaries_are_bounded_and_redacted() {
        let write = summarize_tool_input(
            "write",
            &serde_json::json!({"path": "src/main.rs", "content": "x".repeat(50_000)}),
        );
        assert_eq!(write, "src/main.rs");

        let bash = summarize_tool_input(
            "bash",
            &serde_json::json!({
                "command": "curl -H 'Authorization: Bearer eyJhbGci.opaque.jwt' --header=Authorization:Basic opaque-basic --api-key=also-secret"
            }),
        );
        assert!(!bash.contains("eyJhbGci"), "{bash}");
        assert!(!bash.contains("opaque-basic"), "{bash}");
        assert!(!bash.contains("also-secret"), "{bash}");
        assert!(bash.contains("<redacted>"), "{bash}");

        let custom = summarize_tool_input(
            "custom",
            &serde_json::json!({"apiKey": "secret", "cookie": "session", "query": "safe"}),
        );
        assert!(!custom.contains("secret"), "{custom}");
        assert!(!custom.contains("session"), "{custom}");
        assert!(custom.chars().count() <= MAX_TOOL_ARGUMENT_SUMMARY_CHARS);
    }

    /// Redaction follows the shell, not the tool name: `service` hands
    /// its `command` to `sh -c` exactly as `bash` does, so the same
    /// secret must not be published just because it rode a different
    /// tool. Both the one-line summary and the expanded detail.
    #[test]
    fn a_shell_command_is_redacted_whichever_tool_runs_it() {
        let secret = "curl -H 'Authorization: Bearer eyJhbGci.opaque.jwt' --api-key=also-secret";
        // The last one has no arm of its own and calls the argument
        // something else: the predicate is the whole point.
        for (tool, key) in [
            ("bash", "command"),
            ("service", "command"),
            ("deploy", "cmd"),
        ] {
            let input = serde_json::json!({
                "action": "start",
                "name": "web",
                (key): secret,
            });
            let summary = summarize_tool_input(tool, &input);
            assert!(!summary.contains("eyJhbGci"), "{tool}: {summary}");
            assert!(!summary.contains("also-secret"), "{tool}: {summary}");
            assert!(summary.contains("<redacted>"), "{tool}: {summary}");

            let detail = tool_argument_detail(tool, &input);
            assert!(!detail.contains("eyJhbGci"), "{tool}: {detail}");
            assert!(!detail.contains("also-secret"), "{tool}: {detail}");
            assert!(detail.contains("<redacted>"), "{tool}: {detail}");
        }
    }

    /// A summary says which thing the call acted on. The generic path
    /// used to take three keys in alphabetical order, which dropped the
    /// service name, buried the task id behind a long message, and hid
    /// that a command was detached.
    #[test]
    fn tool_summaries_lead_with_what_the_call_acted_on() {
        let service = summarize_tool_input(
            "service",
            &serde_json::json!({"action": "logs", "name": "web", "lines": 20}),
        );
        assert!(service.contains("logs"), "{service}");
        assert!(service.contains("web"), "{service}");

        let background = summarize_tool_input(
            "bash",
            &serde_json::json!({"command": "cargo test", "run_in_background": true}),
        );
        assert!(background.contains("cargo test"), "{background}");
        assert!(background.contains("background"), "{background}");

        let message = summarize_tool_input(
            "task_message",
            &serde_json::json!({"task_id": "abc-123", "message": "x".repeat(2_000)}),
        );
        assert!(message.starts_with("abc-123"), "{message}");

        let todo = summarize_tool_input(
            "todo",
            &serde_json::json!({"todos": [
                {"content": "one", "status": "completed"},
                {"content": "two", "status": "pending"},
            ]}),
        );
        assert_eq!(todo, "2 todos · 1 done");

        // An arm that cannot make sense of its input falls back to the
        // arguments rather than to a blank row.
        let malformed = summarize_tool_input("task_message", &serde_json::json!({"message": "hi"}));
        assert_eq!(malformed, "message=hi");

        // Array-only and empty schemas said nothing at all.
        let question = summarize_tool_input(
            "question",
            &serde_json::json!({"questions": [{"prompt": "which?"}]}),
        );
        assert!(
            !question.is_empty(),
            "an array-only input summarised to nothing"
        );
        for empty in ["tasks", "models"] {
            let summary = summarize_tool_input(empty, &serde_json::json!({}));
            assert!(!summary.is_empty(), "{empty} summarised to nothing");
        }

        // The generic path still applies, ordered: an unknown tool's
        // identifying key comes before an incidental one whatever the
        // alphabet says.
        let generic = summarize_tool_input(
            "custom",
            &serde_json::json!({"body": "x".repeat(600), "id": "target", "zzz": 1}),
        );
        assert!(generic.starts_with("id=target"), "{generic}");
    }

    #[test]
    fn expanded_tool_details_are_sanitized_and_bounded() {
        let detail = crate::text::bounded_detail(&format!("ok\u{1b}[31m{}", "x".repeat(20_000)));

        assert!(!detail.contains('\u{1b}'));
        assert!(detail.ends_with("… output truncated"));
        assert!(detail.chars().count() <= 16 * 1024 + 20);

        let arguments = tool_argument_detail(
            "custom",
            &serde_json::json!({"token": "secret", "nested": {"password": "hidden"}}),
        );
        assert!(!arguments.contains("secret"));
        assert!(!arguments.contains("hidden"));
        assert!(arguments.contains("<redacted>"));
    }

    #[test]
    fn partial_write_path_extraction_is_bounded_and_json_aware() {
        let mut accumulator = StepAccumulator::default();
        accumulator
            .start_tool_call("write-1".into(), "write".into(), None)
            .unwrap();
        accumulator
            .announced_calls
            .insert("write-1".into(), "write".into());

        assert_eq!(
            accumulator
                .push_tool_input_delta("write-1", r#"{"content_preview":"not a \"path\"","pa"#)
                .unwrap()
                .arguments,
            None
        );
        assert_eq!(
            accumulator
                .push_tool_input_delta("write-1", r#"th":"src/a\nb.rs","content":""#)
                .unwrap()
                .arguments,
            Some("src/a b.rs".into())
        );
        assert!(!accumulator.tool_input_scanners.contains_key("write-1"));

        let mut content_first = StepAccumulator::default();
        content_first
            .start_tool_call("write-2".into(), "write".into(), None)
            .unwrap();
        content_first
            .announced_calls
            .insert("write-2".into(), "write".into());
        let large_content = format!(
            r#"{{"metadata":{{"path":"wrong"}},"content":"{}""#,
            "x".repeat(32 * 1024)
        );
        assert_eq!(
            content_first
                .push_tool_input_delta("write-2", &large_content)
                .unwrap()
                .arguments,
            None
        );
        assert!(matches!(
            content_first.tool_input_scanners["write-2"].state,
            PartialJsonState::AfterValue
        ));
        assert_eq!(
            content_first
                .push_tool_input_delta("write-2", r#", "path":"late.html"}"#)
                .unwrap()
                .arguments,
            Some("late.html".into())
        );
    }

    /// The item id arrives with the announcement and the completion
    /// rewrites the block, so the completion has to carry it forward —
    /// otherwise every finished call replays anonymously, which is the
    /// state that broke prompt caching.
    #[test]
    fn completing_a_tool_call_keeps_the_item_id_from_its_announcement() {
        let mut acc = StepAccumulator::default();
        acc.start_tool_call("call_1".into(), "read".into(), Some("fc_1".into()))
            .unwrap();
        acc.complete_tool_call(
            "call_1".into(),
            "read".into(),
            serde_json::json!({"path": "x"}),
        )
        .unwrap();

        let blocks = acc.content_blocks();
        let ContentBlock::ToolCall { item_id, id, .. } = &blocks[0] else {
            panic!("tool call expected: {blocks:?}");
        };
        assert_eq!(id, "call_1");
        assert_eq!(item_id.as_deref(), Some("fc_1"));
    }

    #[test]
    fn tool_call_terminal_validation_is_strict() {
        let mut missing_completion = StepAccumulator::default();
        missing_completion
            .start_tool_call("call".into(), "read".into(), None)
            .unwrap();
        assert!(
            missing_completion
                .validate_terminal(&StopReason::MaxTokens)
                .unwrap_err()
                .contains("uncompleted")
        );

        let mut null_input = StepAccumulator::default();
        null_input
            .start_tool_call("call".into(), "read".into(), None)
            .unwrap();
        null_input
            .complete_tool_call("call".into(), "read".into(), serde_json::Value::Null)
            .unwrap();
        assert!(null_input.validate_terminal(&StopReason::MaxTokens).is_ok());
        assert!(
            null_input
                .validate_terminal(&StopReason::Stopped)
                .unwrap_err()
                .contains("max_tokens")
        );

        let mut complete_input = StepAccumulator::default();
        complete_input
            .start_tool_call("call".into(), "read".into(), None)
            .unwrap();
        complete_input
            .complete_tool_call("call".into(), "read".into(), serde_json::json!({}))
            .unwrap();
        assert!(
            complete_input
                .validate_terminal(&StopReason::MaxTokens)
                .is_ok()
        );
        assert!(
            complete_input
                .validate_terminal(&StopReason::Stopped)
                .unwrap_err()
                .contains("contradicts")
        );
    }

    #[test]
    fn session_state_is_accepted_only_from_successful_todo_tools() {
        let target = std::sync::Arc::new(std::sync::Mutex::new(crate::todo::TodoList::default()));
        let valid = crate::todo::TodoList {
            items: vec![crate::todo::TodoItem {
                content: "work".into(),
                status: crate::todo::Status::InProgress,
            }],
        };
        let todo =
            crate::tools::ToolOutput::text("ok").with_todo_state(target.clone(), valid.clone());
        assert_eq!(invalid_tool_state("todo", &todo), None);
        assert!(invalid_tool_state("read", &todo).is_some());

        let error =
            crate::tools::ToolOutput::error("failed").with_todo_state(target.clone(), valid);
        assert!(invalid_tool_state("todo", &error).is_some());

        let invalid = crate::tools::ToolOutput::text("bad").with_todo_state(
            target,
            crate::todo::TodoList {
                items: vec![
                    crate::todo::TodoItem {
                        content: "one".into(),
                        status: crate::todo::Status::InProgress,
                    },
                    crate::todo::TodoItem {
                        content: "two".into(),
                        status: crate::todo::Status::InProgress,
                    },
                ],
            },
        );
        assert!(invalid_tool_state("todo", &invalid).is_some());
    }
}
