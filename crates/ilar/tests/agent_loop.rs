use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use ilar::agent::{
    LOOP_EVENT_CAPACITY, LoopConfig, LoopEvent, LoopEventReceiver, LoopEventSender, TurnOutcome,
    loop_event_channel, resume_pending_question, resume_turn, run_turn,
};
use ilar::provider::{EventStream, MockProvider, Provider, ProviderEvent, Request, StopReason};
use ilar::session::{ContentBlock, SessionEvent, SessionMeta, SessionStore, new_id};
use ilar::todo::Status as TodoStatus;
use ilar::tools::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, ToolRegistry, WorkspaceAccess,
};
use tokio_util::sync::CancellationToken;

fn temp_session(agent: &str) -> (SessionStore, String) {
    temp_session_on(agent, "zai/glm-4.7")
}

fn temp_session_on(agent: &str, model: &str) -> (SessionStore, String) {
    let dir = std::env::temp_dir().join(format!("ilar-agent-test-{}", new_id()));
    let store = SessionStore::new(dir);
    let id = new_id();
    store
        .create(SessionMeta {
            session_id: id.clone(),
            parent_id: None,
            agent: agent.into(),
            model: model.into(),
            workspace: None,
            cwd: None,
        })
        .unwrap();
    (store, id)
}

fn events_channel() -> (LoopEventSender, LoopEventReceiver) {
    loop_event_channel(LOOP_EVENT_CAPACITY)
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
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }
    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
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

/// A tool that hands back an image and records the vision flag the turn
/// gave it — the two halves of "a tool result can carry an image".
#[derive(Clone)]
struct SnapshotTool {
    image: ilar::session::ImageContent,
    saw_vision: Arc<Mutex<Option<bool>>>,
}

impl Tool for SnapshotTool {
    fn name(&self) -> &'static str {
        "snapshot"
    }
    fn description(&self) -> &'static str {
        "returns a picture"
    }
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }
    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn run(&self, _input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let image = self.image.clone();
        let saw_vision = self.saw_vision.clone();
        Box::pin(async move {
            *saw_vision.lock().unwrap() = Some(ctx.vision);
            ToolOutput::text("a picture").with_images(vec![image])
        })
    }
}

fn snapshot_turn(id: &str) -> Vec<Vec<ProviderEvent>> {
    vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: id.into(),
                name: "snapshot".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: id.into(),
                name: "snapshot".into(),
                input: serde_json::json!({}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("seen".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
        ],
    ]
}

async fn run_snapshot_turn(model: &str) -> (Vec<SessionEvent>, Option<bool>) {
    let (store, session_id) = temp_session_on("build", model);
    let tool = SnapshotTool {
        image: ilar::session::ImageContent::png(b"\x89PNG\r\n\x1a\npretend"),
        saw_vision: Arc::new(Mutex::new(None)),
    };
    let registry = ToolRegistry::builtin()
        .with_tool(Arc::new(tool.clone()))
        .unwrap();

    run_turn(
        &MockProvider::new(snapshot_turn("snap-1")),
        &registry,
        &store,
        &session_id,
        "look at this",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    let events = store.load(&session_id).unwrap().events().to_vec();
    let saw_vision = *tool.saw_vision.lock().unwrap();
    (events, saw_vision)
}

/// The images a tool returns reach the session log intact — otherwise
/// nothing downstream (wire, replay, compaction) has anything to carry.
#[tokio::test]
async fn a_tool_result_persists_the_images_its_tool_returned() {
    let (events, _) = run_snapshot_turn("zai/glm-4.6v").await;

    let (content, images) = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResult {
                tool_use_id,
                content,
                images,
                ..
            } if tool_use_id == "snap-1" => Some((content.clone(), images.clone())),
            _ => None,
        })
        .expect("tool result for snap-1");
    assert_eq!(content, "a picture");
    assert_eq!(
        images,
        [ilar::session::ImageContent::png(
            b"\x89PNG\r\n\x1a\npretend"
        )]
    );
}

/// A tool asks the context, not the parent, whether anybody can see what
/// it is about to return; the turn answers from the session's own model.
#[tokio::test]
async fn a_tool_sees_the_vision_of_the_model_the_session_runs_on() {
    let (_, vision_model) = run_snapshot_turn("zai/glm-4.6v").await;
    let (_, text_model) = run_snapshot_turn("zai/glm-4.7").await;

    assert_eq!(vision_model, Some(true));
    assert_eq!(text_model, Some(false));
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
async fn every_provider_step_uses_the_stable_session_cache_key() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "echo-1".into(),
                name: "echo".into(),
                item_id: None,
            },
            tool_call_event("echo-1", "first"),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("finished".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
        ],
    ]);
    let registry = registry_with(EchoTool {
        calls: Arc::new(Mutex::new(Vec::new())),
    });

    run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "start",
        &[],
        Some("stable instructions"),
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.cache_key.as_deref() == Some(session_id.as_str()))
    );
}

struct PartialWriteProvider;

impl Provider for PartialWriteProvider {
    fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
        Ok(Box::pin(
            futures::stream::iter(vec![
                ProviderEvent::ToolCallStarted {
                    id: "write-1".into(),
                    name: "write".into(),
                    item_id: None,
                },
                ProviderEvent::ToolCallInputDelta {
                    id: "write-1".into(),
                    delta: r#"{"path":"src/generated.html","content":""#.into(),
                },
            ])
            .chain(futures::stream::pending()),
        ))
    }
}

#[tokio::test]
async fn streamed_write_path_is_published_before_arguments_complete() {
    let (store, session_id) = temp_session("build");
    let (tx, mut rx) = events_channel();
    let cancel = CancellationToken::new();
    let registry = ToolRegistry::builtin();
    let turn = run_turn(
        &PartialWriteProvider,
        &registry,
        &store,
        &session_id,
        "generate a page",
        &[],
        None,
        LoopConfig::default(),
        tx,
        cancel.clone(),
        ToolContext::root(std::env::temp_dir()),
        None,
    );
    tokio::pin!(turn);

    let (arguments, received_bytes) = tokio::time::timeout(Duration::from_secs(1), async {
        let mut arguments = None;
        let mut received_bytes = None;
        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(LoopEvent::ToolInputProgress { id, received_bytes: bytes, .. })
                            if id == "write-1" => received_bytes = Some(bytes),
                        Some(LoopEvent::ToolArguments { id, arguments: summary }) if id == "write-1" => {
                            arguments = Some(summary);
                        }
                        _ => {}
                    }
                    if let Some(received_bytes) = received_bytes
                        && let Some(arguments) = arguments.take()
                    {
                        break (arguments, received_bytes);
                    }
                }
                result = &mut turn => panic!("turn ended before path was published: {result:?}"),
            }
        }
    })
    .await
    .expect("write path was not published while arguments were streaming");

    assert_eq!(arguments, "src/generated.html");
    assert_eq!(
        received_bytes,
        r#"{"path":"src/generated.html","content":""#.len() as u64
    );
    cancel.cancel();
    assert_eq!(turn.await.unwrap(), TurnOutcome::Aborted);
}

