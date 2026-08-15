use std::sync::Arc;
use std::time::Duration;

use futures::stream;
use ilar::agent::{LoopConfig, TurnOutcome, run_turn};
use ilar::config::AgentDefinition;
use ilar::provider::{
    EventStream, FixedProviderResolver, MockProvider, Provider, ProviderEvent, Request, StopReason,
};
use ilar::session::{ContentBlock, SessionMeta, SessionStore, Usage, new_id};
use ilar::subagent::SubagentSpawner;
use ilar::tools::{ToolContext, ToolRegistry};

fn temp_store() -> (SessionStore, String) {
    let dir = std::env::temp_dir().join(format!("ilar-bg-test-{}", new_id()));
    let store = SessionStore::new(dir);
    let id = new_id();
    store
        .create(SessionMeta {
            session_id: id.clone(),
            parent_id: None,
            agent: "build".into(),
            model: "zai/glm-4.7".into(),
        })
        .unwrap();
    (store, id)
}

/// Streams a text turn after `delay_ms`.
#[derive(Clone)]
struct DelayedText {
    text: &'static str,
    delay_ms: u64,
}

impl Provider for DelayedText {
    fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
        let text = self.text.to_string();
        let delay = Duration::from_millis(self.delay_ms);
        Ok(Box::pin(stream::unfold(
            Some((delay, text)),
            |state| async move {
                match state {
                    Some((delay, text)) => {
                        tokio::time::sleep(delay).await;
                        Some((ProviderEvent::TextDelta(text), None))
                    }
                    None => Some((
                        ProviderEvent::TurnComplete {
                            stop_reason: StopReason::EndTurn,
                            usage: Usage::default(),
                        },
                        None,
                    )),
                }
            },
        )))
    }
}

/// Never emits anything (watchdog fodder).
#[derive(Clone)]
struct Silent;

impl Provider for Silent {
    fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
        Ok(Box::pin(stream::pending()))
    }
}

fn spawner(provider: Arc<dyn Provider>, store: &SessionStore) -> Arc<SubagentSpawner> {
    Arc::new(
        SubagentSpawner::new(
            Arc::new(FixedProviderResolver::new(provider)),
            store.clone(),
            vec![AgentDefinition {
                name: "explore".into(),
                description: "explores".into(),
                model: None,
                prompt: "".into(),
            }],
            std::env::temp_dir(),
            0,
            10,
            3,
        )
        .with_stall_timeout(Duration::from_millis(400)),
    )
}

fn bg_call(id: &str) -> ProviderEvent {
    ProviderEvent::ToolCallCompleted {
        id: id.into(),
        name: "task".into(),
        input: serde_json::json!({
            "description": "bg explore",
            "prompt": "find things",
            "subagent_type": "explore",
            "background": true,
        }),
    }
}

fn background_tool_context(
    session_id: String,
    spawner: Arc<SubagentSpawner>,
    cwd: &std::path::Path,
) -> ToolContext {
    let mut ctx = ToolContext::root(cwd.to_path_buf()).with_subagents(spawner);
    ctx.session_id = session_id;
    ctx
}

#[tokio::test]
async fn background_bash_returns_job_id_and_notifies_once() {
    let (store, session_id) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![])), &store);
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let ctx = background_tool_context(session_id.clone(), spawner, std::env::temp_dir().as_ref());

    let started = std::time::Instant::now();
    let outcomes = ilar::tools::executor::execute_calls(
        vec![ilar::tools::executor::ToolCall {
            id: "background-bash".into(),
            name: "bash".into(),
            input: serde_json::json!({
                "command": "sleep 0.2; printf benchmark-done",
                "run_in_background": true
            }),
        }],
        |name| registry.get(name),
        ctx,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let output = &outcomes[0].output;
    assert!(!output.is_error, "{}", output.content);
    assert!(
        output.content.contains("Background job"),
        "{}",
        output.content
    );
    let job_id = output
        .content
        .split_whitespace()
        .nth(2)
        .expect("job ID in launch output");
    assert!(started.elapsed() < Duration::from_millis(150));

    let notification = tokio::time::timeout(Duration::from_secs(2), notifications.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(notification.parent_session_id, session_id);
    assert!(
        notification.text.contains("benchmark-done"),
        "{}",
        notification.text
    );
    assert!(notification.text.contains(job_id), "{}", notification.text);
    assert!(!notification.is_error);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), notifications.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn contended_background_bash_still_returns_immediately() {
    let (store, session_id) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![])), &store);
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let ctx = background_tool_context(session_id, spawner, std::env::temp_dir().as_ref());
    let permit = ctx
        .workspace
        .acquire(ilar::tools::WorkspaceAccess::Mutating)
        .await;
    let started = std::time::Instant::now();
    let output = registry
        .get("bash")
        .unwrap()
        .run(
            serde_json::json!({"command": "printf queued", "run_in_background": true}),
            ctx,
        )
        .await;
    assert!(!output.is_error);
    assert!(started.elapsed() < Duration::from_millis(100));
    drop(permit);
    let notification = notifications.recv().await.unwrap();
    assert!(notification.text.contains("queued"));
}

