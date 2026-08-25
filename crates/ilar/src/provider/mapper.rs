//! One core for the SSE mappers.
//!
//! OpenAI Responses and z.ai's chat-completions decode different wire
//! formats, but the contract they owe the neutral event stream is a single
//! contract: nothing after the terminal event, a stop reason consistent
//! with the tool calls that were started, a `ToolCallCompleted` for every
//! `ToolCallStarted` (null input when the turn was truncated), tool
//! arguments that parse to a JSON object, and one shape for a stream that
//! ends early. This module owns all five, so a contract fix lands once
//! instead of once per mapper.

use serde_json::Value;

use super::event::{ProviderEvent, StopReason};
use crate::session::{InputTokenAccounting, Usage};

/// Per-flavor wording. The rules are shared; the diagnostics still have to
/// name the wire format the offending event came from.
#[derive(Clone, Copy)]
pub(super) struct MapperLabels {
    /// Names the wire dialect: "OpenAI", "OpenAI-compatible".
    pub flavor: &'static str,
    /// Names the terminal event, for the after-terminal guard.
    pub terminal: &'static str,
    /// Names what a stream that ends early never reached.
    pub expected: &'static str,
}

/// A tool call the wire announced.
pub(super) struct ToolCall {
    /// The mapper's own handle for the call (item id, content-block index,
    /// tool-call index — whatever its wire format keys deltas by).
    key: String,
    /// Wire order. Truncated completions synthesize in this order, which
    /// is not necessarily the order the calls were announced in.
    order: usize,
    pub id: String,
    pub name: String,
    /// Started, no `ToolCallCompleted` emitted yet.
    pending: bool,
}

/// A call the ledger handed back for completion.
pub(super) struct OpenCall {
    pub key: String,
    pub id: String,
    pub name: String,
}

/// The tool-call ledger plus the terminal-state flag: everything the five
/// shared behaviors need to decide.
pub(super) struct MapperCore {
    labels: MapperLabels,
    completed: bool,
    calls: Vec<ToolCall>,
}

impl MapperCore {
    pub fn new(labels: MapperLabels) -> Self {
        Self {
            labels,
            completed: false,
            calls: Vec::new(),
        }
    }

    // ---- (a) terminal state ----

    /// Every mapper's first line: a stream that already produced its
    /// terminal event must not produce anything else.
    pub fn guard_open(&self) -> Result<(), String> {
        if self.completed {
            return Err(format!(
                "{} event arrived after {}",
                self.labels.flavor, self.labels.terminal
            ));
        }
        Ok(())
    }

    pub fn is_completed(&self) -> bool {
        self.completed
    }

    /// Marks the terminal event as emitted.
    pub fn complete(&mut self) {
        self.completed = true;
    }

    // ---- (e) end of stream ----

    /// Stream ended without a terminal event: synthesize a retryable error
    /// rather than letting the consumer hang.
    pub fn finish(&self) -> Option<ProviderEvent> {
        (!self.completed).then(|| {
            ProviderEvent::RetryableError(format!("stream ended before {}", self.labels.expected))
        })
    }

    // ---- tool-call ledger ----

