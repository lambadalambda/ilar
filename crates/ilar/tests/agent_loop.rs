use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use ilar::agent::{LoopConfig, LoopEvent, TurnOutcome, run_turn};
use ilar::provider::{EventStream, MockProvider, Provider, ProviderEvent, Request, StopReason};
use ilar::session::{ContentBlock, SessionMeta, SessionStore, new_id};
use ilar::tools::{Tool, ToolContext, ToolFuture, ToolKind, ToolOutput, ToolRegistry};
use tokio_util::sync::CancellationToken;

fn temp_session(agent: &str) -> (SessionStore, String) {
    let dir = std::env::temp_dir().join(format!("ilar-agent-test-{}", new_id()));
    let store = SessionStore::new(dir);
    let id = new_id();
    store
        .create(SessionMeta {
            session_id: id.clone(),
            parent_id: None,
            agent: agent.into(),
            model: "zai/glm-4.7".into(),
        })
        .unwrap();
    (store, id)
}

fn events_channel() -> (
    tokio::sync::mpsc::UnboundedSender<LoopEvent>,
    tokio::sync::mpsc::UnboundedReceiver<LoopEvent>,
) {
    tokio::sync::mpsc::unbounded_channel()
}

/// A tool the model can "call": records invocations, returns canned text.
#[derive(Clone)]
struct EchoTool {
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "echoes input"
    }
    fn kind(&self) -> ToolKind {
        ToolKind::ReadOnly
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.lock().unwrap().push(input.clone());
            ToolOutput::text(format!("echo: {}", input["msg"].as_str().unwrap_or("?")))
        })
    }
}

fn registry_with(tool: EchoTool) -> ToolRegistry {
    // ToolRegistry with a custom tool injected alongside builtins.
    ToolRegistry::builtin().with_tool(Arc::new(tool)).unwrap()
}

fn tool_call_event(id: &str, msg: &str) -> ProviderEvent {
    ProviderEvent::ToolCallCompleted {
        id: id.into(),
        name: "echo".into(),
        input: serde_json::json!({"msg": msg}),
    }
}

#[tokio::test]
async fn multi_turn_tool_conversation_end_to_end() {
    let (store, session_id) = temp_session("build");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with(EchoTool {
        calls: calls.clone(),
    });

    // Turn 1: two parallel tool calls. Turn 2: final text.
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::TextDelta("checking".into()),
            ProviderEvent::ToolCallStarted {
                id: "t1".into(),
                name: "echo".into(),
            },
            tool_call_event("t1", "alpha"),
            ProviderEvent::ToolCallStarted {
                id: "t2".into(),
                name: "echo".into(),
            },
            tool_call_event("t2", "beta"),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("all done".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
        ],
    ]);

    let (tx, mut rx) = events_channel();
    let cancel = CancellationToken::new();
    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "do the thing",
        Some("system prompt here"),
        LoopConfig::default(),
        tx,
        cancel,
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    // Both tool calls executed.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["msg"], "alpha");
    assert_eq!(calls[1]["msg"], "beta");
    drop(calls);

    // Session log contains the full exchange, in order:
    // user, assistant(tool calls), tool results x2, assistant(final).
    let session = store.load(&session_id).unwrap();
    let transcript = session.transcript();
    assert_eq!(transcript.len(), 4, "transcript: {transcript:?}");
    assert!(matches!(
        &transcript[0].content[0],
        ContentBlock::Text { text } if text == "do the thing"
    ));
    let assistant1 = &transcript[1];
    assert_eq!(assistant1.content.len(), 3); // text + 2 tool calls
    assert!(matches!(&assistant1.content[0], ContentBlock::Text { text } if text == "checking"));
    assert!(matches!(&assistant1.content[1], ContentBlock::ToolCall { id, .. } if id == "t1"));
    assert!(matches!(&assistant1.content[2], ContentBlock::ToolCall { id, .. } if id == "t2"));
    let results = &transcript[2];
    assert!(matches!(
        &results.content[0],
        ContentBlock::ToolResult { tool_use_id, content, is_error: false }
            if tool_use_id == "t1" && content.contains("alpha")
    ));
    assert!(matches!(
        &results.content[1],
        ContentBlock::ToolResult { tool_use_id, content, is_error: false }
            if tool_use_id == "t2" && content.contains("beta")
    ));
    assert!(matches!(
        &transcript[3].content[0],
        ContentBlock::Text { text } if text == "all done"
    ));

    // Loop events were published for the UI.
    let mut published = Vec::new();
    while let Ok(event) = rx.try_recv() {
        published.push(event);
    }
    assert!(
        published
            .iter()
            .any(|e| matches!(e, LoopEvent::TextDelta(t) if t == "checking"))
    );
    assert!(
        published
            .iter()
            .any(|e| matches!(e, LoopEvent::ToolStarted { .. }))
    );
    assert!(
        published
            .iter()
            .any(|e| matches!(e, LoopEvent::ToolFinished { .. }))
    );
    assert_eq!(
        published
            .iter()
            .filter(|e| matches!(e, LoopEvent::ToolStarted { .. }))
            .count(),
        2,
        "each tool call must be announced exactly once"
    );
    assert!(
        published
            .iter()
            .any(|e| matches!(e, LoopEvent::TurnDone { .. }))
    );
}