#[tokio::test]
async fn completed_write_arguments_transition_from_receiving_to_execution() {
    let (store, session_id) = temp_session("build");
    let dir = tempfile::tempdir().unwrap();
    let delta = r#"{"path":"generated.txt","content":"hello"}"#;
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "write-1".into(),
                name: "write".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallInputDelta {
                id: "write-1".into(),
                delta: delta.into(),
            },
            ProviderEvent::ToolCallCompleted {
                id: "write-1".into(),
                name: "write".into(),
                input: serde_json::json!({"path": "generated.txt", "content": "hello"}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        }],
    ]);
    let (tx, mut rx) = events_channel();

    let outcome = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "write it",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(dir.path().to_path_buf()),
        None,
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("generated.txt")).unwrap(),
        "hello"
    );
    let mut published = Vec::new();
    while let Ok(event) = rx.try_recv() {
        published.push(event);
    }
    let position = |predicate: &dyn Fn(&LoopEvent) -> bool| {
        published
            .iter()
            .position(predicate)
            .expect("expected write lifecycle event")
    };
    let receiving = position(&|event| {
        matches!(
            event,
            LoopEvent::ToolInputProgress { id, received_bytes, .. }
                if id == "write-1" && *received_bytes == delta.len() as u64
        )
    });
    let queued = position(
        &|event| matches!(event, LoopEvent::ToolInputComplete { id, .. } if id == "write-1"),
    );
    let executing = position(&|event| {
        matches!(
            event,
            LoopEvent::ToolExecutionStarted { id, received_bytes, .. }
                if id == "write-1" && *received_bytes == delta.len() as u64
        )
    });
    let finished =
        position(&|event| matches!(event, LoopEvent::ToolFinished { id, .. } if id == "write-1"));
    assert!(published.iter().any(|event| matches!(
        event,
        LoopEvent::ToolInputComplete { id, arguments }
            if id == "write-1" && arguments.contains("generated.txt")
    )));
    assert!(published.iter().any(|event| matches!(
        event,
        LoopEvent::ToolFinished { id, result, .. }
            if id == "write-1" && !result.is_empty()
    )));
    assert_ne!(receiving, executing);
    assert!(queued < executing);
    assert!(executing < finished);
}

#[tokio::test]
async fn cancellation_during_final_event_backpressure_aborts_the_turn() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("answer".into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        },
    ]]);
    let (tx, mut rx) = loop_event_channel(2);
    let cancel = CancellationToken::new();
    let turn_cancel = cancel.clone();
    let turn = tokio::spawn(async move {
        run_turn(
            &provider,
            &ToolRegistry::builtin(),
            &store,
            &session_id,
            "hello",
            &[],
            None,
            LoopConfig::default(),
            tx,
            turn_cancel,
            ToolContext::root(std::env::temp_dir()),
            None,
        )
        .await
        .unwrap()
    });

    tokio::task::yield_now().await;
    assert!(!turn.is_finished());
    cancel.cancel();
    assert_eq!(turn.await.unwrap(), TurnOutcome::Aborted);
    assert!(matches!(rx.recv().await, Some(LoopEvent::TurnStarted)));
    assert!(matches!(rx.recv().await, Some(LoopEvent::TextDelta(text)) if text == "answer"));
    assert!(matches!(
        rx.recv().await,
        Some(LoopEvent::TurnDone {
            outcome: TurnOutcome::Aborted
        })
    ));
}

#[tokio::test]
async fn cancellation_during_tool_step_backpressure_closes_persisted_calls() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ToolCallStarted {
            id: "call-1".into(),
            name: "echo".into(),
            item_id: None,
        },
        tool_call_event("call-1", "hello"),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Default::default(),
        },
    ]]);
    let (tx, _rx) = loop_event_channel(3);
    let cancel = CancellationToken::new();
    let turn_cancel = cancel.clone();
    let turn_store = store.clone();
    let turn_session_id = session_id.clone();
    let turn = tokio::spawn(async move {
        run_turn(
            &provider,
            &ToolRegistry::builtin(),
            &turn_store,
            &turn_session_id,
            "use a tool",
            &[],
            None,
            LoopConfig::default(),
            tx,
            turn_cancel,
            ToolContext::root(std::env::temp_dir()),
            None,
        )
        .await
        .unwrap()
    });

    tokio::task::yield_now().await;
    assert!(!turn.is_finished());
    cancel.cancel();
    assert_eq!(turn.await.unwrap(), TurnOutcome::Aborted);

    let session = store.load(&session_id).unwrap();
    assert!(session.events().iter().any(|event| matches!(
        event,
        SessionEvent::ToolResult {
            tool_use_id,
            is_error: true,
            ..
        } if tool_use_id == "call-1"
    )));
}

#[tokio::test]
async fn todo_replacements_persist_in_provider_call_order() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "todo-1".into(),
                name: "todo".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "todo-1".into(),
                name: "todo".into(),
                input: serde_json::json!({"todos": [{"content": "first", "status": "in_progress"}]}),
            },
            ProviderEvent::ToolCallStarted {
                id: "todo-2".into(),
                name: "todo".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "todo-2".into(),
                name: "todo".into(),
                input: serde_json::json!({"todos": [{"content": "second", "status": "completed"}]}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        }],
    ]);
    let todos = Arc::new(Mutex::new(ilar::todo::TodoList::default()));
    let registry = ToolRegistry::builtin().with_todos(todos.clone()).unwrap();
    let (tx, _rx) = events_channel();

    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "plan",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    let resumed = store.load(&session_id).unwrap();
    let latest = resumed.todo_list().expect("persisted todo list");
    assert_eq!(latest.items[0].content, "second");
    assert_eq!(latest.items[0].status, TodoStatus::Completed);
    let snapshots = resumed
        .events()
        .iter()
        .filter_map(|event| match event {
            ilar::session::SessionEvent::ToolResult {
                state: Some(ilar::session::SessionState::TodoList { list }),
                ..
            } => Some(list.items[0].content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(snapshots, ["first", "second"]);
    assert_eq!(todos.lock().unwrap().items[0].content, "second");
}

#[tokio::test]
async fn truncated_null_input_never_invokes_custom_tool() {
    let (store, session_id) = temp_session("build");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with(EchoTool {
        calls: calls.clone(),
    });
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "truncated".into(),
                name: "echo".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "truncated".into(),
                name: "echo".into(),
                input: serde_json::Value::Null,
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::MaxTokens,
                usage: Default::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("recovered".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
        ],
    ]);
    let (tx, _rx) = events_channel();

    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "go",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(calls.lock().unwrap().is_empty());
    assert!(
        format!("{:?}", store.load(&session_id).unwrap().transcript())
            .contains("incomplete or had invalid arguments")
    );
}

