//! `ilar exec`: one turn, no terminal.
//!
//! The split is the whole design. The answer goes to stdout and
//! nothing else does, so `ilar exec "…" > answer.md` is a useful
//! thing to type; everything about *how* the answer was reached —
//! tools, retries, subagents — goes to stderr, where a pipe ignores it
//! and a human reading along does not. `--json` swaps that for the
//! loop's own events as NDJSON, and then stdout carries events only.

use std::io::Write;

use anyhow::Result;
use ilar::agent::{LOOP_EVENT_CAPACITY, LoopConfig, LoopEvent, TurnOutcome, loop_event_channel};
use ilar::provider::ProviderResolver;
use ilar::session::SessionStore;
use ilar::tools::{ToolContext, ToolRegistry};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecFormat {
    /// Answer on stdout, progress on stderr.
    Text,
    /// Events as NDJSON on stdout.
    Json,
}

/// Where a rendered line belongs. Getting this wrong is what makes a
/// CLI unusable in a pipeline, so it is a value, not a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Out,
    Err,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecLine {
    pub(crate) stream: Stream,
    pub(crate) text: String,
    /// Answer text streams as it arrives; progress lines stand alone.
    pub(crate) newline: bool,
}

/// The exit code a turn earns. A script has to be able to tell "the
/// model answered" from "the model gave up".
pub(crate) fn exit_code(outcome: &Result<TurnOutcome>) -> i32 {
    match outcome {
        Ok(TurnOutcome::Completed) => 0,
        Ok(TurnOutcome::MaxIterations) => 2,
        Ok(TurnOutcome::Aborted) => 130,
        Err(_) => 1,
    }
}

/// What one event prints, or nothing when it is noise for this format.
pub(crate) fn render_event(event: &LoopEvent, format: ExecFormat) -> Option<ExecLine> {
    match format {
        ExecFormat::Json => event_json(event).map(|value| ExecLine {
            stream: Stream::Out,
            text: value.to_string(),
            newline: true,
        }),
        ExecFormat::Text => match event {
            LoopEvent::TextDelta(text) => Some(ExecLine {
                stream: Stream::Out,
                text: text.clone(),
                newline: false,
            }),
            LoopEvent::ToolInputComplete { .. } => None,
            LoopEvent::ToolFinished {
                name,
                is_error,
                result,
                ..
            } => Some(ExecLine {
                stream: Stream::Err,
                text: if *is_error {
                    let detail = result.lines().next().unwrap_or("failed");
                    format!("✗ {name}: {detail}")
                } else {
                    format!("· {name}")
                },
                newline: true,
            }),
            LoopEvent::SubagentConfigured {
                description, agent, ..
            } => Some(ExecLine {
                stream: Stream::Err,
                text: format!("▸ {agent}: {description}"),
                newline: true,
            }),
            LoopEvent::ProviderRetry {
                attempt,
                max_retries,
                error,
                ..
            } => Some(ExecLine {
                stream: Stream::Err,
                text: format!("retry {attempt}/{max_retries}: {error}"),
                newline: true,
            }),
            LoopEvent::Compacted { .. } => Some(ExecLine {
                stream: Stream::Err,
                text: "context compacted".into(),
                newline: true,
            }),
            _ => None,
        },
    }
}

/// The serializable projection of a loop event. `LoopEvent` carries
/// `Instant`s and cannot be serialized as it stands; naming the fields
/// here also keeps the wire format from changing by accident when the
/// enum grows.
fn event_json(event: &LoopEvent) -> Option<serde_json::Value> {
    use serde_json::json;
    Some(match event {
        LoopEvent::TurnStarted => json!({"type": "turn_started"}),
        LoopEvent::TextDelta(text) => json!({"type": "text", "text": text}),
        LoopEvent::ThinkingDelta(text) => json!({"type": "thinking", "text": text}),
        LoopEvent::ToolStarted { id, name } => {
            json!({"type": "tool_started", "id": id, "name": name})
        }
        LoopEvent::ToolInputComplete { id, arguments } => {
            json!({"type": "tool_input", "id": id, "arguments": arguments})
        }
        LoopEvent::ToolFinished {
            id,
            name,
            is_error,
            result,
            child_session_id,
        } => json!({
            "type": "tool_finished",
            "id": id,
            "name": name,
            "is_error": is_error,
            "result": result,
            "child_session_id": child_session_id,
        }),
        LoopEvent::SubagentConfigured {
            id,
            description,
            agent,
            model,
        } => json!({
            "type": "subagent",
            "id": id,
            "description": description,
            "agent": agent,
            "model": model,
        }),
        LoopEvent::ProviderRetry {
            attempt,
            max_retries,
            error,
            ..
        } => json!({
            "type": "retry",
            "attempt": attempt,
            "max_retries": max_retries,
            "error": error,
        }),
        LoopEvent::Compacted { summary, .. } => json!({"type": "compacted", "summary": summary}),
        LoopEvent::TurnDone { outcome } => json!({
            "type": "turn_done",
            "outcome": match outcome {
                TurnOutcome::Completed => "completed",
                TurnOutcome::Aborted => "aborted",
                TurnOutcome::MaxIterations => "max_iterations",
            },
        }),
        _ => return None,
    })
}

fn emit(line: ExecLine, out: &mut dyn Write, err: &mut dyn Write) -> std::io::Result<()> {
    let sink: &mut dyn Write = match line.stream {
        Stream::Out => out,
        Stream::Err => err,
    };
    if line.newline {
        writeln!(sink, "{}", line.text)?;
    } else {
        write!(sink, "{}", line.text)?;
    }
    sink.flush()
}

