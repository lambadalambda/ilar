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

/// The session this run works in, as a line. `--session <id>` and
/// `ilar --view <id>` both need the id, and a run that printed it
/// nowhere left a script nothing to carry to the next one.
pub(crate) fn session_line(session_id: &str, format: ExecFormat) -> ExecLine {
    match format {
        ExecFormat::Json => ExecLine {
            stream: Stream::Out,
            text: serde_json::json!({"type": "session", "id": session_id}).to_string(),
            newline: true,
        },
        ExecFormat::Text => ExecLine {
            stream: Stream::Err,
            text: format!("session {session_id}"),
            newline: true,
        },
    }
}

/// Why a turn stopped, when it stopped short of an answer, and what to
/// type to carry on. `Completed` says it with the answer and an `Err`
/// prints its own line; under `--json` the outcome already rode
/// `turn_done`, so this is a text-mode line only.
pub(crate) fn outcome_line(
    outcome: &Result<TurnOutcome>,
    session_id: &str,
    format: ExecFormat,
) -> Option<ExecLine> {
    if format == ExecFormat::Json {
        return None;
    }
    let why = match outcome {
        Ok(TurnOutcome::MaxIterations) => "the step cap was reached before an answer",
        Ok(TurnOutcome::Aborted) => "interrupted",
        Ok(TurnOutcome::Completed) | Err(_) => return None,
    };
    Some(ExecLine {
        stream: Stream::Err,
        text: format!("stopped: {why} — ilar exec --session {session_id} carries on from here"),
        newline: true,
    })
}

/// A setting that parsed but was not honoured, as a line. Progress, not
/// answer: stderr in text mode, where a pipe ignores it; an event under
/// `--json`, where stdout carries events only.
pub(crate) fn notice_line(text: &str, format: ExecFormat) -> ExecLine {
    match format {
        ExecFormat::Json => ExecLine {
            stream: Stream::Out,
            text: serde_json::json!({"type": "notice", "text": text}).to_string(),
            newline: true,
        },
        ExecFormat::Text => ExecLine {
            stream: Stream::Err,
            text: format!("notice: {text}"),
            newline: true,
        },
    }
}