#[tokio::test]
async fn tool_finished_means_call_and_result_are_already_on_disk() {
    struct ToolThenWait {
        calls: AtomicUsize,
    }

    impl Provider for ToolThenWait {
        fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Box::pin(futures::stream::iter(vec![
                    ProviderEvent::ToolCallStarted {
                        id: "t1".into(),
                        name: "echo".into(),
                    },
                    tool_call_event("t1", "persist me"),
                    ProviderEvent::TurnComplete {
                        stop_reason: StopReason::ToolUse,
                        usage: Default::default(),
                    },
                ])))
            } else {
                Ok(Box::pin(futures::stream::pending()))
            }
        }
    }

    let (store, session_id) = temp_session("build");
    let registry = registry_with(EchoTool {
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let provider = ToolThenWait {
        calls: AtomicUsize::new(0),
    };
    let (tx, mut rx) = events_channel();
    let cancel = CancellationToken::new();
    let turn = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "use echo",
        None,
        LoopConfig::default(),
        tx,
        cancel.clone(),
        ToolContext::root(std::env::temp_dir()),
    );
    tokio::pin!(turn);

    loop {
        tokio::select! {
            event = rx.recv() => {
                if matches!(event, Some(LoopEvent::ToolFinished { id, .. }) if id == "t1") {
                    break;
                }
            }
            result = &mut turn => panic!("turn ended before persistence check: {result:?}"),
        }
    }

    let transcript = store.load(&session_id).unwrap().transcript();
    assert_eq!(transcript.len(), 3, "transcript: {transcript:?}");
    assert!(matches!(
        &transcript[1].content[0],
        ContentBlock::ToolCall { id, .. } if id == "t1"
    ));
    assert!(matches!(
        &transcript[2].content[0],
        ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"
    ));

    cancel.cancel();
    assert_eq!(turn.await.unwrap(), TurnOutcome::Aborted);
}

#[tokio::test]
async fn provider_error_surfaces_and_session_stays_resumable() {
    let (store, session_id) = temp_session("build");
    let registry = ToolRegistry::builtin();
    let provider = MockProvider::new(vec![vec![ProviderEvent::Error("api down".into())]]);

    let (tx, _rx) = events_channel();
    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "hello",
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await;

    assert!(outcome.is_err());
    assert!(outcome.unwrap_err().to_string().contains("api down"));
    // Session still loadable with the user message intact.
    let session = store.load(&session_id).unwrap();
    let transcript = session.transcript();
    assert!(matches!(
        &transcript[0].content[0],
        ContentBlock::Text { text } if text == "hello"
    ));
}

#[tokio::test]
async fn provider_error_after_tool_call_closes_tool_in_session_and_ui() {
    let (store, session_id) = temp_session("build");
    let registry = ToolRegistry::builtin();
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ToolCallStarted {
            id: "t1".into(),
            name: "read".into(),
        },
        ProviderEvent::ToolCallCompleted {
            id: "t1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "Cargo.toml"}),
        },
        ProviderEvent::Error("connection reset".into()),
    ]]);

    let (tx, mut rx) = events_channel();
    let result = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "read it",
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await;

    assert!(result.is_err());
    let transcript = store.load(&session_id).unwrap().transcript();
    assert_eq!(transcript.len(), 3, "transcript: {transcript:?}");
    assert!(matches!(
        &transcript[2].content[0],
        ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t1"
    ));
    let mut finished = false;
    while let Ok(event) = rx.try_recv() {
        finished |= matches!(
            event,
            LoopEvent::ToolFinished { id, is_error: true, .. } if id == "t1"
        );
    }
    assert!(
        finished,
        "failed provider step must close the TUI tool line"
    );
}

/// Streams slowly: one delta, long pause, then more.
struct SlowProvider {
    first: Arc<Mutex<bool>>,
}

impl Provider for SlowProvider {
    fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
        let first = self.first.clone();
        Ok(Box::pin(
            futures::stream::unfold((), move |()| {
                let first = first.clone();
                async move {
                    let already = std::mem::replace(&mut *first.lock().unwrap(), false);
                    if already {
                        Some((ProviderEvent::TextDelta("before".into()), ()))
                    } else {
                        // Second event: pause long enough for the test to cancel.
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        Some((ProviderEvent::TextDelta("never seen".into()), ()))
                    }
                }
            })
            .chain(futures::stream::iter(vec![ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            }])),
        ))
    }
}