/// Run one turn to completion, printing as it goes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn exec_turn(
    resolver: &dyn ProviderResolver,
    registry: &ToolRegistry,
    store: &SessionStore,
    session_id: &str,
    prompt: &str,
    system_prompt: Option<&str>,
    loop_config: LoopConfig,
    tool_ctx: ToolContext,
    format: ExecFormat,
    cancel: CancellationToken,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<TurnOutcome> {
    let (events, mut rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
    let turn = ilar::agent::run_turn(
        resolver,
        registry,
        store,
        session_id,
        prompt,
        system_prompt,
        loop_config,
        events,
        cancel,
        tool_ctx,
        None,
    );
    tokio::pin!(turn);
    let mut wrote_answer = false;
    let outcome = loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(event) => {
                    if let Some(line) = render_event(&event, format) {
                        if line.stream == Stream::Out {
                            wrote_answer = true;
                        }
                        emit(line, out, err)?;
                    }
                }
                None => break (&mut turn).await,
            },
            outcome = &mut turn => break outcome,
        }
    };
    // Drain whatever the loop published before it finished.
    while let Ok(event) = rx.try_recv() {
        if let Some(line) = render_event(&event, format) {
            if line.stream == Stream::Out {
                wrote_answer = true;
            }
            emit(line, out, err)?;
        }
    }
    // Streamed text arrives without a trailing newline; a shell prompt
    // landing mid-line is the tell of a CLI nobody piped.
    if format == ExecFormat::Text && wrote_answer {
        writeln!(out)?;
        out.flush()?;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use ilar::provider::{MockProvider, ProviderEvent, StopReason};
    use ilar::session::{SessionMeta, SessionStore, Usage, new_id};

    fn temp_store() -> (SessionStore, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: session_id.clone(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                })
                .unwrap(),
        );
        (store, session_id, dir)
    }

    fn answer(text: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::TextDelta(text.into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ]
    }

    async fn run(
        provider: MockProvider,
        format: ExecFormat,
    ) -> (String, String, Result<TurnOutcome>) {
        let (store, session_id, _dir) = temp_store();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = exec_turn(
            &provider,
            &ToolRegistry::builtin(),
            &store,
            &session_id,
            "do the thing",
            Some("system"),
            LoopConfig::default(),
            ToolContext::root(std::env::temp_dir()),
            format,
            CancellationToken::new(),
            &mut out,
            &mut err,
        )
        .await;
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            outcome,
        )
    }

    #[tokio::test]
    async fn the_answer_goes_to_stdout_and_the_work_goes_to_stderr() {
        let provider = MockProvider::new(vec![
            vec![
                ProviderEvent::ToolCallStarted {
                    id: "call-1".into(),
                    name: "glob".into(),
                    item_id: None,
                },
                ProviderEvent::ToolCallCompleted {
                    id: "call-1".into(),
                    name: "glob".into(),
                    input: serde_json::json!({"pattern": "*.rs"}),
                },
                ProviderEvent::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                },
            ],
            answer("the answer"),
        ]);

        let (out, err, outcome) = run(provider, ExecFormat::Text).await;

        assert!(matches!(outcome, Ok(TurnOutcome::Completed)));
        // stdout is the answer, and a trailing newline so a shell
        // prompt does not land mid-line.
        assert_eq!(out, "the answer\n");
        // The tool ran, and said so somewhere a pipe ignores.
        assert!(err.contains("glob"), "{err:?}");
    }

    #[tokio::test]
    async fn json_mode_puts_events_on_stdout_and_nothing_else() {
        let provider = MockProvider::new(vec![answer("hello")]);

        let (out, err, outcome) = run(provider, ExecFormat::Json).await;

        assert!(matches!(outcome, Ok(TurnOutcome::Completed)));
        assert!(err.is_empty(), "{err:?}");
        let events: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
            .collect();
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"turn_started"), "{kinds:?}");
        assert!(kinds.contains(&"text"), "{kinds:?}");
        assert_eq!(kinds.last(), Some(&"turn_done"), "{kinds:?}");
        assert_eq!(
            events.last().unwrap()["outcome"].as_str(),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn a_failed_turn_reports_itself_and_writes_no_answer() {
        let provider = MockProvider::error("provider exploded");

        let (out, err, outcome) = run(provider, ExecFormat::Text).await;

        assert!(outcome.is_err(), "{outcome:?}");
        assert_eq!(exit_code(&outcome), 1);
        assert!(out.is_empty(), "a failed turn wrote an answer: {out:?}");
        let _ = err;
    }

    #[test]
    fn exit_codes_tell_a_script_what_happened() {
        assert_eq!(exit_code(&Ok(TurnOutcome::Completed)), 0);
        assert_eq!(exit_code(&Ok(TurnOutcome::MaxIterations)), 2);
        assert_eq!(exit_code(&Ok(TurnOutcome::Aborted)), 130);
        assert_eq!(exit_code(&Err(anyhow::anyhow!("boom"))), 1);
    }

    #[test]
    fn text_mode_routes_each_event_to_the_right_stream() {
        let answer = render_event(&LoopEvent::TextDelta("hi".into()), ExecFormat::Text).unwrap();
        assert_eq!(answer.stream, Stream::Out);
        assert!(!answer.newline, "answer text streams as it arrives");

        let failure = render_event(
            &LoopEvent::ToolFinished {
                id: "1".into(),
                name: "read".into(),
                is_error: true,
                result: "no such file\nstack trace".into(),
                child_session_id: None,
            },
            ExecFormat::Text,
        )
        .unwrap();
        assert_eq!(failure.stream, Stream::Err);
        assert_eq!(failure.text, "✗ read: no such file");

        // Thinking is not the answer and is not progress either.
        assert!(render_event(&LoopEvent::ThinkingDelta("hm".into()), ExecFormat::Text).is_none());
    }
}