#[tokio::test]
async fn background_tool_must_be_the_only_call_in_a_step() {
    let (store, session_id) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![])), &store);
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let ctx = background_tool_context(session_id, spawner, std::env::temp_dir().as_ref());
    let outcomes = ilar::tools::executor::execute_calls(
        vec![
            ilar::tools::executor::ToolCall {
                id: "background".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "printf nope", "run_in_background": true}),
            },
            ilar::tools::executor::ToolCall {
                id: "read".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "missing"}),
            },
        ],
        |name| registry.get(name),
        ctx,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(outcomes[0].output.is_error);
    assert!(outcomes[0].output.content.contains("only tool call"));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), notifications.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn root_cancellation_stops_background_bash() {
    let (store, session_id) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![])), &store);
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("root-cancelled");
    let root_cancel = tokio_util::sync::CancellationToken::new();
    let mut ctx = background_tool_context(session_id, spawner, dir.path());
    ctx.cancel = root_cancel.clone();
    registry
        .get("bash")
        .unwrap()
        .run(
            serde_json::json!({
                "command": format!("sleep 1; touch {}", marker.display()),
                "run_in_background": true
            }),
            ctx,
        )
        .await;
    root_cancel.cancel();
    let notification = notifications.recv().await.unwrap();
    assert!(notification.text.contains("cancelled"));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!marker.exists());
}

#[tokio::test]
async fn root_cancellation_reaches_background_bash_from_foreground_child() {
    let (store, session_id) = temp_store();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("nested-root-cancelled");
    let child = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallCompleted {
                id: "nested-bash".into(),
                name: "bash".into(),
                input: serde_json::json!({
                    "command": format!("sleep 1; touch {}", marker.display()),
                    "run_in_background": true
                }),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("child continues".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
        ],
    ]);
    let spawner = spawner(Arc::new(child), &store);
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let root_cancel = tokio_util::sync::CancellationToken::new();
    let mut ctx = background_tool_context(session_id, spawner, dir.path());
    ctx.cancel = root_cancel.clone();
    let output = registry
        .get("task")
        .unwrap()
        .run(
            serde_json::json!({
                "description": "nested background Bash",
                "prompt": "launch benchmark",
                "subagent_type": "explore"
            }),
            ctx,
        )
        .await;
    assert!(!output.is_error, "{}", output.content);
    root_cancel.cancel();
    let notification = tokio::time::timeout(Duration::from_secs(2), notifications.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        notification.text.contains("cancelled"),
        "{}",
        notification.text
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!marker.exists());
}

#[tokio::test]
async fn shutdown_seals_background_registry() {
    let (store, session_id) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![])), &store);
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    spawner.shutdown().await;
    let output = registry
        .get("bash")
        .unwrap()
        .run(
            serde_json::json!({"command": "printf nope", "run_in_background": true}),
            background_tool_context(session_id, spawner.clone(), std::env::temp_dir().as_ref()),
        )
        .await;
    assert!(output.is_error);
    assert!(
        output.content.contains("shutting down"),
        "{}",
        output.content
    );
    assert_eq!(spawner.running_background(), 0);
}

