use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use ilar::agent::{
    LOOP_EVENT_CAPACITY, LoopConfig, LoopEvent, LoopEventReceiver, LoopEventSender, TurnOutcome,
    loop_event_channel, resume_pending_question, run_turn,
};
use ilar::provider::zai::{Flavor, ZaiProvider};
use ilar::provider::{EventStream, MockProvider, Provider, ProviderEvent, Request, StopReason};
use ilar::session::{ContentBlock, SessionEvent, SessionMeta, SessionStore, new_id};
use ilar::todo::Status as TodoStatus;
use ilar::tools::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, ToolRegistry, WorkspaceAccess,
};
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
            workspace: None,
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
            },
            ProviderEvent::ToolCallCompleted {
                id: "todo-1".into(),
                name: "todo".into(),
                input: serde_json::json!({"todos": [{"content": "first", "status": "in_progress"}]}),
            },
            ProviderEvent::ToolCallStarted {
                id: "todo-2".into(),
                name: "todo".into(),
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
async fn paused_turn_is_reissued_without_persisting_an_assistant_step() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::TextDelta("before pause".into()),
            ProviderEvent::ResponseContent {
                provider: "zai-anthropic".into(),
                content: serde_json::json!([{
                    "type": "server_tool_use",
                    "id": "srv_1",
                    "name": "web_search",
                    "input": {"query": "news"}
                }]),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::Paused,
                usage: Default::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("after pause".into()),
            ProviderEvent::ResponseContent {
                provider: "zai-anthropic".into(),
                content: serde_json::json!([{"type": "text", "text": "after pause"}]),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
        ],
    ]);
    let (tx, _rx) = events_channel();

    let outcome = run_turn(
        &provider,
        &ToolRegistry::read_only(),
        &store,
        &session_id,
        "go",
        None,
        LoopConfig {
            max_iterations: 1,
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
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(provider.requests()[1].continuations.len(), 1);
    let transcript = store.load(&session_id).unwrap().transcript();
    assert_eq!(transcript.len(), 2, "{transcript:?}");
    assert!(format!("{transcript:?}").contains("before pause"));
}

#[tokio::test]
async fn paused_turn_retry_cap_is_finite() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::repeating(vec![vec![
        ProviderEvent::ResponseContent {
            provider: "zai-anthropic".into(),
            content: serde_json::json!([{
                "type": "server_tool_use",
                "id": "srv_1",
                "name": "web_search",
                "input": {"query": "news"}
            }]),
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::Paused,
            usage: Default::default(),
        },
    ]]);
    let (tx, _rx) = events_channel();

    let error = run_turn(
        &provider,
        &ToolRegistry::read_only(),
        &store,
        &session_id,
        "go",
        None,
        LoopConfig {
            max_pause_retries: 2,
            ..LoopConfig::default()
        },
        tx,
        CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("pause retry limit"), "{error:#}");
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn resumed_max_tokens_does_not_require_complete_replay_content() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::TextDelta("before pause".into()),
            ProviderEvent::ResponseContent {
                provider: "zai-anthropic".into(),
                content: serde_json::json!([{
                    "type": "server_tool_use",
                    "id": "srv_1",
                    "name": "web_search",
                    "input": {"query": "news"}
                }]),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::Paused,
                usage: Default::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("truncated".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::MaxTokens,
                usage: Default::default(),
            },
        ],
    ]);
    let (tx, _rx) = events_channel();

    let outcome = run_turn(
        &provider,
        &ToolRegistry::read_only(),
        &store,
        &session_id,
        "go",
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
    let transcript = store.load(&session_id).unwrap().transcript();
    assert!(format!("{transcript:?}").contains("truncated"));
}