    /// No tool call was ever announced.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn has_key(&self, key: &str) -> bool {
        self.calls.iter().any(|call| call.key == key)
    }

    pub fn has_id(&self, id: &str) -> bool {
        self.calls.iter().any(|call| call.id == id)
    }

    /// The call a mapper's own handle refers to.
    pub fn call(&self, key: &str) -> Option<&ToolCall> {
        self.calls.iter().find(|call| call.key == key)
    }

    /// Records an announced call. `order` is the wire position truncated
    /// completions are synthesized in; mappers whose wire order is their
    /// announcement order pass [`MapperCore::len`].
    ///
    /// Keys are unique: lookups take the first match, so a reused key
    /// would misdirect a call's deltas and strand the shadowed call
    /// forever pending. Every mapper rejects reuse on its own wire terms
    /// before getting here, and the assertion keeps it that way.
    pub fn start(
        &mut self,
        key: impl Into<String>,
        order: usize,
        id: impl Into<String>,
        name: impl Into<String>,
    ) {
        let key = key.into();
        debug_assert!(
            !self.has_key(&key),
            "duplicate tool-call ledger key {key:?}"
        );
        self.calls.push(ToolCall {
            key,
            order,
            id: id.into(),
            name: name.into(),
            pending: true,
        });
    }

    pub fn is_pending(&self, key: &str) -> bool {
        self.call(key).is_some_and(|call| call.pending)
    }

    /// Every announced call has been completed.
    pub fn all_complete(&self) -> bool {
        !self.calls.iter().any(|call| call.pending)
    }

    /// Marks one call completed, yielding its identity for the event.
    pub fn complete_call(&mut self, key: &str) -> Option<(String, String)> {
        let call = self.calls.iter_mut().find(|call| call.key == key)?;
        call.pending = false;
        Some((call.id.clone(), call.name.clone()))
    }

    /// Marks every open call completed, in wire order.
    pub fn take_open(&mut self) -> Vec<OpenCall> {
        let mut open = self
            .calls
            .iter_mut()
            .filter(|call| call.pending)
            .collect::<Vec<_>>();
        open.sort_by_key(|call| call.order);
        open.into_iter()
            .map(|call| {
                call.pending = false;
                OpenCall {
                    key: call.key.clone(),
                    id: call.id.clone(),
                    name: call.name.clone(),
                }
            })
            .collect()
    }

    // ---- (c) truncation ----

    /// Truncated mid-arguments: synthesize null-input completions so every
    /// `ToolCallStarted` still gets its `ToolCallCompleted` (event
    /// contract), in wire order.
    pub fn truncated_completions(&mut self) -> Vec<ProviderEvent> {
        self.take_open()
            .into_iter()
            .map(|call| ProviderEvent::ToolCallCompleted {
                id: call.id,
                name: call.name,
                input: Value::Null,
            })
            .collect()
    }

    // ---- (b) stop-reason / tool-call consistency ----

    /// A turn's stop reason and its tool calls have to agree. `refusal` is
    /// the flavor's refusal signal, which is not always the stop reason:
    /// OpenAI streams refusal text and can still report truncation.
    pub fn validate_stop(&self, stop: &StopReason, refusal: bool) -> Result<(), String> {
        let flavor = self.labels.flavor;
        if refusal && !self.calls.is_empty() {
            return Err(format!("{flavor} response combined refusal and tool calls"));
        }
        match stop {
            StopReason::ToolUse if self.calls.is_empty() => {
                Err(format!("{flavor} tool_use stop has no tool calls"))
            }
            StopReason::EndTurn | StopReason::Refusal if !self.calls.is_empty() => {
                Err(format!("{flavor} {stop:?} stop contradicts tool calls"))
            }
            _ => Ok(()),
        }
    }

    // ---- (d) tool arguments ----

    /// Tool arguments are a JSON *object* or they are not arguments: the
    /// loop hands `input` to a tool that expects named parameters.
    pub fn parse_tool_input(&self, arguments: &str) -> Result<Value, String> {
        let flavor = self.labels.flavor;
        let input = serde_json::from_str(arguments)
            .map_err(|error| format!("invalid {flavor} tool arguments: {error}"))?;
        self.require_object_input(input)
    }

    /// The object rule alone, for arguments that did not arrive as text.
    pub fn require_object_input(&self, input: Value) -> Result<Value, String> {
        if !input.is_object() {
            return Err(format!(
                "{} tool arguments must be a JSON object",
                self.labels.flavor
            ));
        }
        Ok(input)
    }
}

/// A required, non-empty string field.
pub(super) fn required_str<'a>(
    value: &'a Value,
    key: &str,
    label: &str,
) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing or empty {label}"))
}