#[tokio::test]
async fn duplicate_tool_completion_is_rejected_before_execution() {
    let (store, session_id) = temp_session("build");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with(EchoTool {
        calls: calls.clone(),
    });
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ToolCallStarted {
            id: "duplicate".into(),
            name: "echo".into(),
            item_id: None,
        },
        tool_call_event("duplicate", "one"),
        tool_call_event("duplicate", "two"),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Default::default(),
        },
    ]]);
    let (tx, _rx) = events_channel();

    let error = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "go",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("duplicate"), "{error:#}");
    assert!(calls.lock().unwrap().is_empty());
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
            ProviderEvent::ThinkingDelta("plan".into()),
            ProviderEvent::ThinkingCompleted,
            ProviderEvent::TextDelta("checking".into()),
            ProviderEvent::ToolCallStarted {
                id: "t1".into(),
                name: "echo".into(),
                item_id: None,
            },
            tool_call_event("t1", "alpha"),
            ProviderEvent::TextDelta("after first".into()),
            ProviderEvent::ToolCallStarted {
                id: "t2".into(),
                name: "echo".into(),
                item_id: None,
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
        &[],
        Some("system prompt here"),
        LoopConfig::default(),
        tx,
        cancel,
        ToolContext::root(std::env::temp_dir()),
        None,
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
    assert_eq!(assistant1.content.len(), 5);
    assert!(
        matches!(&assistant1.content[0], ContentBlock::Diagnostic { text, .. } if text == "plan")
    );
    assert!(matches!(&assistant1.content[1], ContentBlock::Text { text } if text == "checking"));
    assert!(matches!(&assistant1.content[2], ContentBlock::ToolCall { id, .. } if id == "t1"));
    assert!(matches!(&assistant1.content[3], ContentBlock::Text { text } if text == "after first"));
    assert!(matches!(&assistant1.content[4], ContentBlock::ToolCall { id, .. } if id == "t2"));
    let results = &transcript[2];
    assert!(matches!(
        &results.content[0],
        ContentBlock::ToolResult { tool_use_id, content, is_error: false, .. }
            if tool_use_id == "t1" && content.contains("alpha")
    ));
    assert!(matches!(
        &results.content[1],
        ContentBlock::ToolResult { tool_use_id, content, is_error: false, .. }
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
            .filter(|event| matches!(event, LoopEvent::ToolExecutionStarted { .. }))
            .count(),
        2,
        "immediately-ready tools must still publish execution starts"
    );
    assert_eq!(
        published
            .iter()
            .filter(|e| matches!(e, LoopEvent::ToolStarted { .. }))
            .count(),
        2,
        "each tool call must be announced exactly once"
    );
    assert_eq!(
        published
            .iter()
            .filter(|event| matches!(event, LoopEvent::ToolArguments { .. }))
            .count(),
        2,
        "each completed tool call must publish its arguments"
    );
    assert!(published.iter().any(|event| matches!(
        event,
        LoopEvent::ToolArguments { id, arguments }
            if id == "t1" && arguments.contains("alpha")
    )));
    let position = |predicate: &dyn Fn(&LoopEvent) -> bool| {
        published
            .iter()
            .position(predicate)
            .expect("expected loop event")
    };
    let started =
        position(&|event| matches!(event, LoopEvent::ToolStarted { id, .. } if id == "t1"));
    let arguments =
        position(&|event| matches!(event, LoopEvent::ToolArguments { id, .. } if id == "t1"));
    let executing = position(
        &|event| matches!(event, LoopEvent::ToolExecutionStarted { id, .. } if id == "t1"),
    );
    let finished =
        position(&|event| matches!(event, LoopEvent::ToolFinished { id, .. } if id == "t1"));
    assert!(started < arguments && arguments < executing && executing < finished);
    assert!(
        published
            .iter()
            .any(|e| matches!(e, LoopEvent::TurnDone { .. }))
    );
}

#[tokio::test]
async fn multiple_thinking_runs_preserve_order() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ThinkingDelta("first thought".into()),
        ProviderEvent::ThinkingCompleted,
        ProviderEvent::TextDelta("between".into()),
        ProviderEvent::ThinkingDelta("second thought".into()),
        ProviderEvent::ThinkingCompleted,
        ProviderEvent::TextDelta("answer".into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        },
    ]]);
    let (tx, _rx) = events_channel();

    run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "think",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    let transcript = store.load(&session_id).unwrap().transcript();
    let content = &transcript[1].content;
    // Thinking is never replayed, so each closed run is persisted as the
    // diagnostic the reader sees — in the order it was streamed.
    assert!(
        matches!(&content[0], ContentBlock::Diagnostic { text, .. } if text == "first thought"),
        "{content:?}"
    );
    assert!(matches!(&content[1], ContentBlock::Text { text } if text == "between"));
    assert!(
        matches!(&content[2], ContentBlock::Diagnostic { text, .. } if text == "second thought"),
        "{content:?}"
    );
    assert!(matches!(&content[3], ContentBlock::Text { text } if text == "answer"));
}

#[tokio::test]
async fn opaque_reasoning_is_persisted_and_replayed_with_tool_continuation() {
    let (store, session_id) = temp_session("build");
    let registry = registry_with(EchoTool {
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let reasoning = serde_json::json!({
        "id": "rs_1",
        "type": "reasoning",
        "encrypted_content": "encrypted"
    });
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ReasoningItem {
                item: reasoning.clone(),
            },
            ProviderEvent::ToolCallStarted {
                id: "t1".into(),
                name: "echo".into(),
                item_id: None,
            },
            tool_call_event("t1", "continue"),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("done".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
        ],
    ]);
    let (tx, _rx) = events_channel();

    run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "go",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    let session = store.load(&session_id).unwrap();
    assert!(matches!(
        &session.transcript()[1].content[0],
        ContentBlock::Reasoning { item } if item == &reasoning
    ));
    let second = &provider.requests()[1];
    assert!(matches!(
        &second.messages[1].content[0],
        ContentBlock::Reasoning { item } if item == &reasoning
    ));
}

#[tokio::test]
async fn public_reasoning_summary_is_persisted_without_becoming_replay_input() {
    let (store, session_id) = temp_session("build");
    let reasoning = serde_json::json!({
        "id": "rs_1",
        "type": "reasoning",
        "encrypted_content": "encrypted"
    });
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ReasoningSummaryDelta("**Inspecting".into()),
        ProviderEvent::ReasoningSummaryDelta(" configuration**".into()),
        ProviderEvent::ReasoningSummaryCompleted,
        ProviderEvent::ReasoningItem {
            item: reasoning.clone(),
        },
        ProviderEvent::TextDelta("done".into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        },
    ]]);
    let (tx, mut rx) = events_channel();

    run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "go",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    let content = &store.load(&session_id).unwrap().transcript()[1].content;
    assert!(
        matches!(&content[0], ContentBlock::ReasoningSummary { text, completed: true }
        if text == "**Inspecting configuration**")
    );
    assert!(matches!(&content[1], ContentBlock::Reasoning { item } if item == &reasoning));
    assert!(matches!(&content[2], ContentBlock::Text { text } if text == "done"));
    let mut published = Vec::new();
    while let Ok(event) = rx.try_recv() {
        published.push(event);
    }
    assert!(published.iter().any(|event| matches!(
        event,
        LoopEvent::ReasoningSummaryDelta(text) if text.contains("Inspecting")
    )));
    assert!(
        published
            .iter()
            .any(|event| matches!(event, LoopEvent::ReasoningSummaryCompleted))
    );
}

#[tokio::test]
async fn interrupted_reasoning_summary_is_not_persisted() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ReasoningSummaryDelta("**Half written".into()),
        ProviderEvent::Error("connection lost".into()),
    ]]);
    let (tx, _rx) = events_channel();

    let error = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "go",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("connection lost"));
    // The half-written reasoning summary is dropped; only the user turn
    // and the error-diagnostic assistant turn (provider-invisible) remain.
    let transcript = store.load(&session_id).unwrap().transcript();
    assert_eq!(transcript.len(), 2, "{transcript:?}");
    assert!(
        transcript[1]
            .content
            .iter()
            .all(|block| matches!(block, ContentBlock::Diagnostic { text, .. } if text.contains("turn error"))),
        "{transcript:?}"
    );
}

#[tokio::test]
async fn unsigned_thinking_is_persisted_as_diagnostic_text() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ThinkingDelta("unfinished".into()),
        ProviderEvent::ThinkingCompleted,
        ProviderEvent::TextDelta("answer".into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        },
    ]]);
    let (tx, _rx) = events_channel();

    run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "think",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    let content = &store.load(&session_id).unwrap().transcript()[1].content;
    assert!(matches!(&content[0], ContentBlock::Diagnostic { text, .. }
        if text == "unfinished"));
    assert!(matches!(&content[1], ContentBlock::Text { text } if text == "answer"));
}