#[tokio::test]
async fn abort_mid_stream_leaves_resumable_session() {
    let (store, session_id) = temp_session("build");
    let registry = ToolRegistry::builtin();
    let provider = SlowProvider {
        first: Arc::new(Mutex::new(true)),
    };

    let (tx, _rx) = events_channel();
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel_clone.cancel();
    });

    let start = std::time::Instant::now();
    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "slow request",
        None,
        LoopConfig::default(),
        tx,
        cancel,
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Aborted);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "abort did not stop the stream"
    );
    // Session resumable: user message + partial assistant text.
    let session = store.load(&session_id).unwrap();
    let transcript = session.transcript();
    assert!(
        transcript.len() >= 2,
        "partial assistant message not persisted: {transcript:?}"
    );
    assert!(matches!(
        &transcript[1].content[0],
        ContentBlock::Text { text } if text == "before"
    ));
}

#[tokio::test]
async fn max_iterations_guard_stops_loop() {
    let (store, session_id) = temp_session("build");
    let registry = ToolRegistry::builtin();
    // Always ends with a tool call -> would loop forever without the guard.
    let provider = MockProvider::new(vec![vec![
        tool_call_event("t", "again"),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Default::default(),
        },
    ]]);

    let (tx, _rx) = events_channel();
    let config = LoopConfig {
        max_iterations: 5,
        ..LoopConfig::default()
    };
    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "loop forever",
        None,
        config,
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();

    assert!(matches!(outcome, TurnOutcome::MaxIterations));
    // Provider was called exactly max_iterations times.
    assert_eq!(provider.requests().len(), 5);
}

#[tokio::test]
async fn abort_after_tool_call_keeps_transcript_valid() {
    // Cancel between ToolCallCompleted and TurnComplete: the announced
    // tool call must be answered by a synthetic error result, or the
    // next provider call 400s (tool_use without tool_result).
    struct ToolThenHang;
    impl Provider for ToolThenHang {
        fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
            Ok(Box::pin(
                futures::stream::iter(vec![ProviderEvent::ToolCallCompleted {
                    id: "t1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"msg": "x"}),
                }])
                .chain(futures::stream::unfold((), |()| async move {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Some((
                        ProviderEvent::TurnComplete {
                            stop_reason: StopReason::ToolUse,
                            usage: Default::default(),
                        },
                        (),
                    ))
                })),
            ))
        }
    }

    let (store, session_id) = temp_session("build");
    let registry = ToolRegistry::builtin();
    let provider = ToolThenHang;

    let (tx, _rx) = events_channel();
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel_clone.cancel();
    });

    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "use the tool",
        None,
        LoopConfig::default(),
        tx,
        cancel,
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();
    assert_eq!(outcome, TurnOutcome::Aborted);

    // Transcript: user, assistant(tool_call), user(tool_result is_error).
    let session = store.load(&session_id).unwrap();
    let transcript = session.transcript();
    assert_eq!(transcript.len(), 3, "transcript: {transcript:?}");
    assert!(matches!(
        &transcript[1].content[0],
        ContentBlock::ToolCall { id, .. } if id == "t1"
    ));
    assert!(matches!(
        &transcript[2].content[0],
        ContentBlock::ToolResult { tool_use_id, is_error: true, content }
            if tool_use_id == "t1" && content.contains("aborted")
    ));
}

#[tokio::test]
async fn provider_error_mid_stream_persists_partial_step() {
    // Deltas already shown to the UI must not evaporate from the
    // transcript when the provider errors afterwards.
    let (store, session_id) = temp_session("build");
    let registry = ToolRegistry::builtin();
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("half an answ".into()),
        ProviderEvent::Error("connection reset".into()),
    ]]);

    let (tx, _rx) = events_channel();
    let result = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "hello",
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("connection reset"));
    let session = store.load(&session_id).unwrap();
    let transcript = session.transcript();
    assert_eq!(transcript.len(), 2, "transcript: {transcript:?}");
    assert!(matches!(
        &transcript[1].content[0],
        ContentBlock::Text { text } if text == "half an answ"
    ));
}

#[tokio::test]
async fn stream_ending_without_terminal_event_is_error() {
    // A stream that just ends (no TurnComplete/Error) must surface as an
    // error, not silently execute any announced tool calls.
    struct DeadStream;
    impl Provider for DeadStream {
        fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
            Ok(Box::pin(futures::stream::iter(vec![
                ProviderEvent::TextDelta("trunc".into()),
            ])))
        }
    }

    let (store, session_id) = temp_session("build");
    let registry = ToolRegistry::builtin();
    let (tx, _rx) = events_channel();
    let result = run_turn(
        &DeadStream,
        &registry,
        &store,
        &session_id,
        "hello",
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await;

    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("stream ended before completion")
    );
}