/// Folds one wire usage payload into an accumulating [`Usage`].
///
/// One function for every wire dialect, so a field only has to be learned
/// once:
///
/// - Responses (`input_tokens`, `input_tokens_details.*`) and
///   chat-completions (`prompt_tokens`, `prompt_tokens_details.*`) report
///   an input total that *includes* what was read from and written to the
///   cache, so both are carved back out — the cost is identical (models
///   without a separate write price bill writes at the input rate) and the
///   split becomes visible.
/// - A gateway that instead reports `cache_read_input_tokens` /
///   `cache_creation_input_tokens` beside an input total that already
///   excludes them has those taken as-is.
///
/// Zero is "not reported": usage can arrive spread over several events, and
/// a later event carrying only some of the fields must not zero the rest.
pub(super) fn merge_usage(usage: &mut Usage, wire: &Value) {
    usage.input_token_accounting = Some(InputTokenAccounting::ExcludesCached);
    let field = |key: &str| wire.get(key).and_then(Value::as_u64).unwrap_or_default();
    let detail = |key: &str| {
        wire["input_tokens_details"][key]
            .as_u64()
            .or_else(|| wire["prompt_tokens_details"][key].as_u64())
            .unwrap_or_default()
    };

    let cached = detail("cached_tokens");
    // Reported from GPT-5.6 on. Without it every request looked like it
    // wrote nothing, which made "the prefix was never cached" and "the
    // prefix was cached somewhere this request could not reach"
    // indistinguishable.
    let written = detail("cache_write_tokens");
    if cached > 0 {
        usage.cache_read_input_tokens = cached;
    }
    if written > 0 {
        usage.cache_creation_input_tokens = written;
    }
    let input = field("input_tokens").max(field("prompt_tokens"));
    if input > 0 {
        usage.input_tokens = input.saturating_sub(cached).saturating_sub(written);
    }
    let output = field("output_tokens").max(field("completion_tokens"));
    if output > 0 {
        usage.output_tokens = output;
    }
    let cache_read = field("cache_read_input_tokens");
    if cache_read > 0 {
        usage.cache_read_input_tokens = cache_read;
    }
    let cache_created = field("cache_creation_input_tokens");
    if cache_created > 0 {
        usage.cache_creation_input_tokens = cache_created;
    }
}

