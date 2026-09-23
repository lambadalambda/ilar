//! `ilar exec`: one prompt, no terminal — its turn, and the follow-up
//! turns the work it sent to the background owes it.
//!
//! The split is the whole design. The answer goes to stdout and
//! nothing else does, so `ilar exec "…" > answer.md` is a useful
//! thing to type; everything about *how* the answer was reached —
//! tools, retries, subagents — goes to stderr, where a pipe ignores it
//! and a human reading along does not. `--json` swaps that for the
//! loop's own events as NDJSON, and then stdout carries events only.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use ilar::agent::{LOOP_EVENT_CAPACITY, LoopConfig, LoopEvent, TurnOutcome, loop_event_channel};
use ilar::delivery::{Parcel, Step, deliver_step};
use ilar::provider::ProviderResolver;
use ilar::session::SessionStore;
use ilar::subagent::SubagentSpawner;
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

/// How much of a call's summary a progress row carries. The summary
/// itself is capped at 512 for a transcript row that wraps; a line of
/// stderr scrolling past between tool calls wants far less.
const MAX_ROW_ARGUMENT_CHARS: usize = 100;

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

/// The session line, once, on the turn's first event.
///
/// Not before the turn, which is what it looks like it should be: a
/// run whose provider never resolves — a bad key, a refused model —
/// leaves the session with nothing in it, because `run_turn` appends
/// the user message only after resolving, and `end_session` then
/// removes it. Printing the id first advertised one that the same run
/// deleted, and `--session <id>` on it failed.
///
/// The first event is `TurnStarted`, published after that append, so
/// by the time anything arrives the session is durable. A run that
/// dies before then prints no id and has no work to point at.
fn name_session_once(
    named: &mut bool,
    session_id: &str,
    format: ExecFormat,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<()> {
    if std::mem::replace(named, true) {
        return Ok(());
    }
    emit(session_line(session_id, format), out, err)
}

/// One event, printed. Split out so the select loop and the drain that
/// follows it cannot disagree about what a line costs.
fn show(
    event: &LoopEvent,
    format: ExecFormat,
    arguments: &mut ilar::agent::ToolArguments,
    wrote_answer: &mut bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<()> {
    // Only the text rows have somewhere to put it; a `--json` reader
    // has the arguments already, on their own event, unsummarised.
    let argument = match format {
        ExecFormat::Text => {
            arguments.observe(event);
            match event {
                // Redacted at the source: the loop summarises through
                // `summarize_tool_input`, which scrubs sensitive keys,
                // shell commands and URL credentials from every arm.
                // That matters here more than on a transcript — stderr
                // is redirected to a file.
                LoopEvent::ToolFinished { id, .. } => arguments.take(id).map(|summary| {
                    ilar::text::truncate_chars_ellipsis(&summary, MAX_ROW_ARGUMENT_CHARS)
                }),
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

/// Run one turn to completion, printing as it goes.
///
/// `opening` is `Some` for the run's first turn, carrying the settings
/// this launch could not honour: they belong to the run, not to the
/// turn, but they print here so that one place decides what reaches the
/// two streams and in what order, and the session is named on that
/// turn's first event. A follow-up turn passes `None` and says neither
/// again.
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
    opening: Option<&[String]>,
    cancel: CancellationToken,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<TurnOutcome> {
    // Ahead of everything the turn says, and said even when the turn
    // never starts: a setting that was not honoured may be the reason
    // it did not.
    emit_notices(opening.unwrap_or_default(), format, out, err)?;
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
    let mut named = opening.is_none();
    let mut arguments = ilar::agent::ToolArguments::default();
    let outcome = loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(event) => {
                    name_session_once(&mut named, session_id, format, out, err)?;
                    show(&event, format, &mut arguments, &mut wrote_answer, out, err)?;
                }
                None => break (&mut turn).await,
            },
            outcome = &mut turn => break outcome,
        }
    };
    // Drain whatever the loop published before it finished.
    while let Ok(event) = rx.try_recv() {
        name_session_once(&mut named, session_id, format, out, err)?;
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

/// How long a run keeps trying a busy target once nothing else is
/// running: a target still busy after that is not going to free up for
/// a process that is about to exit, and the outbox keeps the result.
const IDLE_HOLD_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The prompt's turn, then every follow-up its background work owes.
///
/// A task or a background job reports as a notification that starts a
/// follow-up turn, and the model was told so. A headless run that
/// stopped after the first turn cancelled that work at exit and left a
/// script reading "I started it". So the run carries every completion
/// to the root — a follow-up turn — or down the tree, the same step the
/// gateway's seats take, until nothing is running and nothing is owed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn exec_run(
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
    spawner: &Arc<SubagentSpawner>,
    outbox_dir: &std::path::Path,
    cancel: CancellationToken,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<TurnOutcome> {
    // Read before the first turn, as a gateway seat does at its start:
    // what the outbox holds now is an earlier process's. Anything this
    // run's own work reports is recorded there *and* sent live, so a
    // read after the first turn would carry it twice.
    let mut queue: VecDeque<Parcel> = ilar::outbox::pending(store, outbox_dir, session_id)
        .into_iter()
        .map(Parcel::fresh)
        .collect();
    let mut live = spawner.subscribe();
    let mut outcome = exec_turn(
        resolver,
        registry,
        store,
        session_id,
        prompt,
        system_prompt,
        loop_config.clone(),
        tool_ctx.clone(),
        format,
        Some(notices),
        cancel.clone(),
        out,
        err,
    )
    .await;
    let mut held: Vec<Parcel> = Vec::new();
    // A fixed deadline, not a fresh sleep per pass: a steady trickle of
    // live notifications must not starve the held ones.
    let mut retry_at: Option<tokio::time::Instant> = None;
    let mut idle_since: Option<tokio::time::Instant> = None;
    // Only after a turn that ended cleanly: one that failed, was
    // stopped or hit the step cap has said its last word, and the exit
    // cancels the rest.
    while matches!(outcome, Ok(TurnOutcome::Completed)) && !cancel.is_cancelled() {
        while let Ok(notification) = live.try_recv() {
            queue.push_back(Parcel::fresh(notification));
        }
        if let Some(parcel) = queue.pop_front() {
            match deliver_step(
                spawner,
                outbox_dir,
                session_id,
                parcel,
                cancel.child_token(),
            )
            .await
            {
                // Already in the log — an earlier process delivered it
                // and died before the retire: settle it, say nothing.
                Step::FollowUp { retire, .. }
                    if ilar::delivery::is_delivered(store, session_id, &retire.text) =>
                {
                    ilar::outbox::retire(outbox_dir, &retire);
                }
                Step::FollowUp { prompt, retire } => {
                    outcome = exec_turn(
                        resolver,
                        registry,
                        store,
                        session_id,
                        &prompt,
                        system_prompt,
                        loop_config.clone(),
                        tool_ctx.clone(),
                        format,
                        None,
                        cancel.clone(),
                        out,
                        err,
                    )
                    .await;
                    // Settled once the log holds it, whatever the turn
                    // did after taking it.
                    if ilar::delivery::is_delivered(store, session_id, &retire.text) {
                        ilar::outbox::retire(outbox_dir, &retire);
                    }
                }
                Step::Again(parcel) => queue.push_back(parcel),
                Step::Hold(parcel) => {
                    held.push(parcel);
                    retry_at.get_or_insert_with(|| {
                        tokio::time::Instant::now() + ilar::delivery::HOLD_RETRY
                    });
                }
                Step::Delivered => {}
            }
            continue;
        }
        // Counted before the last look at the channel: a task sends its
        // notification before it stops counting as running, so nothing
        // can finish in between unseen.
        let running = spawner.running_background();
        if let Ok(notification) = live.try_recv() {
            queue.push_back(Parcel::fresh(notification));
            continue;
        }
        if running > 0 {
            idle_since = None;
        } else if held.is_empty() {
            break;
        } else if idle_since
            .get_or_insert_with(tokio::time::Instant::now)
            .elapsed()
            > IDLE_HOLD_WAIT
        {
            let text = format!(
                "{} could not be delivered before exit and wait{} in the outbox for the next \
                 `ilar --continue`",
                ilar::text::plural(held.len(), "background result"),
                if held.len() == 1 { "s" } else { "" }
            );
            emit(notice_line(&text, format), out, err)?;
            break;
        }
        let retry = async {
            match retry_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = cancel.cancelled() => break,
            next = live.recv() => match next {
                Some(notification) => queue.push_back(Parcel::fresh(notification)),
                None => break,
            },
            () = retry => {
                retry_at = None;
                queue.extend(held.drain(..));
            }
        }
    }
    // Stopped while waiting on the work, after a turn that completed:
    // the run was interrupted all the same, and a script must hear so.
    if cancel.is_cancelled() && matches!(outcome, Ok(TurnOutcome::Completed)) {
        outcome = Ok(TurnOutcome::Aborted);
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
            Some(notices),
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
        // The notice leads — it may be the reason the turn went the way
        // it did, and it is said even when no turn happens at all. The
        // session follows, ahead of everything the turn says.
        assert_eq!(lines.first(), Some(&"notice: [providers] is ignored"));
        assert_eq!(
            lines.get(1),
            Some(&format!("session {}", ran.session_id).as_str()),
            "{lines:?}"
        );
        assert!(ran.err.contains("· glob *.rs"), "{:?}", ran.err);
    }

    /// Under `--json` the session is an event like any other, ahead of
    /// every event the turn publishes.
    #[tokio::test]
    async fn a_json_run_names_its_session_before_the_turn_speaks() {
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
        let kinds: Vec<String> = ran
            .out
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["type"].to_string()
            })
            .collect();
        let session = kinds.iter().position(|kind| kind == "\"session\"");
        let started = kinds.iter().position(|kind| kind == "\"turn_started\"");
        assert!(session < started, "{kinds:?}");
        assert_eq!(
            kinds.iter().filter(|kind| *kind == "\"session\"").count(),
            1
        );
        let line = ran
            .out
            .lines()
            .find(|line| line.contains("\"session\""))
            .unwrap();
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(event["id"], ran.session_id.as_str());
    }

    /// The id is published on the turn's first event, not before it.
    ///
    /// `run_turn` resolves the provider before appending the user
    /// message, so a bad key leaves the session empty and the run's own
    /// exit removes it. Printing the id first handed a script an id
    /// that would not open.
    #[tokio::test]
    async fn a_turn_that_never_starts_names_no_session() {
        /// A resolver that has no provider for anything — a key that is
        /// not set, a model no configuration can route.
        struct NoProvider;
        impl ilar::provider::ProviderResolver for NoProvider {
            fn resolve_provider(
                &self,
                model: &str,
            ) -> anyhow::Result<ilar::provider::ProviderHandle<'_>> {
                anyhow::bail!("no provider for {model}")
            }
        }

        let (store, session_id, _dir) = temp_store();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = exec_turn(
            &NoProvider,
            &ToolRegistry::builtin(),
            &store,
            &session_id,
            "do the thing",
            Some("system"),
            LoopConfig::default(),
            ToolContext::root(std::env::temp_dir()),
            ExecFormat::Text,
            Some(&[]),
            CancellationToken::new(),
            &mut out,
            &mut err,
        )
        .await;

        assert!(outcome.is_err(), "{outcome:?}");
        let err = String::from_utf8(err).unwrap();
        assert!(
            !err.contains("session "),
            "an id was published for a session the exit will remove: {err:?}"
        );
        // And the session really is the disposable kind: nothing was
        // ever said in it.
        assert!(
            store.is_unspoken_root(&session_id, &std::env::temp_dir().join("no-outbox")),
            "the session has something in it after all; the id was safe to print"
        );
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

    /// A session wired for background work: a spawner with an outbox, as
    /// every runtime has, and the tools and context that reach it.
    struct Rig {
        store: SessionStore,
        session_id: String,
        spawner: Arc<SubagentSpawner>,
        registry: ToolRegistry,
        tool_ctx: ToolContext,
        outbox: tempfile::TempDir,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    fn rig() -> Rig {
        use ilar::config::ProjectInstructions;
        use ilar::provider::FixedProviderResolver;

        let (store, session_id, dir) = temp_store();
        let cwd = tempfile::tempdir().unwrap();
        let outbox = tempfile::tempdir().unwrap();
        let spawner = Arc::new(
            SubagentSpawner::new(
                Arc::new(FixedProviderResolver::new(Arc::new(MockProvider::new(
                    vec![],
                )))),
                store.clone(),
                Vec::new(),
                cwd.path().to_path_buf(),
                0,
                10,
                3,
                ProjectInstructions::Include,
            )
            .with_outbox_dir(outbox.path().to_path_buf()),
        );
        let registry = ToolRegistry::builtin()
            .with_subagents(spawner.clone())
            .unwrap();
        let mut tool_ctx =
            ToolContext::root(cwd.path().to_path_buf()).with_subagents(spawner.clone());
        tool_ctx.session_id = session_id.clone();
        Rig {
            store,
            session_id,
            spawner,
            registry,
            tool_ctx,
            outbox,
            _dirs: (dir, cwd),
        }
    }

    /// A turn that sends `command` to the background, then says so.
    fn backgrounds(command: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ToolCallStarted {
                id: "call-1".into(),
                name: "bash".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "call-1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": command, "run_in_background": true}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ]
    }

    async fn run_in(
        rig: &Rig,
        provider: &MockProvider,
        cancel: CancellationToken,
    ) -> (Result<TurnOutcome>, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = exec_run(
            provider,
            &rig.registry,
            &rig.store,
            &rig.session_id,
            "run it in the background",
            Some("system"),
            LoopConfig::default(),
            rig.tool_ctx.clone(),
            ExecFormat::Text,
            &[],
            &rig.spawner,
            rig.outbox.path(),
            cancel,
            &mut out,
            &mut err,
        )
        .await;
        (
            outcome,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// Work the turn sent to the background comes back before the run
    /// ends: its completion is a follow-up turn, and the answer a
    /// script reads is the one written after it — not "I started it"
    /// followed by the work cancelled at exit. A job that finishes
    /// during the first turn is recorded in the outbox *and* sent live,
    /// and still gets one follow-up, not two.
    #[tokio::test]
    async fn a_run_answers_after_the_work_it_sent_to_the_background() {
        let rig = rig();
        let provider = MockProvider::new(vec![
            backgrounds("printf job-output"),
            answer("started it"),
            answer("the job said job-output"),
        ]);

        let (outcome, out, err) = run_in(&rig, &provider, CancellationToken::new()).await;

        assert!(matches!(outcome, Ok(TurnOutcome::Completed)), "{outcome:?}");
        assert!(out.ends_with("the job said job-output\n"), "{out:?}");
        assert_eq!(
            err.lines()
                .filter(|line| line.starts_with("session "))
                .count(),
            1,
            "the session is named once per run: {err:?}"
        );
        let requests = provider.requests();
        assert_eq!(requests.len(), 3, "one follow-up turn, exactly");
        let follow_up = format!("{:?}", requests[2].messages.last());
        assert!(follow_up.contains("job-output"), "{follow_up}");
        assert_eq!(rig.spawner.running_background(), 0);
    }

    /// Stopped while it waits on the work, after a turn that completed:
    /// the run was interrupted all the same, and its exit code says so.
    #[tokio::test]
    async fn a_run_stopped_while_it_waits_is_an_interrupted_run() {
        let rig = rig();
        let provider = MockProvider::new(vec![backgrounds("sleep 30"), answer("started it")]);
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            stop.cancel();
        });

        let (outcome, _, _) = run_in(&rig, &provider, cancel).await;

        assert!(matches!(outcome, Ok(TurnOutcome::Aborted)), "{outcome:?}");
        assert_eq!(exit_code(&outcome), 130);
        rig.spawner.shutdown().await;
    }
}