#[tokio::test]
async fn started_tool_call_without_completion_is_rejected() {
    let (store, session_id) = temp_session("build");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with(EchoTool {
        calls: calls.clone(),
    });
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ToolCallStarted {
            id: "incomplete".into(),
            name: "echo".into(),
            item_id: None,
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::MaxTokens,
            usage: Default::default(),
        },
    ]]);
    let (tx, _rx) = events_channel();

    let error = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "go",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap_err();

    assert!(calls.lock().unwrap().is_empty());
    assert!(
        error.to_string().contains("uncompleted tool calls"),
        "{error:#}"
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
                        item_id: None,
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
        &[],
        None,
        LoopConfig::default(),
        tx,
        cancel.clone(),
        ToolContext::root(std::env::temp_dir()),
        None,
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
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
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
async fn failed_tool_chain_resumes_without_replaying_prompt_or_tool() {
    let (store, session_id) = temp_session("build");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with(EchoTool {
        calls: calls.clone(),
    });
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "echo-1".into(),
                name: "echo".into(),
                item_id: None,
            },
            tool_call_event("echo-1", "once"),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![ProviderEvent::Error("connection lost".into())],
        vec![
            ProviderEvent::TextDelta("recovered".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
        ],
    ]);

    let first = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "original prompt",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await;
    assert!(first.is_err());

    let resumed = resume_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    assert_eq!(resumed, TurnOutcome::Completed);
    assert_eq!(calls.lock().unwrap().len(), 1, "completed tool reran");
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let resumed_messages = &requests[2].messages;
    let original_count = resumed_messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, ContentBlock::Text { text } if text == "original prompt"))
        .count();
    assert_eq!(original_count, 1, "original prompt was duplicated");
    assert!(format!("{resumed_messages:?}").contains("echo: once"));
}

#[tokio::test]
async fn resumed_provider_cannot_replay_a_completed_tool_call_id() {
    let (store, session_id) = temp_session("build");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with(EchoTool {
        calls: calls.clone(),
    });
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "echo-1".into(),
                name: "echo".into(),
                item_id: None,
            },
            tool_call_event("echo-1", "once"),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![ProviderEvent::Error("connection lost".into())],
        vec![
            ProviderEvent::ToolCallStarted {
                id: "echo-1".into(),
                name: "echo".into(),
                item_id: None,
            },
            tool_call_event("echo-1", "twice"),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
    ]);

    assert!(
        run_turn(
            &provider,
            &registry,
            &store,
            &session_id,
            "original prompt",
            &[],
            None,
            LoopConfig::default(),
            events_channel().0,
            CancellationToken::new(),
            ToolContext::root(std::env::temp_dir()),
            None,
        )
        .await
        .is_err()
    );
    // Compact the completed tool round out of the active replay window. The
    // canonical id index must still reserve its call id.
    let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
    let kept_from = session.events().len() - 1;
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "earlier work completed".into(),
            kept_from,
            ts: chrono::Utc::now(),
        })
        .unwrap();
    drop(session);

    let error = resume_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("duplicate tool call id"));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["msg"], "once");
    drop(calls);
    store.load(&session_id).expect("session remains valid");
}

#[tokio::test]
async fn transient_provider_errors_retry_with_bounded_backoff() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![
        vec![ProviderEvent::RetryableError("overloaded".into())],
        vec![ProviderEvent::RetryableError("still overloaded".into())],
        vec![
            ProviderEvent::TextDelta("ready".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
        ],
    ]);
    let (tx, mut rx) = events_channel();

    let outcome = run_turn(
        &provider,
        &ToolRegistry::read_only(),
        &store,
        &session_id,
        "hello",
        &[],
        None,
        LoopConfig {
            max_provider_retries: 2,
            provider_retry_base_delay: Duration::from_millis(10),
            provider_retry_max_delay: Duration::from_millis(15),
            ..LoopConfig::default()
        },
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(provider.requests().len(), 3);
    let mut retries = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let LoopEvent::ProviderRetry { attempt, delay, .. } = event {
            retries.push((attempt, delay));
        }
    }
    assert_eq!(
        retries,
        vec![
            (1, Duration::from_millis(10)),
            (2, Duration::from_millis(15)),
        ]
    );
}

#[tokio::test]
async fn transient_provider_retry_limit_returns_the_last_error() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::repeating(vec![vec![ProviderEvent::RetryableError(
        "service unavailable".into(),
    )]]);

    let error = run_turn(
        &provider,
        &ToolRegistry::read_only(),
        &store,
        &session_id,
        "hello",
        &[],
        None,
        LoopConfig {
            max_provider_retries: 2,
            provider_retry_base_delay: Duration::ZERO,
            ..LoopConfig::default()
        },
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("service unavailable"));
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn permanent_provider_errors_are_not_retried() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![vec![ProviderEvent::Error("invalid request".into())]]);

    let error = run_turn(
        &provider,
        &ToolRegistry::read_only(),
        &store,
        &session_id,
        "hello",
        &[],
        None,
        LoopConfig {
            max_provider_retries: 3,
            provider_retry_base_delay: Duration::ZERO,
            ..LoopConfig::default()
        },
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("invalid request"));
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn provider_backoff_is_cancellable() {
    let (store, session_id) = temp_session("build");
    let provider =
        MockProvider::repeating(vec![vec![ProviderEvent::RetryableError("offline".into())]]);
    let (tx, mut rx) = events_channel();
    let cancel = CancellationToken::new();
    let registry = ToolRegistry::read_only();
    let turn = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "hello",
        &[],
        None,
        LoopConfig {
            max_provider_retries: 3,
            provider_retry_base_delay: Duration::from_secs(60),
            ..LoopConfig::default()
        },
        tx,
        cancel.clone(),
        ToolContext::root(std::env::temp_dir()),
        None,
    );
    tokio::pin!(turn);

    loop {
        tokio::select! {
            event = rx.recv() => {
                if matches!(event, Some(LoopEvent::ProviderRetry { .. })) {
                    cancel.cancel();
                    break;
                }
            }
            result = &mut turn => panic!("turn ended before backoff: {result:?}"),
        }
    }

    assert_eq!(turn.await.unwrap(), TurnOutcome::Aborted);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn concurrent_turn_on_same_session_is_rejected_before_append() {
    struct PendingProvider {
        started: Arc<tokio::sync::Notify>,
    }
    impl Provider for PendingProvider {
        fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
            self.started.notify_one();
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    let (store, session_id) = temp_session("build");
    let provider = Arc::new(PendingProvider {
        started: Arc::new(tokio::sync::Notify::new()),
    });
    let registry = ToolRegistry::builtin();
    let cancel = CancellationToken::new();
    let first = {
        let store = store.clone();
        let session_id = session_id.clone();
        let provider = provider.clone();
        let registry = registry.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let (tx, _rx) = events_channel();
            run_turn(
                provider.as_ref(),
                &registry,
                &store,
                &session_id,
                "first",
                &[],
                None,
                LoopConfig::default(),
                tx,
                cancel,
                ToolContext::root(std::env::temp_dir()),
                None,
            )
            .await
        })
    };
    provider.started.notified().await;

    let (tx, _rx) = events_channel();
    let error = run_turn(
        provider.as_ref(),
        &registry,
        &store,
        &session_id,
        "second",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("already active"));

    cancel.cancel();
    assert_eq!(first.await.unwrap().unwrap(), TurnOutcome::Aborted);
    store.acquire_writer(&session_id).unwrap();
    let transcript = store.load(&session_id).unwrap().transcript();
    assert_eq!(transcript.len(), 1);
    assert!(matches!(
        &transcript[0].content[0],
        ContentBlock::Text { text } if text == "first"
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
            item_id: None,
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
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
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
        &[],
        None,
        LoopConfig::default(),
        tx,
        cancel,
        ToolContext::root(std::env::temp_dir()),
        None,
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
    let provider = MockProvider::new(
        (0..5)
            .map(|index| {
                let id = format!("t-{index}");
                vec![
                    ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "echo".into(),
                        item_id: None,
                    },
                    tool_call_event(&id, "again"),
                    ProviderEvent::TurnComplete {
                        stop_reason: StopReason::ToolUse,
                        usage: Default::default(),
                    },
                ]
            })
            .collect(),
    );

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
        &[],
        None,
        config,
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
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
                futures::stream::iter(vec![
                    ProviderEvent::ToolCallStarted {
                        id: "t1".into(),
                        name: "echo".into(),
                        item_id: None,
                    },
                    ProviderEvent::ToolCallCompleted {
                        id: "t1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"msg": "x"}),
                    },
                ])
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
        &[],
        None,
        LoopConfig::default(),
        tx,
        cancel,
        ToolContext::root(std::env::temp_dir()),
        None,
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
        ContentBlock::ToolResult { tool_use_id, is_error: true, content, .. }
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
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
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
    // The error itself is recorded in the session log (as a diagnostic
    // block, which never flows back to providers) so stream failures are
    // diagnosable after the fact.
    let recorded = session
        .events()
        .iter()
        .filter_map(|event| match event {
            ilar::session::SessionEvent::AssistantMessage {
                content,
                stop_reason,
                ..
            } if stop_reason == "error" => Some(content),
            _ => None,
        })
        .flatten()
        .find_map(|block| match block {
            ContentBlock::Diagnostic { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("error turn records a diagnostic block");
    assert!(
        recorded.contains("turn error: connection reset"),
        "{recorded}"
    );
}

#[tokio::test]
async fn provider_error_with_no_content_still_persists_the_error() {
    // A turn that dies before any visible content (e.g. a decode error
    // right after thinking) must still leave a diagnosable trace.
    let (store, session_id) = temp_session("build");
    let registry = ToolRegistry::builtin();
    let provider = MockProvider::new(vec![vec![ProviderEvent::Error(
        "unknown OpenAI-compatible finish reason \"weird\" · offending event: {…}".into(),
    )]]);

    let (tx, _rx) = events_channel();
    let result = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "hello",
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await;

    assert!(result.is_err());
    let session = store.load(&session_id).unwrap();
    let error_turns: Vec<_> = session
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event,
                ilar::session::SessionEvent::AssistantMessage { stop_reason, .. }
                    if stop_reason == "error"
            )
        })
        .collect();
    assert_eq!(error_turns.len(), 1, "{error_turns:?}");
    let rendered = format!("{error_turns:?}");
    assert!(rendered.contains("offending event"), "{rendered}");
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
        &[],
        None,
        LoopConfig::default(),
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
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

// ---- steering ----

fn echo_call(id: &str, msg: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallStarted {
            id: id.into(),
            name: "echo".into(),
            item_id: None,
        },
        ProviderEvent::ToolCallCompleted {
            id: id.into(),
            name: "echo".into(),
            input: serde_json::json!({ "msg": msg }),
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Default::default(),
        },
    ]
}