#[tokio::test]
async fn resumed_tool_use_persists_replay_before_tool_results() {
    let (store, session_id) = temp_session("build");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = registry_with(EchoTool {
        calls: calls.clone(),
    });
    let first = serde_json::json!([{
        "type": "server_tool_use",
        "id": "srv_1",
        "name": "web_search",
        "input": {"query": "news"}
    }]);
    let resumed = serde_json::json!([
        {
            "type": "web_search_tool_result",
            "tool_use_id": "srv_1",
            "content": []
        },
        {
            "type": "tool_use",
            "id": "client_1",
            "name": "echo",
            "input": {"msg": "result"}
        }
    ]);
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ResponseContent {
                provider: "zai-anthropic".into(),
                content: first.clone(),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::Paused,
                usage: Default::default(),
            },
        ],
        vec![
            ProviderEvent::ToolCallStarted {
                id: "client_1".into(),
                name: "echo".into(),
            },
            tool_call_event("client_1", "result"),
            ProviderEvent::ResponseContent {
                provider: "zai-anthropic".into(),
                content: resumed.clone(),
            },
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

    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "go",
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
    assert_eq!(calls.lock().unwrap().len(), 1);
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].continuations.is_empty());
    let body =
        ZaiProvider::new("k".into(), None, Flavor::Anthropic).wire_body_for_test(&requests[2]);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"][0], first[0]);
    assert_eq!(body["messages"][1]["content"][1], resumed[0]);
    assert_eq!(body["messages"][1]["content"][2], resumed[1]);
    assert_eq!(body["messages"][2]["role"], "user");
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
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
            ProviderEvent::ThinkingCompleted {
                signature: Some("sig-plan".into()),
            },
            ProviderEvent::TextDelta("checking".into()),
            ProviderEvent::ToolCallStarted {
                id: "t1".into(),
                name: "echo".into(),
            },
            tool_call_event("t1", "alpha"),
            ProviderEvent::TextDelta("after first".into()),
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
        matches!(&assistant1.content[0], ContentBlock::Thinking { text, .. } if text == "plan")
    );
    assert!(matches!(&assistant1.content[1], ContentBlock::Text { text } if text == "checking"));
    assert!(matches!(&assistant1.content[2], ContentBlock::ToolCall { id, .. } if id == "t1"));
    assert!(matches!(&assistant1.content[3], ContentBlock::Text { text } if text == "after first"));
    assert!(matches!(&assistant1.content[4], ContentBlock::ToolCall { id, .. } if id == "t2"));
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
async fn multiple_thinking_runs_preserve_order_and_signatures() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ThinkingDelta("first thought".into()),
        ProviderEvent::ThinkingCompleted {
            signature: Some("sig-1".into()),
        },
        ProviderEvent::TextDelta("between".into()),
        ProviderEvent::ThinkingDelta("second thought".into()),
        ProviderEvent::ThinkingCompleted {
            signature: Some("sig-2".into()),
        },
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
    assert!(
        matches!(&content[0], ContentBlock::Thinking { text, signature: Some(signature) }
        if text == "first thought" && signature == "sig-1")
    );
    assert!(matches!(&content[1], ContentBlock::Text { text } if text == "between"));
    assert!(
        matches!(&content[2], ContentBlock::Thinking { text, signature: Some(signature) }
        if text == "second thought" && signature == "sig-2")
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
            .all(|block| matches!(block, ContentBlock::Diagnostic { text } if text.contains("turn error"))),
        "{transcript:?}"
    );
}

#[tokio::test]
async fn unsigned_thinking_is_persisted_as_diagnostic_text() {
    let (store, session_id) = temp_session("build");
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::ThinkingDelta("unfinished".into()),
        ProviderEvent::ThinkingCompleted { signature: None },
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
    assert!(matches!(&content[0], ContentBlock::Diagnostic { text }
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
    let provider = MockProvider::repeating(vec![vec![
        ProviderEvent::ToolCallStarted {
            id: "t".into(),
            name: "echo".into(),
        },
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
            ContentBlock::Diagnostic { text } => Some(text.clone()),
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
            },
            ProviderEvent::ToolCallCompleted {
                id: "q-1".into(),
                name: "question".into(),
                input: serde_json::json!({"questions": [{"id":"x","type":"free_text","prompt":"X?","required":true}]}),
            },
            ProviderEvent::ToolCallStarted {
                id: "echo-1".into(),
                name: "echo".into(),
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
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session.append(SessionEvent::AssistantMessage { id: new_id(), model: "zai/glm-4.7".into(), content: vec![ContentBlock::ToolCall { id: "q-pending".into(), name: "question".into(), input: serde_json::json!({"questions": [{"id":"details","type":"free_text","prompt":"Details?","required":true}]}) }], usage: Default::default(), stop_reason: "tool_use".into(), ts: chrono::Utc::now() }).unwrap();
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
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session.append(SessionEvent::AssistantMessage { id: new_id(), model: "zai/glm-4.7".into(), content: vec![ContentBlock::ToolCall { id: "q-existing".into(), name: "question".into(), input: serde_json::json!({"questions": [{"id":"x","type":"free_text","prompt":"X?","required":true}]}) }], usage: Default::default(), stop_reason: "tool_use".into(), ts: chrono::Utc::now() }).unwrap();
    }
    let provider = MockProvider::new(vec![]);
    let error = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "new message",
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