#[tokio::test]
async fn background_bash_timeout_is_overridable() {
    let (store, session_id) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![])), &store);
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let ctx = background_tool_context(session_id, spawner, std::env::temp_dir().as_ref());
    let output = registry
        .get("bash")
        .unwrap()
        .run(
            serde_json::json!({
                "command": "sleep 10",
                "run_in_background": true,
                "timeout_ms": 100
            }),
            ctx,
        )
        .await;
    assert!(!output.is_error);

    let notification = tokio::time::timeout(Duration::from_secs(2), notifications.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(notification.is_error);
    assert!(
        notification.text.contains("timed out after 100ms"),
        "{}",
        notification.text
    );
}

#[tokio::test]
async fn cancelling_background_bash_kills_work_and_notifies() {
    let (store, session_id) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![])), &store);
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("should-not-exist");
    let command = format!("sleep 1; touch {}", marker.display());
    let ctx = background_tool_context(session_id, spawner.clone(), dir.path());
    registry
        .get("bash")
        .unwrap()
        .run(
            serde_json::json!({"command": command, "run_in_background": true}),
            ctx,
        )
        .await;
    spawner.abort_all();

    let notification = tokio::time::timeout(Duration::from_secs(2), notifications.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(notification.is_error);
    assert!(
        notification.text.contains("cancelled"),
        "{}",
        notification.text
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while spawner.running_background() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled job handle was not cleaned up");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!marker.exists(), "cancelled process continued running");
}

#[tokio::test]
async fn background_bash_holds_workspace_until_completion() {
    let (store, session_id) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![])), &store);
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let background_started = dir.path().join("background-started");
    let ctx = background_tool_context(session_id, spawner, dir.path());
    registry
        .get("bash")
        .unwrap()
        .run(
            serde_json::json!({
                "command": format!("touch {}; sleep 0.2; printf background-finished", background_started.display()),
                "run_in_background": true
            }),
            ctx.clone(),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !background_started.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("background Bash did not acquire the workspace");

    let started = std::time::Instant::now();
    let outcomes = ilar::tools::executor::execute_calls(
        vec![ilar::tools::executor::ToolCall {
            id: "write-after-background".into(),
            name: "write".into(),
            input: serde_json::json!({"path": "after.txt", "content": "foreground"}),
        }],
        |name| registry.get(name),
        ctx,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(
        !outcomes[0].output.is_error,
        "{}",
        outcomes[0].output.content
    );
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "foreground mutation overlapped background Bash: {:?}",
        started.elapsed()
    );
    let notification = notifications.recv().await.unwrap();
    assert!(notification.text.contains("background-finished"));
}