fn plain_turn(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        },
    ]
}

/// Sends a steer the moment it is invoked, standing in for the user
/// typing while a tool runs.
struct SteerOnCallTool {
    steer: ilar::agent::SteerSender,
}

impl Tool for SteerOnCallTool {
    fn name(&self) -> &'static str {
        "slow_work"
    }
    fn description(&self) -> &'static str {
        "test tool that steers mid-turn"
    }
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Barrier
    }
    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn run(&self, _input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let _ = self.steer.send("actually, look at the other file".into());
        Box::pin(async move { ToolOutput::text("worked") })
    }
}

/// Emits one steer while producing its first response, standing in for
/// the user typing as the model is wrapping up.
struct SteerWhileRespondingProvider {
    steer: ilar::agent::SteerSender,
    calls: AtomicUsize,
    text: String,
}

impl Provider for SteerWhileRespondingProvider {
    fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = self.steer.send(self.text.clone());
        }
        Ok(Box::pin(futures::stream::iter(plain_turn("done"))))
    }
}

/// A message sent while the model is mid-tool-loop must reach it at the
/// next step, not after the whole turn ends. Waiting for the turn is the
/// behaviour steering exists to replace.
#[tokio::test]
async fn a_steer_reaches_the_model_at_the_next_step() {
    let (store, session_id) = temp_session("build");
    let (steer, steer_rx) = ilar::agent::steer_channel();
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "call-1".into(),
                name: "slow_work".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "call-1".into(),
                name: "slow_work".into(),
                input: serde_json::json!({}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        echo_call("call-2", "second"),
        plain_turn("done"),
    ]);
    let registry = ToolRegistry::builtin()
        .with_tool(Arc::new(SteerOnCallTool {
            steer: steer.clone(),
        }))
        .unwrap()
        .with_tool(Arc::new(EchoTool {
            calls: Arc::new(Mutex::new(Vec::new())),
        }))
        .unwrap();

    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "start the task",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        Some(steer_rx),
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    let requests = provider.requests();
    let first = format!("{:?}", requests[0].messages);
    let second = format!("{:?}", requests[1].messages);
    assert!(
        !first.contains("actually, look at the other file"),
        "the steer had not been sent yet: {first}"
    );
    assert!(
        second.contains("actually, look at the other file"),
        "the steer should reach the very next step: {second}"
    );
    // The steer merges into the tool-result user message, so block order
    // is the real invariant: providers require tool results to lead.
    let transcript = store.load(&session_id).unwrap().transcript();
    for pair in transcript.windows(2) {
        assert_ne!(pair[0].role, pair[1].role);
    }
    let merged = transcript
        .iter()
        .find(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
        .expect("a user message carrying tool results");
    let kinds: Vec<&str> = merged
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult { .. } => "tool_result",
            ContentBlock::Text { .. } => "text",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["tool_result", "text"],
        "the steer must follow the tool results, not precede them"
    );
}

/// The reopen decision must come from the drain, not from asking the
/// channel whether it is empty: a whitespace-only steer is counted as
/// pending but then discarded, which reopened the turn with nothing to
/// add and appended a second assistant message with no user message
/// between them — permanently breaking role alternation.
#[tokio::test]
async fn a_steer_that_drains_to_nothing_does_not_reopen_the_turn() {
    let (store, session_id) = temp_session("build");
    let (steer, steer_rx) = ilar::agent::steer_channel();
    let provider = SteerWhileRespondingProvider {
        steer: steer.clone(),
        calls: AtomicUsize::new(0),
        text: "   ".into(),
    };

    let outcome = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "do the thing",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        Some(steer_rx),
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "a steer with no content must not reopen the turn"
    );
    for pair in store.load(&session_id).unwrap().transcript().windows(2) {
        assert_ne!(pair[0].role, pair[1].role);
    }
}

/// A steer arriving as the model stops should reopen the turn rather
/// than being stranded until the user sends something else.
#[tokio::test]
async fn a_steer_reopens_a_finishing_turn() {
    let (store, session_id) = temp_session("build");
    let (steer, steer_rx) = ilar::agent::steer_channel();
    let provider = SteerWhileRespondingProvider {
        steer,
        calls: AtomicUsize::new(0),
        text: "one more thing".into(),
    };

    let outcome = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "do the thing",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        Some(steer_rx),
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "the steer arriving as the model stopped should have reopened the turn"
    );
    let rendered = format!("{:?}", store.load(&session_id).unwrap().transcript());
    assert!(rendered.contains("one more thing"), "{rendered}");
}