/// One wire usage payload as a standalone [`Usage`] (single-event flavors).
pub(super) fn wire_usage(wire: &Value) -> Usage {
    let mut usage = Usage::default();
    merge_usage(&mut usage, wire);
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core() -> MapperCore {
        MapperCore::new(MapperLabels {
            flavor: "Test",
            terminal: "the end",
            expected: "the end",
        })
    }

    #[test]
    fn the_terminal_guard_closes_the_mapper_exactly_once() {
        let mut core = core();
        assert!(core.guard_open().is_ok());
        assert!(core.finish().is_some());
        core.complete();
        let error = core.guard_open().expect_err("closed");
        assert_eq!(error, "Test event arrived after the end");
        // A completed stream needs no synthesized terminal event.
        assert!(core.finish().is_none());
    }

    #[test]
    fn an_early_end_of_stream_is_retryable() {
        assert!(matches!(
            core().finish(),
            Some(ProviderEvent::RetryableError(error)) if error == "stream ended before the end"
        ));
    }

    /// A refusal and a tool call cannot both be what the turn did — the
    /// rule holds whatever stop reason the flavor reports, including a
    /// truncated one.
    #[test]
    fn a_refusal_never_coexists_with_tool_calls() {
        let mut called = core();
        called.start("k", 0, "call_1", "read");
        for stop in [
            StopReason::Refusal,
            StopReason::MaxTokens,
            StopReason::ToolUse,
        ] {
            assert_eq!(
                called.validate_stop(&stop, true).expect_err("refused"),
                "Test response combined refusal and tool calls"
            );
        }
        // Without tool calls a refusal is just a refusal.
        assert!(core().validate_stop(&StopReason::Refusal, true).is_ok());
    }

    #[test]
    fn a_stop_reason_must_agree_with_the_calls_that_were_started() {
        let empty = core();
        assert!(
            empty
                .validate_stop(&StopReason::ToolUse, false)
                .expect_err("no calls")
                .contains("has no tool calls")
        );
        assert!(empty.validate_stop(&StopReason::EndTurn, false).is_ok());

        let mut called = core();
        called.start("k", 0, "call_1", "read");
        assert!(called.validate_stop(&StopReason::ToolUse, false).is_ok());
        // Truncation is the one stop reason that may leave calls dangling.
        assert!(called.validate_stop(&StopReason::MaxTokens, false).is_ok());
        for stop in [StopReason::EndTurn, StopReason::Refusal] {
            assert!(
                called
                    .validate_stop(&stop, false)
                    .expect_err("contradiction")
                    .contains("contradicts tool calls")
            );
        }
    }

    /// Every started call is completed, and truncation synthesizes the
    /// missing ones in wire order — not announcement order.
    #[test]
    fn truncation_completes_open_calls_in_wire_order() {
        let mut core = core();
        core.start("b", 2, "call_2", "edit");
        core.start("a", 1, "call_1", "read");
        core.start("done", 0, "call_0", "list");
        assert!(!core.all_complete());
        assert_eq!(
            core.complete_call("done"),
            Some(("call_0".into(), "list".into()))
        );
        assert!(!core.is_pending("done"));

        let events = core.truncated_completions();
        assert_eq!(
            events,
            vec![
                ProviderEvent::ToolCallCompleted {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: Value::Null,
                },
                ProviderEvent::ToolCallCompleted {
                    id: "call_2".into(),
                    name: "edit".into(),
                    input: Value::Null,
                },
            ]
        );
        assert!(core.all_complete());
        assert!(core.truncated_completions().is_empty());
    }

    #[test]
    fn tool_arguments_must_parse_to_a_json_object() {
        let core = core();
        assert_eq!(
            core.parse_tool_input("{\"path\":\"x\"}").unwrap(),
            serde_json::json!({"path": "x"})
        );
        assert!(
            core.parse_tool_input("{bad")
                .expect_err("malformed")
                .starts_with("invalid Test tool arguments")
        );
        for arguments in ["[1,2]", "\"text\"", "null"] {
            assert_eq!(
                core.parse_tool_input(arguments).expect_err("not an object"),
                "Test tool arguments must be a JSON object"
            );
        }
        assert!(core.require_object_input(Value::Null).is_err());
    }

    /// Responses and chat-completions report an input total that includes
    /// the cached and written tokens; a gateway may instead report them
    /// beside a total that already excludes them. One function reads both.
    ///
    /// The Responses case is the example from OpenAI's prompt-caching
    /// guide, verbatim: reads and writes are both carved out of the input
    /// total, and what remains is neither. Context and cost must come out
    /// unchanged by the split.
    #[test]
    fn usage_normalization_reads_every_dialect() {
        let responses = wire_usage(&serde_json::json!({
            "input_tokens": 2_600,
            "output_tokens": 50,
            "input_tokens_details": {"cached_tokens": 2_000, "cache_write_tokens": 400},
        }));
        assert_eq!(responses.input_tokens, 200);
        assert_eq!(responses.cache_read_input_tokens, 2_000);
        assert_eq!(responses.cache_creation_input_tokens, 400);
        // The split is cost- and context-neutral: 2_600 input + 50 output.
        assert_eq!(responses.context_tokens(), 2_650);

        // A model that reports no writes still accounts the same way.
        let silent = wire_usage(&serde_json::json!({
            "input_tokens": 2_600,
            "input_tokens_details": {"cached_tokens": 2_000},
        }));
        assert_eq!(silent.cache_creation_input_tokens, 0);
        assert_eq!(silent.input_tokens, 600);
        assert_eq!(silent.context_tokens(), 2_600);

        let chat = wire_usage(&serde_json::json!({
            "prompt_tokens": 1_800,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 1_500, "cache_write_tokens": 100},
        }));
        assert_eq!(chat.input_tokens, 200);
        assert_eq!(chat.cache_read_input_tokens, 1_500);
        assert_eq!(chat.cache_creation_input_tokens, 100);

        let excluded_cache = wire_usage(&serde_json::json!({
            "input_tokens": 300,
            "cache_read_input_tokens": 1_500,
            "cache_creation_input_tokens": 50,
        }));
        assert_eq!(excluded_cache.input_tokens, 300);
        assert_eq!(excluded_cache.cache_read_input_tokens, 1_500);
        assert_eq!(excluded_cache.cache_creation_input_tokens, 50);
        assert_eq!(excluded_cache.context_tokens(), 1_850);
    }

    /// Usage arrives spread over several events; a later one carrying only
    /// output tokens must not erase the cache fields an earlier one set.
    #[test]
    fn merging_usage_never_zeroes_a_field_a_previous_event_reported() {
        let mut usage = Usage::default();
        merge_usage(
            &mut usage,
            &serde_json::json!({
                "input_tokens": 300,
                "cache_read_input_tokens": 1_500,
                "cache_creation_input_tokens": 50,
            }),
        );
        merge_usage(&mut usage, &serde_json::json!({"output_tokens": 30}));
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.output_tokens, 30);
        assert_eq!(usage.cache_read_input_tokens, 1_500);
        assert_eq!(usage.cache_creation_input_tokens, 50);
        assert_eq!(
            usage.input_token_accounting,
            Some(InputTokenAccounting::ExcludesCached)
        );
    }
}