#[tokio::test]
async fn background_task_returns_immediately_and_notifies_once() {
    let (store, session_id) = temp_store();
    // Child takes 300ms — the tool call must return well before that.
    let spawner = spawner(
        Arc::new(DelayedText {
            text: "bg found it",
            delay_ms: 300,
        }),
        &store,
    );
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();

    let parent = MockProvider::new(vec![
        vec![
            bg_call("t1"),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("meanwhile".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
        ],
    ]);

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let start = std::time::Instant::now();
    let outcome = run_turn(
        &parent,
        &registry,
        &store,
        &session_id,
        "go",
        None,
        LoopConfig::default(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();
    assert_eq!(outcome, TurnOutcome::Completed);
    // The tool result came back fast (child still running).
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "background task blocked the turn: {:?}",
        start.elapsed()
    );
    let session = store.load(&session_id).unwrap();
    let results = &session.transcript()[2].content;
    assert!(matches!(
        &results[0],
        ContentBlock::ToolResult { content, is_error: false, .. }
            if content.to_lowercase().contains("background")
    ));

    // Exactly one notification arrives with the child's answer.
    let notification = tokio::time::timeout(Duration::from_secs(5), notifications.recv())
        .await
        .expect("notification within timeout")
        .expect("notification present");
    assert!(
        notification.text.contains("bg found it"),
        "got: {}",
        notification.text
    );
    assert_eq!(notification.parent_session_id, session_id);
    // No second notification.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), notifications.recv())
            .await
            .is_err(),
        "duplicate notification"
    );
}

#[tokio::test]
async fn stall_watchdog_fires_on_silent_child() {
    let (store, session_id) = temp_store();
    let spawner = spawner(Arc::new(Silent), &store);
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let task = registry.get("task").unwrap();

    let start = std::time::Instant::now();
    let out = task
        .run(
            serde_json::json!({
                "description": "silent bg",
                "prompt": "say nothing",
                "subagent_type": "explore",
                "background": true,
            }),
            ToolContext {
                cwd: std::env::temp_dir(),
                session_id,
                depth: 0,
                subagent: Some(spawner),
                workspace: ilar::tools::WorkspaceScheduler::new(),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await;
    assert!(!out.is_error);

    let notification = tokio::time::timeout(Duration::from_secs(5), notifications.recv())
        .await
        .expect("watchdog notification")
        .expect("present");
    assert!(
        notification.is_error,
        "watchdog notification should be error: {:?}",
        notification.text
    );
    assert!(
        notification.text.to_lowercase().contains("stall"),
        "should mention stall: {}",
        notification.text
    );
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "watchdog took too long"
    );
}

#[tokio::test]
async fn nested_notification_runs_declared_parent_and_propagates_once() {
    let (store, root_id) = temp_store();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(root_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
            })
            .unwrap(),
    );
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("child parent acknowledged".into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]]);
    let root = spawner(Arc::new(provider.clone()), &store);

    let outcome = root
        .route_notification(
            ilar::subagent::Notification {
                parent_session_id: child_id.clone(),
                description: "nested".into(),
                text: "nested result".into(),
                is_error: false,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let ilar::subagent::RouteOutcome::Propagate(propagated) = outcome else {
        panic!("expected propagated notification");
    };

    assert_eq!(propagated.parent_session_id, root_id);
    assert!(propagated.text.contains("child parent acknowledged"));
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(provider.requests()[0].model, "zai/glm-4.7");
    let child = store.load(&child_id).unwrap();
    assert!(format!("{:?}", child.transcript()).contains("nested result"));
}

#[tokio::test]
async fn notification_waits_for_busy_parent_without_being_lost() {
    let (store, root_id) = temp_store();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(root_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
            })
            .unwrap(),
    );
    let provider = MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }]]);
    let router = spawner(Arc::new(provider.clone()), &store);
    let mut active_parent = store.acquire_writer(&child_id).unwrap().load().unwrap();
    let handle = {
        let router = router.clone();
        let child_id = child_id.clone();
        tokio::spawn(async move {
            router
                .route_notification(
                    ilar::subagent::Notification {
                        parent_session_id: child_id,
                        description: "queued".into(),
                        text: "wait for parent".into(),
                        is_error: false,
                    },
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!handle.is_finished());
    active_parent
        .append(ilar::session::SessionEvent::UserMessage {
            id: new_id(),
            text: "active parent work".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    active_parent
        .append(ilar::session::SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content: vec![ContentBlock::Text {
                text: "previous active answer".into(),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    drop(active_parent);

    let outcome = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("router resumed")
        .unwrap()
        .unwrap();
    let ilar::subagent::RouteOutcome::Propagate(notification) = outcome else {
        panic!("expected propagation");
    };
    assert_eq!(notification.parent_session_id, root_id);
    assert!(notification.text.contains("finished with no text"));
    assert!(!notification.text.contains("previous active answer"));
    assert_eq!(provider.requests().len(), 1);
    assert!(
        format!("{:?}", store.load(&child_id).unwrap().transcript()).contains("wait for parent")
    );
}

#[tokio::test]
async fn cancelled_undelivered_notification_is_returned_for_requeue() {
    let (store, root_id) = temp_store();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(root_id),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
            })
            .unwrap(),
    );
    let router = spawner(Arc::new(Silent), &store);
    let _writer = store.acquire_writer(&child_id).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let task = {
        let router = router.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            router
                .route_notification(
                    ilar::subagent::Notification {
                        parent_session_id: child_id,
                        description: "keep me".into(),
                        text: "queued".into(),
                        is_error: false,
                    },
                    cancel,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let outcome = task.await.unwrap().unwrap();
    assert!(matches!(
        outcome,
        ilar::subagent::RouteOutcome::Requeue(notification)
            if notification.description == "keep me"
    ));
}

#[tokio::test]
async fn nested_parent_failure_propagates_one_error() {
    let (store, root_id) = temp_store();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(root_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
            })
            .unwrap(),
    );
    let router = spawner(Arc::new(MockProvider::error("provider failed")), &store);

    let outcome = router
        .route_notification(
            ilar::subagent::Notification {
                parent_session_id: child_id,
                description: "nested failure".into(),
                text: "deliver me".into(),
                is_error: false,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let ilar::subagent::RouteOutcome::Propagate(notification) = outcome else {
        panic!("expected propagated error");
    };
    assert_eq!(notification.parent_session_id, root_id);
    assert!(notification.is_error);
    assert!(notification.text.contains("provider failed"));
}

#[tokio::test]
async fn nested_no_text_completion_does_not_reuse_stale_answer() {
    let (store, root_id) = temp_store();
    let child_id = new_id();
    let mut child = store
        .create(SessionMeta {
            session_id: child_id.clone(),
            parent_id: Some(root_id),
            agent: "explore".into(),
            model: "zai/glm-4.7".into(),
        })
        .unwrap();
    child
        .append(ilar::session::SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content: vec![ContentBlock::Text {
                text: "stale answer".into(),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    drop(child);
    let router = spawner(
        Arc::new(MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }]])),
        &store,
    );

    let outcome = router
        .route_notification(
            ilar::subagent::Notification {
                parent_session_id: child_id,
                description: "no text".into(),
                text: "new notification".into(),
                is_error: false,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let ilar::subagent::RouteOutcome::Propagate(notification) = outcome else {
        panic!("expected propagation");
    };
    assert!(notification.text.contains("finished with no text"));
    assert!(!notification.text.contains("stale answer"));
}

#[tokio::test]
async fn notification_reinvokes_parent_loop_as_synthetic_user_turn() {
    let (store, session_id) = temp_store();
    let spawner = spawner(
        Arc::new(DelayedText {
            text: "answer from bg",
            delay_ms: 50,
        }),
        &store,
    );
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();

    // Kick off the background task directly (unit-level).
    let task = registry.get("task").unwrap();
    task.run(
        serde_json::json!({
            "description": "bg",
            "prompt": "work",
            "subagent_type": "explore",
            "background": true,
        }),
        ToolContext {
            session_id: session_id.clone(),
            ..ToolContext::root(std::env::temp_dir())
        }
        .with_subagents(spawner),
    )
    .await;

    // The consumer (TUI) side: notification becomes the next user turn.
    let notification = notifications.recv().await.expect("notification");
    let parent = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("acknowledged".into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        },
    ]]);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    run_turn(
        &parent,
        &registry,
        &store,
        &session_id,
        &notification.text,
        None,
        LoopConfig::default(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();

    let session = store.load(&session_id).unwrap();
    let transcript = session.transcript();
    let last_user = transcript
        .iter()
        .rev()
        .find(|m| {
            m.role == ilar::session::Role::User
                && matches!(m.content.first(), Some(ContentBlock::Text { .. }))
        })
        .unwrap();
    assert!(format!("{:?}", last_user.content).contains("answer from bg"));
}

#[tokio::test]
async fn abort_all_kills_running_children() {
    let (store, _session_id) = temp_store();
    let spawner = spawner(
        Arc::new(DelayedText {
            text: "never",
            delay_ms: 10_000,
        }),
        &store,
    );
    let _notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let task = registry.get("task").unwrap();

    task.run(
        serde_json::json!({
            "description": "long bg",
            "prompt": "work",
            "subagent_type": "explore",
            "background": true,
        }),
        ToolContext::root(std::env::temp_dir()).with_subagents(spawner.clone()),
    )
    .await;

    let start = std::time::Instant::now();
    spawner.abort_all();
    // The detached task must terminate quickly (no 10s child turn).
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if spawner.running_background() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("children terminated");
    assert!(start.elapsed() < Duration::from_secs(2));
}