/// Without a steer, a turn that stops stays stopped.
#[tokio::test]
async fn no_steer_leaves_turn_completion_alone() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![plain_turn("done")]);
    let (_steer, steer_rx) = ilar::agent::steer_channel();

    let outcome = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "do the thing",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        Some(steer_rx),
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn structured_question_suspends_and_continues_with_answer_result() {
    let (store, session_id) = temp_session("build");
    let input = serde_json::json!({"questions": [{"id": "language", "type": "single_choice", "prompt": "Language?", "required": true, "allow_other": true, "options": [{"id": "rust", "label": "Rust"}]}]});
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "q-1".into(),
                name: "question".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "q-1".into(),
                name: "question".into(),
                input,
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        }],
    ]);
    let (question_tx, mut question_rx) = ilar::question::question_channel(1);
    let registry = ToolRegistry::builtin().with_questions(question_tx);
    let turn = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "start",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    );
    tokio::pin!(turn);
    let prompt = tokio::select! { prompt = question_rx.recv() => prompt.unwrap(), result = &mut turn => panic!("turn did not suspend: {result:?}") };
    assert_eq!(prompt.tool_call_id, "q-1");
    prompt
        .reply
        .send(ilar::question::QuestionResponse::Answered {
            answers: vec![ilar::question::QuestionAnswer::SingleChoice {
                question_id: "language".into(),
                option_id: Some("rust".into()),
                other: None,
            }],
        })
        .unwrap();
    assert_eq!(turn.await.unwrap(), TurnOutcome::Completed);
    let events = store.audit_events(&session_id).unwrap();
    let result = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } if tool_use_id == "q-1" => Some((content, is_error)),
            _ => None,
        })
        .unwrap();
    assert!(!result.1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.0).unwrap()["status"],
        "answered"
    );
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn mixed_question_batch_is_rejected_before_tool_effects() {
    let (store, session_id) = temp_session("build");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (question_tx, _question_rx) = ilar::question::question_channel(1);
    let registry = registry_with(EchoTool {
        calls: calls.clone(),
    })
    .with_questions(question_tx);
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "q-1".into(),
                name: "question".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "q-1".into(),
                name: "question".into(),
                input: serde_json::json!({"questions": [{"id":"x","type":"free_text","prompt":"X?","required":true}]}),
            },
            ProviderEvent::ToolCallStarted {
                id: "echo-1".into(),
                name: "echo".into(),
                item_id: None,
            },
            tool_call_event("echo-1", "must not run"),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        }],
    ]);
    run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "start",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();
    assert!(calls.lock().unwrap().is_empty());
    let errors = store
        .audit_events(&session_id)
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event, SessionEvent::ToolResult { is_error: true, .. }))
        .count();
    assert_eq!(errors, 2);
}

#[tokio::test]
async fn pending_question_resumes_without_a_new_user_message() {
    let (store, session_id) = temp_session("build");
    {
        let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
        session
            .append(SessionEvent::UserMessage {
                id: new_id(),
                text: "start".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session.append(SessionEvent::AssistantMessage { id: new_id(), model: "zai/glm-4.7".into(), content: vec![ContentBlock::ToolCall { id: "q-pending".into(), name: "question".into(), input: serde_json::json!({"questions": [{"id":"details","type":"free_text","prompt":"Details?","required":true}]}), item_id: None }], usage: Default::default(), stop_reason: "tool_use".into(), ts: chrono::Utc::now() }).unwrap();
    }
    assert!(
        store
            .load(&session_id)
            .unwrap()
            .pending_question()
            .is_some()
    );
    let provider = MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
        stop_reason: StopReason::EndTurn,
        usage: Default::default(),
    }]]);
    let (question_tx, _question_rx) = ilar::question::question_channel(1);
    let registry = ToolRegistry::builtin().with_questions(question_tx);
    resume_pending_question(
        &provider,
        &registry,
        &store,
        &session_id,
        ilar::question::QuestionResponse::Answered {
            answers: vec![ilar::question::QuestionAnswer::FreeText {
                question_id: "details".into(),
                text: "done".into(),
            }],
        },
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();
    let events = store.audit_events(&session_id).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::UserMessage { .. }))
            .count(),
        1
    );
    assert!(events.iter().any(|event| matches!(event, SessionEvent::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "q-pending")));
}

#[tokio::test]
async fn structured_question_cancellation_is_a_successful_tool_result() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "q-cancel".into(),
                name: "question".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "q-cancel".into(),
                name: "question".into(),
                input: serde_json::json!({"questions": [{"id":"confirm","type":"free_text","prompt":"Continue?","required":true}]}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        }],
    ]);
    let (question_tx, mut question_rx) = ilar::question::question_channel(1);
    let registry = ToolRegistry::builtin().with_questions(question_tx);
    let turn = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "start",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    );
    tokio::pin!(turn);
    let prompt = tokio::select! { prompt = question_rx.recv() => prompt.unwrap(), result = &mut turn => panic!("turn did not suspend: {result:?}") };
    prompt
        .reply
        .send(ilar::question::QuestionResponse::Cancelled)
        .unwrap();
    assert_eq!(turn.await.unwrap(), TurnOutcome::Completed);
    assert!(store.audit_events(&session_id).unwrap().iter().any(|event| matches!(event, SessionEvent::ToolResult { tool_use_id, content, is_error: false, .. } if tool_use_id == "q-cancel" && content == r#"{"status":"cancelled"}"#)));
}

#[tokio::test]
async fn question_delivery_backpressure_is_cancellable_and_preserves_pending_call() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ToolCallStarted {
            id: "q-wait".into(),
            name: "question".into(),
            item_id: None,
        },
        ProviderEvent::ToolCallCompleted {
            id: "q-wait".into(),
            name: "question".into(),
            input: serde_json::json!({"questions": [{"id":"x","type":"free_text","prompt":"X?","required":true}]}),
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Default::default(),
        },
    ]]);
    let (question_tx, _question_rx) = ilar::question::question_channel(1);
    let (dummy_reply, _dummy_rx) = tokio::sync::oneshot::channel();
    question_tx
        .send(ilar::question::QuestionPrompt {
            session_id: "session".into(),
            tool_call_id: "dummy".into(),
            request: ilar::question::QuestionRequest { questions: vec![] },
            reply: dummy_reply,
        })
        .await
        .unwrap();
    let registry = ToolRegistry::builtin().with_questions(question_tx);
    let cancel = CancellationToken::new();
    let turn = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "start",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        cancel.clone(),
        ToolContext::root(std::env::temp_dir()),
        None,
    );
    tokio::pin!(turn);
    tokio::select! {
        result = &mut turn => panic!("turn ended before cancellation: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
    }
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), &mut turn)
            .await
            .unwrap()
            .unwrap(),
        TurnOutcome::Aborted
    );
    assert_eq!(
        store
            .load(&session_id)
            .unwrap()
            .pending_question()
            .unwrap()
            .tool_call_id,
        "q-wait"
    );
}

#[tokio::test]
async fn a_new_turn_cannot_overwrite_a_pending_question() {
    let (store, session_id) = temp_session("build");
    {
        let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
        session
            .append(SessionEvent::UserMessage {
                id: new_id(),
                text: "start".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session.append(SessionEvent::AssistantMessage { id: new_id(), model: "zai/glm-4.7".into(), content: vec![ContentBlock::ToolCall { item_id: None, id: "q-existing".into(), name: "question".into(), input: serde_json::json!({"questions": [{"id":"x","type":"free_text","prompt":"X?","required":true}]}) }], usage: Default::default(), stop_reason: "tool_use".into(), ts: chrono::Utc::now() }).unwrap();
    }
    let provider = MockProvider::new(vec![]);
    let error = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "new message",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("pending question"));
    let reader = store.load(&session_id).unwrap();
    assert_eq!(
        reader.pending_question().unwrap().tool_call_id,
        "q-existing"
    );
    assert_eq!(
        reader
            .events()
            .iter()
            .filter(|event| matches!(event, SessionEvent::UserMessage { .. }))
            .count(),
        1
    );
}

fn scratch_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    std::fs::create_dir(&root).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    (temp, root)
}

fn text_only_provider(text: &str) -> MockProvider {
    MockProvider::new(vec![vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        },
    ]])
}

