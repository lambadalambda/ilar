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
async fn nested_background_tasks_are_rejected() {
    let (store, _session_id) = temp_store();
    let root = spawner(Arc::new(Silent), &store);
    let nested = Arc::new(SubagentSpawner::new(
        root.resolver(),
        store,
        root.agents().to_vec(),
        std::env::temp_dir(),
        1,
        10,
        3,
    ));
    let task = ToolRegistry::builtin()
        .with_subagents(nested.clone())
        .unwrap()
        .get("task")
        .unwrap();

    let output = task
        .run(
            serde_json::json!({
                "description": "nested background",
                "prompt": "work",
                "subagent_type": "explore",
                "background": true,
            }),
            ToolContext::root(std::env::temp_dir()).with_subagents(nested),
        )
        .await;

    assert!(output.is_error);
    assert!(output.content.contains("Nested background"));
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