pub(crate) fn emit_notices(
    notices: &[String],
    format: ExecFormat,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<()> {
    for notice in notices {
        emit(notice_line(notice, format), out, err)?;
    }
    Ok(())
}

/// What a call was made with, kept from the event that carries the
/// arguments until the one that finishes the call — they arrive under
/// one id, several events apart, and only the second one prints.
#[derive(Debug, Default)]
pub(crate) struct ToolArguments(std::collections::HashMap<String, String>);

/// How much of a call's arguments a progress row carries. The summary
/// is capped at 512 for a transcript row that wraps; a line of stderr
/// scrolling past between tool calls wants far less.
const MAX_ROW_ARGUMENT_CHARS: usize = 100;

impl ToolArguments {
    /// Keep the raw arguments of a call. Summarising waits for the
    /// finish rather than reading the name off the `ToolStarted` before
    /// it: a provider that skips the started event still gets a row.
    pub(crate) fn remember(&mut self, event: &LoopEvent) {
        if let LoopEvent::ToolInputComplete { id, arguments } = event {
            self.0.insert(id.clone(), arguments.clone());
        }
    }

    /// The summary for a finished call, taken rather than read: a turn
    /// with thousands of calls should not carry every one of them to
    /// the end. `None` when the arguments never arrived, or were not
    /// JSON, or say nothing.
    pub(crate) fn take(&mut self, id: &str, name: &str) -> Option<String> {
        let arguments = self.0.remove(id)?;
        let value = serde_json::from_str(&arguments).ok()?;
        // Redacted where it can be: `summarize_tool_input` scrubs
        // sensitive keys, shell commands and URL credentials, but a
        // tool with an arm of its own — a `task_message`'s message, a
        // `grep` pattern — returns its free text as written.
        let summary = ilar::agent::summarize_tool_input(name, &value);
        (!summary.is_empty())
            .then(|| ilar::text::truncate_chars_ellipsis(&summary, MAX_ROW_ARGUMENT_CHARS))
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What one event prints, or nothing when it is noise for this format.
/// `argument` is what the call was made with, for the event that
/// finishes one.
pub(crate) fn render_event(
    event: &LoopEvent,
    format: ExecFormat,
    argument: Option<&str>,
) -> Option<ExecLine> {
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
            } => {
                // `· read` twelve times over says a dozen files were
                // read and not one of them which.
                let call = match argument.filter(|argument| !argument.is_empty()) {
                    Some(argument) => format!("{name} {argument}"),
                    None => name.clone(),
                };
                Some(ExecLine {
                    stream: Stream::Err,
                    text: if *is_error {
                        let detail = result.lines().next().unwrap_or("failed");
                        format!("✗ {call}: {detail}")
                    } else {
                        format!("· {call}")
                    },
                    newline: true,
                })
            }
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
            LoopEvent::StepInterrupted {
                attempt,
                max_resumes,
                error,
            } => Some(ExecLine {
                stream: Stream::Err,
                text: format!("interrupted mid-step, continuing {attempt}/{max_resumes}: {error}"),
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
        LoopEvent::StepInterrupted {
            attempt,
            max_resumes,
            error,
        } => json!({
            "type": "step_interrupted",
            "attempt": attempt,
            "max_resumes": max_resumes,
            "error": error,
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

/// One event, printed. Split out so the select loop and the drain that
/// follows it cannot disagree about what a line costs.
fn show(
    event: &LoopEvent,
    format: ExecFormat,
    arguments: &mut ToolArguments,
    wrote_answer: &mut bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<()> {
    // Only the text rows have somewhere to put it; a `--json` reader
    // has the arguments already, on their own event, unsummarised.
    let argument = match format {
        ExecFormat::Text => {
            arguments.remember(event);
            match event {
                LoopEvent::ToolFinished { id, name, .. } => arguments.take(id, name),
                _ => None,
            }
        }
        ExecFormat::Json => None,
    };
    let Some(line) = render_event(event, format, argument.as_deref()) else {
        return Ok(());
    };
    if line.stream == Stream::Out {
        *wrote_answer = true;
    }
    emit(line, out, err)
}

/// Run one turn to completion, printing as it goes. `notices` are the
/// settings this launch could not honour; they belong to the run, not
/// to the turn, but they print here so that one place decides what
/// reaches the two streams and in what order.
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
    notices: &[String],
    cancel: CancellationToken,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<TurnOutcome> {
    // First of all, ahead of the notices: a run killed halfway still
    // told the script which session holds what it got done, and
    // `--json | head -1` is how a script will reach for it.
    emit(session_line(session_id, format), out, err)?;
    emit_notices(notices, format, out, err)?;
    let (events, mut rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
    let turn = ilar::agent::run_turn(
        resolver,
        registry,
        store,
        session_id,
        prompt,
        &[],
        system_prompt,
        loop_config,
        events,
        cancel,
        tool_ctx,
        None,
    );
    tokio::pin!(turn);
    let mut wrote_answer = false;
    let mut arguments = ToolArguments::default();
    let outcome = loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(event) => show(
                    &event, format, &mut arguments, &mut wrote_answer, out, err,
                )?,
                None => break (&mut turn).await,
            },
            outcome = &mut turn => break outcome,
        }
    };
    // Drain whatever the loop published before it finished.
    while let Ok(event) = rx.try_recv() {
        show(&event, format, &mut arguments, &mut wrote_answer, out, err)?;
    }
    // Streamed text arrives without a trailing newline; a shell prompt
    // landing mid-line is the tell of a CLI nobody piped.
    if format == ExecFormat::Text && wrote_answer {
        writeln!(out)?;
        out.flush()?;
    }
    // Last, under the answer: a turn that gave up said so only in its
    // exit code, which a person running it by hand never sees.
    if let Some(line) = outcome_line(&outcome, session_id, format) {
        emit(line, out, err)?;
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
                    cwd: None,
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

    /// What one run printed, and the session it printed it for.
    struct Ran {
        out: String,
        err: String,
        outcome: Result<TurnOutcome>,
        session_id: String,
    }

    async fn run(provider: MockProvider, format: ExecFormat) -> Ran {
        run_with(provider, format, LoopConfig::default(), &[]).await
    }

    async fn run_with(
        provider: MockProvider,
        format: ExecFormat,
        loop_config: LoopConfig,
        notices: &[String],
    ) -> Ran {
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
            loop_config,
            ToolContext::root(std::env::temp_dir()),
            format,
            notices,
            CancellationToken::new(),
            &mut out,
            &mut err,
        )
        .await;
        Ran {
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
            outcome,
            session_id,
        }
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

        let ran = run(provider, ExecFormat::Text).await;

        assert!(matches!(ran.outcome, Ok(TurnOutcome::Completed)));
        // stdout is the answer, and a trailing newline so a shell
        // prompt does not land mid-line.
        assert_eq!(ran.out, "the answer\n");
        // The tool ran, and said so somewhere a pipe ignores.
        assert!(ran.err.contains("glob"), "{:?}", ran.err);
    }

    #[tokio::test]
    async fn json_mode_puts_events_on_stdout_and_nothing_else() {
        let provider = MockProvider::new(vec![answer("hello")]);

        let ran = run(provider, ExecFormat::Json).await;

        assert!(matches!(ran.outcome, Ok(TurnOutcome::Completed)));
        assert!(ran.err.is_empty(), "{:?}", ran.err);
        let events: Vec<serde_json::Value> = ran
            .out
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

        let ran = run(provider, ExecFormat::Text).await;

        assert!(ran.outcome.is_err(), "{:?}", ran.outcome);
        assert_eq!(exit_code(&ran.outcome), 1);
        assert!(
            ran.out.is_empty(),
            "a failed turn wrote an answer: {:?}",
            ran.out
        );
    }

    /// A setting that was not honoured goes where the rest of the "how"
    /// goes: stderr in text mode, an event under `--json`. `ilar exec`
    /// read neither and a project `[providers]` table was dropped in
    /// silence.
    #[test]
    fn a_notice_is_progress_in_text_and_an_event_in_json() {
        let text = notice_line("ilar.toml: [providers] is ignored", ExecFormat::Text);
        assert_eq!(text.stream, Stream::Err);
        assert!(text.text.starts_with("notice: "), "{:?}", text.text);
        assert!(text.newline);

        let json = notice_line("ilar.toml: [providers] is ignored", ExecFormat::Json);
        assert_eq!(json.stream, Stream::Out);
        let parsed: serde_json::Value = serde_json::from_str(&json.text).unwrap();
        assert_eq!(parsed["type"], "notice");
        assert_eq!(parsed["text"], "ilar.toml: [providers] is ignored");

        // Both sinks, in order, and nothing when there is nothing.
        let mut out = Vec::new();
        let mut err = Vec::new();
        emit_notices(
            &["first".to_string(), "second".to_string()],
            ExecFormat::Text,
            &mut out,
            &mut err,
        )
        .unwrap();
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "notice: first\nnotice: second\n"
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        emit_notices(&[], ExecFormat::Json, &mut out, &mut err).unwrap();
        assert!(out.is_empty() && err.is_empty());
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
        let answer =
            render_event(&LoopEvent::TextDelta("hi".into()), ExecFormat::Text, None).unwrap();
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
            None,
        )
        .unwrap();
        assert_eq!(failure.stream, Stream::Err);
        assert_eq!(failure.text, "✗ read: no such file");

        // Thinking is not the answer and is not progress either.
        assert!(
            render_event(
                &LoopEvent::ThinkingDelta("hm".into()),
                ExecFormat::Text,
                None
            )
            .is_none()
        );
    }

    /// A progress row of bare `· read` × 12 says a dozen files were read
    /// and not one of them which. The argument the call was made with is
    /// the row's whole information.
    #[test]
    fn a_tool_row_says_what_the_call_was_about() {
        let finished = |argument| {
            render_event(
                &LoopEvent::ToolFinished {
                    id: "1".into(),
                    name: "read".into(),
                    is_error: false,
                    result: "…".into(),
                    child_session_id: None,
                },
                ExecFormat::Text,
                argument,
            )
            .unwrap()
            .text
        };
        assert_eq!(finished(Some("src/main.rs")), "· read src/main.rs");
        // A call whose arguments never arrived keeps the bare row rather
        // than growing a dangling separator.
        assert_eq!(finished(None), "· read");
        assert_eq!(finished(Some("")), "· read");

        // A failure says what it was about too, before why it failed.
        let failure = render_event(
            &LoopEvent::ToolFinished {
                id: "1".into(),
                name: "read".into(),
                is_error: true,
                result: "no such file".into(),
                child_session_id: None,
            },
            ExecFormat::Text,
            Some("nope.rs"),
        )
        .unwrap();
        assert_eq!(failure.text, "✗ read nope.rs: no such file");
    }

    /// The arguments arrive under one event and the row is printed on
    /// another, several calls later; the id is what joins them.
    #[test]
    fn arguments_wait_for_the_row_that_finishes_them() {
        let mut arguments = ToolArguments::default();
        arguments.remember(&LoopEvent::ToolInputComplete {
            id: "call-1".into(),
            arguments: r#"{"pattern": "**/*.rs"}"#.into(),
        });
        arguments.remember(&LoopEvent::ToolInputComplete {
            id: "call-2".into(),
            arguments: r#"{"path": "/etc/hosts"}"#.into(),
        });
        // Out of order, and each one only once: the map is not a leak
        // that grows for the length of the turn.
        assert_eq!(
            arguments.take("call-2", "read").as_deref(),
            Some("/etc/hosts")
        );
        assert_eq!(arguments.take("call-2", "read"), None);
        assert_eq!(arguments.take("call-1", "glob").as_deref(), Some("**/*.rs"));
        assert!(arguments.is_empty());
        // Arguments that are not JSON at all say nothing rather than
        // printing the parse error into a progress row.
        arguments.remember(&LoopEvent::ToolInputComplete {
            id: "call-3".into(),
            arguments: "{not json".into(),
        });
        assert_eq!(arguments.take("call-3", "read"), None);
        assert_eq!(arguments.take("never-seen", "read"), None);
    }

    /// `--session <id>` on the next run has to come from somewhere, and
    /// nothing printed the id a run made.
    #[test]
    fn a_run_names_the_session_it_made() {
        let text = session_line("abc123", ExecFormat::Text);
        assert_eq!(text.stream, Stream::Err);
        assert_eq!(text.text, "session abc123");
        assert!(text.newline);

        let json = session_line("abc123", ExecFormat::Json);
        assert_eq!(json.stream, Stream::Out);
        let parsed: serde_json::Value = serde_json::from_str(&json.text).unwrap();
        assert_eq!(parsed["type"], "session");
        assert_eq!(parsed["id"], "abc123");
    }

    /// A turn that stopped short of an answer exited 2 or 130 in
    /// silence: the exit code was the only tell, and a person running it
    /// by hand never sees one.
    #[test]
    fn a_turn_that_stopped_short_says_why() {
        let stopped = outcome_line(&Ok(TurnOutcome::MaxIterations), "abc123", ExecFormat::Text)
            .expect("an iteration cap is worth a line");
        assert_eq!(stopped.stream, Stream::Err);
        assert!(stopped.text.starts_with("stopped: "), "{stopped:?}");
        // What to type next, with the id of the session that holds the
        // work so far.
        assert!(stopped.text.contains("--session abc123"), "{stopped:?}");

        let aborted = outcome_line(&Ok(TurnOutcome::Aborted), "abc123", ExecFormat::Text)
            .expect("an interrupted turn is worth a line");
        assert!(aborted.text.starts_with("stopped: "), "{aborted:?}");

        // An answer is its own report, and an error already prints one.
        assert!(outcome_line(&Ok(TurnOutcome::Completed), "abc123", ExecFormat::Text).is_none());
        assert!(outcome_line(&Err(anyhow::anyhow!("boom")), "abc123", ExecFormat::Text).is_none());
        // Under `--json` the outcome already rode `turn_done`.
        assert!(
            outcome_line(&Ok(TurnOutcome::MaxIterations), "abc123", ExecFormat::Json).is_none()
        );
    }

    /// End to end: the session line leads and the tool row carries its
    /// argument. A notice does not get in front of the session — a
    /// script reaches for it with `head -1`, and a launch with a notice
    /// on it is exactly the launch worth scripting around.
    #[tokio::test]
    async fn a_text_run_names_its_session_and_its_calls() {
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
        let ran = run_with(
            provider,
            ExecFormat::Text,
            LoopConfig::default(),
            &["[providers] is ignored".to_string()],
        )
        .await;

        assert!(matches!(ran.outcome, Ok(TurnOutcome::Completed)));
        assert_eq!(ran.out, "the answer\n", "the answer, and only the answer");
        let lines: Vec<&str> = ran.err.lines().collect();
        assert_eq!(
            lines.first(),
            Some(&format!("session {}", ran.session_id).as_str()),
            "{lines:?}"
        );
        assert_eq!(lines.get(1), Some(&"notice: [providers] is ignored"));
        assert!(ran.err.contains("· glob *.rs"), "{:?}", ran.err);
    }

    /// Under `--json` the session is an event like any other, and it is
    /// the first one, notice or no notice.
    #[tokio::test]
    async fn a_json_run_leads_with_its_session() {
        let provider = MockProvider::new(vec![answer("hello")]);
        let ran = run_with(
            provider,
            ExecFormat::Json,
            LoopConfig::default(),
            &["[providers] is ignored".to_string()],
        )
        .await;

        assert!(matches!(ran.outcome, Ok(TurnOutcome::Completed)));
        assert!(ran.err.is_empty(), "{:?}", ran.err);
        let first: serde_json::Value =
            serde_json::from_str(ran.out.lines().next().unwrap()).unwrap();
        assert_eq!(first["type"], "session");
        assert_eq!(first["id"], ran.session_id.as_str());
    }

    /// The line is no use as a function nobody calls: a turn that runs
    /// out of steps has to print it, on stderr, without touching the
    /// answer's stream.
    #[tokio::test]
    async fn a_run_that_hits_the_step_cap_prints_the_line() {
        // A provider that only ever calls a tool: the cap is the only
        // way this turn ends. Each step needs its own call id — the
        // session refuses to record the same one twice.
        let step = |id: &str| {
            vec![
                ProviderEvent::ToolCallStarted {
                    id: id.into(),
                    name: "glob".into(),
                    item_id: None,
                },
                ProviderEvent::ToolCallCompleted {
                    id: id.into(),
                    name: "glob".into(),
                    input: serde_json::json!({"pattern": "*.rs"}),
                },
                ProviderEvent::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                },
            ]
        };
        let provider = MockProvider::new(vec![step("call-1"), step("call-2"), step("call-3")]);
        let ran = run_with(
            provider,
            ExecFormat::Text,
            LoopConfig {
                max_iterations: 2,
                ..LoopConfig::default()
            },
            &[],
        )
        .await;

        assert!(
            matches!(ran.outcome, Ok(TurnOutcome::MaxIterations)),
            "{:?}",
            ran.outcome
        );
        assert_eq!(exit_code(&ran.outcome), 2);
        let last = ran.err.lines().last().unwrap_or_default();
        assert!(last.starts_with("stopped: "), "{:?}", ran.err);
        assert!(
            last.contains(&format!("--session {}", ran.session_id)),
            "{last:?}"
        );
        // Not a word of it on the answer's stream.
        assert!(!ran.out.contains("stopped"), "{:?}", ran.out);
    }
}