#[tokio::test]
async fn root_turn_in_a_git_repo_checkpoints_before_the_user_message() {
    let (store, session_id) = temp_session("build");
    let (_temp, root) = scratch_repo();
    let provider = text_only_provider("done");
    let registry = ToolRegistry::builtin();

    run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "start",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(root.clone()),
        None,
    )
    .await
    .unwrap();

    let events = store.load(&session_id).unwrap().events().to_vec();
    let checkpoints: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(event, SessionEvent::Checkpoint { .. }).then_some(index)
        })
        .collect();
    let [checkpoint] = checkpoints[..] else {
        panic!("expected exactly one checkpoint, got {checkpoints:?}");
    };
    assert!(matches!(
        events[checkpoint + 1],
        SessionEvent::UserMessage { .. }
    ));
    let SessionEvent::Checkpoint { commit, .. } = &events[checkpoint] else {
        unreachable!()
    };
    // The snapshot is a real commit in the workspace repository.
    let verified = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .status()
        .unwrap();
    assert!(verified.success());
}

#[tokio::test]
async fn turn_outside_a_git_repo_records_no_checkpoint() {
    let (store, session_id) = temp_session("build");
    let provider = text_only_provider("done");
    let registry = ToolRegistry::builtin();

    run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "start",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    let events = store.load(&session_id).unwrap().events().to_vec();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::Checkpoint { .. }))
    );
}

#[tokio::test]
async fn child_session_turns_never_checkpoint_even_without_a_call_id() {
    // Notification turns on child sessions run with depth > 0 but no
    // call_id; `call_id` alone must not be mistaken for a root test.
    let (store, session_id) = temp_session("explore");
    let (_temp, root) = scratch_repo();
    let provider = text_only_provider("noted");
    let registry = ToolRegistry::builtin();
    let mut ctx = ToolContext::root(root.clone());
    ctx.depth = 1;

    run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "background task finished",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ctx,
        None,
    )
    .await
    .unwrap();

    let events = store.load(&session_id).unwrap().events().to_vec();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::Checkpoint { .. }))
    );
    let no_ref = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args([
            "rev-parse",
            "--verify",
            &format!("refs/ilar/checkpoints/{session_id}"),
        ])
        .output()
        .unwrap();
    assert!(!no_ref.status.success());
}

// ------------------------------------------------------- live scratch

/// A scripted provider that paces its events, so a test can watch the
/// live scratch grow between them instead of after the whole step.
struct PacedProvider {
    steps: Arc<Mutex<std::collections::VecDeque<Vec<ProviderEvent>>>>,
    gap: Duration,
}

impl PacedProvider {
    fn new(steps: Vec<Vec<ProviderEvent>>, gap: Duration) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into())),
            gap,
        }
    }
}

impl Provider for PacedProvider {
    fn stream(&self, _request: Request) -> anyhow::Result<EventStream> {
        let events = self.steps.lock().unwrap().pop_front().unwrap_or_default();
        let gap = self.gap;
        Ok(Box::pin(futures::stream::iter(events).then(
            move |event| async move {
                tokio::time::sleep(gap).await;
                event
            },
        )))
    }
}

/// Every distinct state the scratch file passed through, sampled far
/// faster than the turn writes. A file that is gone, empty or torn is
/// simply not a state — `parse_scratch` drops incomplete lines.
async fn sample_scratch(
    path: std::path::PathBuf,
    stop: CancellationToken,
) -> Vec<Vec<ilar::session::LiveDelta>> {
    let mut seen: Vec<Vec<ilar::session::LiveDelta>> = Vec::new();
    while !stop.is_cancelled() {
        if let Ok(bytes) = std::fs::read(&path) {
            let deltas = ilar::session::parse_scratch(&bytes);
            if !deltas.is_empty() && seen.last() != Some(&deltas) {
                seen.push(deltas);
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    seen
}

fn step_of(snapshot: &[ilar::session::LiveDelta]) -> u64 {
    match snapshot.first() {
        Some(ilar::session::LiveDelta::TurnStarted { step, .. }) => *step,
        other => panic!("every generation opens with turn_started, got {other:?}"),
    }
}

fn scratch_path(store: &SessionStore, session_id: &str) -> std::path::PathBuf {
    ilar::session::live_path(&store.session_path(session_id).unwrap())
}

/// The writer's whole lifecycle across a scripted two-step turn: deltas
/// reach the file while the first step is still streaming, the step
/// commit resets it to a new generation naming the tool now running, and
/// the end of the turn takes the file with it.
#[tokio::test]
async fn a_live_scratch_streams_a_turn_and_is_deleted_at_the_end() {
    use ilar::session::LiveDelta;

    let (store, session_id) = temp_session("build");
    let registry = registry_with(EchoTool {
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let provider = PacedProvider::new(
        vec![
            vec![
                ProviderEvent::TextDelta("hello ".into()),
                ProviderEvent::ThinkingDelta("hmm".into()),
                ProviderEvent::TextDelta("world".into()),
                ProviderEvent::ToolCallStarted {
                    id: "echo-1".into(),
                    name: "echo".into(),
                    item_id: None,
                },
                tool_call_event("echo-1", "ping"),
                ProviderEvent::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                    usage: Default::default(),
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                    usage: Default::default(),
                },
            ],
        ],
        Duration::from_millis(80),
    );

    let scratch = scratch_path(&store, &session_id);
    let stop = CancellationToken::new();
    let sampler = tokio::spawn(sample_scratch(scratch.clone(), stop.clone()));

    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "say hello",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();
    stop.cancel();
    let seen = sampler.await.unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(!scratch.exists(), "the scratch outlived its turn");
    assert!(seen.len() >= 2, "nothing was ever streamed: {seen:?}");

    // The first generation, mid-step: text and thinking, batched but on
    // disk long before the assistant message is committed.
    let streaming = seen
        .iter()
        .filter(|snapshot| step_of(snapshot) == 0)
        .max_by_key(|snapshot| snapshot.len())
        .expect("a first-generation snapshot");
    assert_eq!(
        &streaming[1..],
        [
            LiveDelta::TextDelta {
                text: "hello ".into()
            },
            LiveDelta::ThinkingDelta { text: "hmm".into() },
            LiveDelta::TextDelta {
                text: "world".into()
            },
            LiveDelta::ToolStarted {
                id: "echo-1".into(),
                name: "echo".into(),
                summary: "msg=ping".into(),
            },
        ],
        "everything the step streamed, in order, behind its generation line"
    );

    // The commit truncated it: the next generation carries the tool the
    // turn went off to run, and none of the text that is now committed.
    let running = seen
        .iter()
        .filter(|snapshot| step_of(snapshot) > 0)
        .max_by_key(|snapshot| snapshot.len())
        .expect("a post-commit snapshot");
    assert_eq!(step_of(running), 1);
    assert!(
        running.contains(&LiveDelta::ToolStarted {
            id: "echo-1".into(),
            name: "echo".into(),
            summary: "msg=ping".into(),
        }),
        "the running tool is the turn's activity: {running:?}"
    );
    assert!(
        running.contains(&LiveDelta::ToolFinished {
            id: "echo-1".into(),
            ok: true,
        }),
        "{running:?}"
    );
    // The committed step's own deltas, specifically: a later generation
    // legitimately carries the *next* step's, which are not committed
    // yet.
    let committed = ["hello ", "world", "hmm"];
    assert!(
        seen.iter()
            .filter(|snapshot| step_of(snapshot) > 0)
            .all(|snapshot| !snapshot.iter().any(|delta| match delta {
                LiveDelta::TextDelta { text } | LiveDelta::ThinkingDelta { text } =>
                    committed.contains(&text.as_str()),
                _ => false,
            })),
        "a committed step's deltas survived the truncate: {seen:?}"
    );

    // And the turn itself is untouched by any of it: the committed
    // message carries every delta the scratch only ever hinted at.
    let text: String = store
        .load(&session_id)
        .unwrap()
        .events()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::AssistantMessage { content, .. } => Some(content.clone()),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello worlddone", "both steps landed in full");
}

/// Successive thoughts are separate thoughts, and the scratch has to
/// say where one ends: a reader with no boundary to split on renders a
/// step's every summary as one run-on paragraph.
#[tokio::test]
async fn successive_reasoning_summaries_are_broken_apart_on_the_scratch() {
    use ilar::session::LiveDelta;

    let (store, session_id) = temp_session("build");
    let registry = registry_with(EchoTool {
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let provider = PacedProvider::new(
        vec![
            vec![
                ProviderEvent::ReasoningSummaryDelta("**Planning**".into()),
                ProviderEvent::ReasoningSummaryCompleted,
                ProviderEvent::ReasoningSummaryDelta("**Checking**".into()),
                ProviderEvent::ReasoningSummaryCompleted,
                ProviderEvent::ThinkingDelta("raw".into()),
                ProviderEvent::ThinkingCompleted,
                ProviderEvent::ToolCallStarted {
                    id: "echo-1".into(),
                    name: "echo".into(),
                    item_id: None,
                },
                tool_call_event("echo-1", "ping"),
                ProviderEvent::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                    usage: Default::default(),
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                    usage: Default::default(),
                },
            ],
        ],
        Duration::from_millis(40),
    );

    let scratch = scratch_path(&store, &session_id);
    let stop = CancellationToken::new();
    let sampler = tokio::spawn(sample_scratch(scratch.clone(), stop.clone()));

    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "say hello",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();
    stop.cancel();
    let seen = sampler.await.unwrap();
    assert_eq!(outcome, TurnOutcome::Completed);

    let streaming = seen
        .iter()
        .filter(|snapshot| step_of(snapshot) == 0)
        .max_by_key(|snapshot| snapshot.len())
        .expect("a first-generation snapshot");
    assert_eq!(
        &streaming[1..],
        [
            LiveDelta::ThinkingDelta {
                text: "**Planning**".into()
            },
            LiveDelta::ThinkingBreak,
            LiveDelta::ThinkingDelta {
                text: "**Checking**".into()
            },
            LiveDelta::ThinkingBreak,
            LiveDelta::ThinkingDelta { text: "raw".into() },
            LiveDelta::ThinkingBreak,
            LiveDelta::ToolStarted {
                id: "echo-1".into(),
                name: "echo".into(),
                summary: "msg=ping".into(),
            },
        ],
        "each thought is closed where the provider closed it"
    );
}

/// An aborted turn is still a turn that ended: the drop guard runs on
/// the way out, whatever the outcome.
#[tokio::test]
async fn an_aborted_turn_still_deletes_its_scratch() {
    let (store, session_id) = temp_session("build");
    let registry = ToolRegistry::builtin();
    let provider = SlowProvider {
        first: Arc::new(Mutex::new(true)),
    };
    let scratch = scratch_path(&store, &session_id);
    let stop = CancellationToken::new();
    let sampler = tokio::spawn(sample_scratch(scratch.clone(), stop.clone()));

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel_clone.cancel();
    });
    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "slow request",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        cancel,
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();
    stop.cancel();
    let seen = sampler.await.unwrap();

    assert_eq!(outcome, TurnOutcome::Aborted);
    assert!(!seen.is_empty(), "the scratch never existed to begin with");
    assert!(!scratch.exists(), "an aborted turn left its scratch behind");
}

/// The failure policy, end to end: a scratch that cannot be written is
/// not a turn that cannot run. The unwritable path is a directory where
/// the file belongs — what a full or read-only disk looks like from
/// inside the loop.
#[tokio::test]
async fn a_scratch_that_cannot_be_written_never_disturbs_the_turn() {
    let (store, session_id) = temp_session("build");
    let scratch = scratch_path(&store, &session_id);
    std::fs::create_dir_all(&scratch).unwrap();

    let registry = registry_with(EchoTool {
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let provider = MockProvider::new(vec![echo_call("echo-1", "ping"), plain_turn("all done")]);
    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "say hello",
        &[],
        None,
        LoopConfig::default(),
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(scratch.is_dir(), "the loop tried to delete what it found");
    let transcript = store.load(&session_id).unwrap().transcript();
    assert!(matches!(
        transcript.last().map(|message| &message.content[0]),
        Some(ContentBlock::Text { text }) if text == "all done"
    ));
}

/// A tool that outlasts several heartbeats, reporting what the scratch
/// looked like on the way in and on the way out.
#[derive(Clone)]
struct SlowTool {
    scratch: std::path::PathBuf,
    seen: Arc<Mutex<Vec<(u64, std::time::SystemTime)>>>,
}

impl SlowTool {
    fn observe(&self) {
        let metadata = std::fs::metadata(&self.scratch).expect("the turn's scratch");
        self.seen
            .lock()
            .unwrap()
            .push((metadata.len(), metadata.modified().unwrap()));
    }

    /// The loop announces a tool in the scratch from its own task, so a
    /// tool that looked immediately would be racing that write — and the
    /// claim under test is about what *heartbeats* add, not markers.
    async fn once_announced(&self) {
        for _ in 0..400 {
            let announced =
                ilar::session::parse_scratch(&std::fs::read(&self.scratch).unwrap_or_default())
                    .iter()
                    .any(|delta| matches!(delta, ilar::session::LiveDelta::ToolStarted { .. }));
            if announced {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the turn never announced the running tool");
    }
}

impl Tool for SlowTool {
    fn name(&self) -> &'static str {
        "slow"
    }
    fn description(&self) -> &'static str {
        "takes its time"
    }
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }
    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn run(&self, _input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let tool = self.clone();
        Box::pin(async move {
            tool.once_announced().await;
            tool.observe();
            tokio::time::sleep(HEARTBEAT * 6).await;
            tool.observe();
            ToolOutput::text("eventually")
        })
    }
}

/// Short enough to watch, long enough that the sleep above spans several
/// of them. The shipped interval is `SCRATCH_HEARTBEAT`.
const HEARTBEAT: Duration = Duration::from_millis(40);

/// The complaint this whole feature exists to answer: a turn three
/// minutes into a `cargo test` must not read as dead. The tool watches
/// its own turn's scratch, so nothing here races a sampler, and the
/// interval is a config value, so the minutes cost milliseconds.
#[tokio::test]
async fn a_long_tool_run_keeps_the_scratch_alive_without_writing_to_it() {
    let (store, session_id) = temp_session("build");
    let tool = SlowTool {
        scratch: scratch_path(&store, &session_id),
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let registry = ToolRegistry::builtin()
        .with_tool(Arc::new(tool.clone()))
        .unwrap();
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "slow-1".into(),
                name: "slow".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "slow-1".into(),
                name: "slow".into(),
                input: serde_json::json!({}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        plain_turn("that took a while"),
    ]);

    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "run the slow thing",
        &[],
        None,
        LoopConfig {
            live_heartbeat: HEARTBEAT,
            ..LoopConfig::default()
        },
        events_channel().0,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    let seen = tool.seen.lock().unwrap().clone();
    let [(entered_len, entered_at), (left_len, left_at)] = seen[..] else {
        panic!("the tool observed its scratch twice, got {seen:?}");
    };
    assert!(
        left_at > entered_at,
        "the scratch went stale under a running tool: {entered_at:?} → {left_at:?}"
    );
    assert_eq!(
        left_len, entered_len,
        "a heartbeat wrote something a reader would have to parse"
    );
}
