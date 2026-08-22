use std::sync::Arc;
use std::time::Duration;

use futures::{StreamExt, stream};
use ilar::agent::{LOOP_EVENT_CAPACITY, LoopConfig, TurnOutcome, loop_event_channel, run_turn};
use ilar::config::{AgentDefinition, AgentWorkspaceMode};
use ilar::provider::{
    EventStream, FixedProviderResolver, MockProvider, Provider, ProviderEvent, Request, StopReason,
};
use ilar::session::{ContentBlock, SessionEvent, SessionMeta, SessionStore, Usage, new_id};
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
            workspace: None,
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

#[derive(Clone)]
struct NotifyingPending {
    started: Arc<tokio::sync::Notify>,
}

impl Provider for NotifyingPending {
    fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
        self.started.notify_one();
        Ok(Box::pin(stream::pending()))
    }
}

#[derive(Clone)]
struct NotifyingDelayedText {
    started: Arc<tokio::sync::Notify>,
}

impl Provider for NotifyingDelayedText {
    fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
        self.started.notify_one();
        Ok(Box::pin(
            stream::once(async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                ProviderEvent::TextDelta("finished".into())
            })
            .chain(stream::once(async {
                ProviderEvent::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                }
            })),
        ))
    }
}

#[derive(Clone)]
struct NestedBackgroundProvider {
    worktree: std::path::PathBuf,
}

impl Provider for NestedBackgroundProvider {
    fn stream(&self, request: Request) -> anyhow::Result<EventStream> {
        let initial_prompt = request
            .messages
            .first()
            .and_then(|message| message.content.first())
            .and_then(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            });
        if initial_prompt == Some("remain pending") {
            return Ok(Box::pin(stream::pending()));
        }
        let has_tool_result = request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        if has_tool_result {
            return Ok(Box::pin(stream::iter(vec![
                ProviderEvent::TextDelta("intermediate finished".into()),
                ProviderEvent::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                },
            ])));
        }
        Ok(Box::pin(stream::iter(vec![
            ProviderEvent::ToolCallStarted {
                id: "nested-task".into(),
                name: "task".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "nested-task".into(),
                name: "task".into(),
                input: serde_json::json!({
                    "description": "nested pending",
                    "prompt": "remain pending",
                    "subagent_type": "explore",
                    "background": true,
                    "workspace": {
                        "cwd": self.worktree.clone(),
                        "isolation": "git_worktree"
                    }
                }),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ])))
    }
}

fn spawner(provider: Arc<dyn Provider>, store: &SessionStore) -> Arc<SubagentSpawner> {
    spawner_for_workspace(
        provider,
        store,
        AgentWorkspaceMode::Mutable,
        std::env::temp_dir(),
    )
}

fn spawner_for_workspace(
    provider: Arc<dyn Provider>,
    store: &SessionStore,
    workspace_mode: AgentWorkspaceMode,
    cwd: std::path::PathBuf,
) -> Arc<SubagentSpawner> {
    Arc::new(
        SubagentSpawner::new(
            Arc::new(FixedProviderResolver::new(provider)),
            store.clone(),
            vec![AgentDefinition {
                name: "explore".into(),
                description: "explores".into(),
                model: None,
                prompt: "".into(),
                workspace_mode,
                tools: None,
            }],
            cwd,
            0,
            10,
            3,
        )
        .with_stall_timeout(Duration::from_millis(400)),
    )
}

fn repository_with_worktree() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    fn git(cwd: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.name", "ilar tests"]);
    git(&root, &["config", "user.email", "ilar@example.invalid"]);
    std::fs::write(root.join("README.md"), "test\n").unwrap();
    git(&root, &["add", "README.md"]);
    git(&root, &["commit", "-qm", "initial"]);
    let worktree = temp.path().join("isolated-worktree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-qb",
            "notification-test",
            worktree.to_str().unwrap(),
        ],
    );
    (temp, root, worktree)
}

fn remove_worktree(root: &std::path::Path, worktree: &std::path::Path) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git worktree remove: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn leased_child_rejects_background_task() {
    let (store, session_id) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![])), &store);
    let task = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap()
        .get("task")
        .unwrap();
    let mut ctx = background_tool_context(session_id, spawner.clone(), &std::env::temp_dir());
    ctx.workspace_lease = Some(
        spawner
            .workspace()
            .acquire_lease(ilar::tools::WorkspaceAccess::Mutating)
            .await,
    );

    let output = task
        .run(
            serde_json::json!({
                "description": "nested detached",
                "prompt": "work",
                "subagent_type": "explore",
                "background": true,
            }),
            ctx,
        )
        .await;

    assert!(output.is_error);
    assert!(
        output.content.contains("cannot outlive"),
        "{}",
        output.content
    );
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
async fn root_cancellation_reaches_nested_background_task_after_parent_finishes() {
    let (_repo, root, worktree) = repository_with_worktree();
    let (store, session_id) = temp_store();
    let provider = NestedBackgroundProvider {
        worktree: worktree.clone(),
    };
    let spawner = spawner_for_workspace(
        Arc::new(provider),
        &store,
        AgentWorkspaceMode::Mutable,
        root.clone(),
    );
    let mut notifications = spawner.subscribe();
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap();
    let root_cancel = tokio_util::sync::CancellationToken::new();
    let mut context = background_tool_context(session_id, spawner.clone(), &root);
    context.cancel = root_cancel.clone();

    let started = registry
        .get("task")
        .unwrap()
        .run(
            serde_json::json!({
                "description": "intermediate parent",
                "prompt": "launch nested",
                "subagent_type": "explore",
                "background": true,
            }),
            context,
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let parent_finished = tokio::time::timeout(Duration::from_secs(2), notifications.recv())
        .await
        .expect("intermediate task should finish")
        .expect("intermediate notification should be present");
    assert!(!parent_finished.is_error, "{}", parent_finished.text);

    root_cancel.cancel();
    let nested_cancelled = tokio::time::timeout(Duration::from_millis(200), notifications.recv())
        .await
        .expect("nested task should inherit root cancellation")
        .expect("nested cancellation notification should be present");
    assert!(nested_cancelled.is_error, "{}", nested_cancelled.text);
    assert_eq!(nested_cancelled.description, "nested pending");
    assert!(
        nested_cancelled.text.contains("cancelled") || nested_cancelled.text.contains("aborted"),
        "{}",
        nested_cancelled.text
    );

    spawner.shutdown().await;
}

#[tokio::test]
async fn background_subagent_max_iterations_is_error() {
    let (store, session_id) = temp_store();
    let spawner = Arc::new(
        SubagentSpawner::new(
            Arc::new(FixedProviderResolver::new(Arc::new(MockProvider::new(
                vec![],
            )))),
            store,
            vec![AgentDefinition {
                name: "explore".into(),
                description: "explores".into(),
                model: None,
                prompt: String::new(),
                workspace_mode: AgentWorkspaceMode::Mutable,
                tools: None,
            }],
            std::env::temp_dir(),
            0,
            10,
            3,
        )
        .with_loop_config(LoopConfig {
            max_iterations: 0,
            ..LoopConfig::default()
        }),
    );
    let mut notifications = spawner.subscribe();
    let task = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap()
        .get("task")
        .unwrap();

    let started = task
        .run(
            serde_json::json!({
                "description": "bounded background",
                "prompt": "work",
                "subagent_type": "explore",
                "background": true,
            }),
            background_tool_context(session_id, spawner.clone(), std::env::temp_dir().as_ref()),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    let notification = tokio::time::timeout(Duration::from_secs(2), notifications.recv())
        .await
        .expect("bounded task should notify")
        .expect("bounded task notification should be present");
    assert!(notification.is_error, "{}", notification.text);
    assert!(
        notification.text.contains("iteration"),
        "{}",
        notification.text
    );

    spawner.shutdown().await;
}

#[tokio::test]
async fn foreground_child_rejects_detached_workspace_mutation() {
    let (store, session_id) = temp_store();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("nested-root-cancelled");
    let child = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "nested-bash".into(),
                name: "bash".into(),
                item_id: None,
            },
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
    let mut ctx = background_tool_context(session_id, spawner, dir.path());
    ctx.cancel = tokio_util::sync::CancellationToken::new();
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
    assert!(
        tokio::time::timeout(Duration::from_millis(100), notifications.recv())
            .await
            .is_err(),
        "detached Bash unexpectedly started"
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
            ProviderEvent::ToolCallStarted {
                id: "t1".into(),
                name: "task".into(),
                item_id: None,
            },
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

    let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);
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
        None,
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
    let launch = match &results[0] {
        ContentBlock::ToolResult {
            content,
            is_error: false,
            ..
        } => content,
        result => panic!("expected successful task result, got {result:?}"),
    };
    assert!(launch.to_lowercase().contains("background"), "{launch}");
    assert!(launch.contains("separate follow-up turn"), "{launch}");
    assert!(launch.contains("disjoint"), "{launch}");
    assert!(
        launch.contains("Do not perform this task's scope"),
        "{launch}"
    );
    assert!(launch.contains("Do not sleep, poll, or check"), "{launch}");
    assert!(!launch.contains("end your response"), "{launch}");

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
                location: ilar::tools::WorkspaceLocation::shared(std::env::temp_dir()),
                session_id,
                call_id: None,
                depth: 0,
                subagent: Some(spawner),
                workspace: ilar::tools::WorkspaceScheduler::new(),
                workspace_lease: None,
                workspace_ancestry: Vec::new(),
                cancel: tokio_util::sync::CancellationToken::new(),
                output_tail: None,
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
                workspace: None,
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
async fn routed_notification_waits_for_an_active_parent_without_requeueing() {
    let (store, root_id) = temp_store();
    let dir = tempfile::tempdir().unwrap();
    let location = ilar::tools::WorkspaceLocation::shared(dir.path().to_path_buf());
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(root_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: Some(location),
            })
            .unwrap(),
    );
    let started = Arc::new(tokio::sync::Notify::new());
    let spawner = spawner_for_workspace(
        Arc::new(NotifyingDelayedText {
            started: started.clone(),
        }),
        &store,
        AgentWorkspaceMode::Mutable,
        dir.path().to_path_buf(),
    );
    let task = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap()
        .get("task")
        .unwrap();
    let context = background_tool_context(root_id.clone(), spawner.clone(), dir.path());
    let resumed_id = child_id.clone();
    let active = tokio::spawn(async move {
        task.run(
            serde_json::json!({
                "description": "active parent",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": resumed_id,
            }),
            context,
        )
        .await
    });
    started.notified().await;

    let outcome = spawner
        .route_notification(
            ilar::subagent::Notification {
                parent_session_id: child_id,
                description: "nested result".into(),
                text: "nested finished".into(),
                is_error: false,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, ilar::subagent::RouteOutcome::Propagate(_)),
        "active parent notification was requeued"
    );
    assert!(!active.await.unwrap().is_error);
}

#[tokio::test]
async fn routed_abort_after_append_is_terminal_instead_of_requeued() {
    let (store, session_id) = temp_store();
    let started = Arc::new(tokio::sync::Notify::new());
    let spawner = Arc::new(SubagentSpawner::new(
        Arc::new(FixedProviderResolver::new(Arc::new(NotifyingPending {
            started: started.clone(),
        }))),
        store.clone(),
        vec![AgentDefinition {
            name: "build".into(),
            description: "builds".into(),
            model: None,
            prompt: String::new(),
            workspace_mode: AgentWorkspaceMode::Mutable,
            tools: None,
        }],
        std::env::temp_dir(),
        0,
        10,
        3,
    ));
    let cancel = tokio_util::sync::CancellationToken::new();
    let routed_spawner = spawner.clone();
    let routed_cancel = cancel.clone();
    let routed_session_id = session_id.clone();
    let routed = tokio::spawn(async move {
        routed_spawner
            .route_notification(
                ilar::subagent::Notification {
                    parent_session_id: routed_session_id,
                    description: "completed child".into(),
                    text: "synthetic notification".into(),
                    is_error: false,
                },
                routed_cancel,
            )
            .await
    });
    started.notified().await;
    cancel.cancel();

    let error = match routed.await.unwrap() {
        Err(error) => error,
        Ok(_) => panic!("an appended aborted route must not be replayed"),
    };
    assert!(error.to_string().contains("aborted"), "{error:#}");
    let notification_messages = store
        .load(&session_id)
        .unwrap()
        .events()
        .iter()
        .filter(|event| {
            matches!(event, SessionEvent::UserMessage { text, .. } if text == "synthetic notification")
        })
        .count();
    assert_eq!(notification_messages, 1);
}

#[tokio::test]
async fn isolated_notification_uses_persisted_cwd_and_independent_lock() {
    let (_repo, root, worktree) = repository_with_worktree();
    let (store, root_id) = temp_store();
    let location = ilar::tools::WorkspaceLocation::validated_git_worktree(
        &ilar::tools::WorkspaceLocation::shared(root.clone()),
        worktree.clone(),
    )
    .await
    .unwrap();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(root_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: Some(location),
            })
            .unwrap(),
    );
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "write-cwd".into(),
                name: "bash".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "write-cwd".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "pwd > routed-cwd.txt"}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ],
        vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }],
    ]);
    let router = spawner_for_workspace(
        Arc::new(provider),
        &store,
        AgentWorkspaceMode::Mutable,
        root.clone(),
    );
    let _root_busy = router
        .workspace()
        .acquire(ilar::tools::WorkspaceAccess::Mutating)
        .await;

    tokio::time::timeout(
        Duration::from_secs(2),
        router.route_notification(
            ilar::subagent::Notification {
                parent_session_id: child_id,
                description: "isolated route".into(),
                text: "deliver".into(),
                is_error: false,
            },
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await
    .expect("isolated route should not wait for the root checkout")
    .unwrap();

    assert!(worktree.join("routed-cwd.txt").exists());
    assert!(!root.join("routed-cwd.txt").exists());
}

#[tokio::test]
async fn nested_notification_resolves_each_workspace_against_its_parent() {
    let (_repo, root, worktree) = repository_with_worktree();
    let (store, root_id) = temp_store();
    let root_location = ilar::tools::WorkspaceLocation::shared(root.clone());
    let isolated = ilar::tools::WorkspaceLocation::validated_git_worktree(&root_location, worktree)
        .await
        .unwrap();
    let back_to_root =
        ilar::tools::WorkspaceLocation::validated_git_worktree(&isolated, root.clone())
            .await
            .unwrap();
    let isolated_parent_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: isolated_parent_id.clone(),
                parent_id: Some(root_id),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: Some(isolated),
            })
            .unwrap(),
    );
    let nested_root_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: nested_root_id.clone(),
                parent_id: Some(isolated_parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: Some(back_to_root),
            })
            .unwrap(),
    );
    let provider = MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }]]);
    let router = spawner_for_workspace(
        Arc::new(provider.clone()),
        &store,
        AgentWorkspaceMode::Mutable,
        root,
    );

    let outcome = router
        .route_notification(
            ilar::subagent::Notification {
                parent_session_id: nested_root_id,
                description: "nested root route".into(),
                text: "deliver".into(),
                is_error: false,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        ilar::subagent::RouteOutcome::Propagate(notification)
            if notification.parent_session_id == isolated_parent_id
    ));
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn notification_with_cyclic_ancestry_is_preserved_without_propagating() {
    let (store, _root_id) = temp_store();
    let first_id = new_id();
    let second_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: first_id.clone(),
                parent_id: Some(second_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap(),
    );
    drop(
        store
            .create(SessionMeta {
                session_id: second_id,
                parent_id: Some(first_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap(),
    );
    let provider = MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }]]);
    let router = spawner(Arc::new(provider.clone()), &store);

    let outcome = router
        .route_notification(
            ilar::subagent::Notification {
                parent_session_id: first_id,
                description: "cyclic route".into(),
                text: "deliver".into(),
                is_error: false,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        ilar::subagent::RouteOutcome::Requeue(notification)
            if notification.description == "cyclic route"
    ));
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn notification_with_missing_parent_session_is_preserved() {
    let (store, _root_id) = temp_store();
    let provider = MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }]]);
    let router = spawner(Arc::new(provider.clone()), &store);
    let missing_id = new_id();

    let outcome = router
        .route_notification(
            ilar::subagent::Notification {
                parent_session_id: missing_id.clone(),
                description: "missing route".into(),
                text: "deliver".into(),
                is_error: false,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        ilar::subagent::RouteOutcome::Requeue(notification)
            if notification.parent_session_id == missing_id
    ));
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn isolated_notification_revalidates_after_waiting_for_lease() {
    let (_repo, root, worktree) = repository_with_worktree();
    let (store, root_id) = temp_store();
    let location = ilar::tools::WorkspaceLocation::validated_git_worktree(
        &ilar::tools::WorkspaceLocation::shared(root.clone()),
        worktree.clone(),
    )
    .await
    .unwrap();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(root_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: Some(location.clone()),
            })
            .unwrap(),
    );
    let provider = MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }]]);
    let router = spawner_for_workspace(
        Arc::new(provider.clone()),
        &store,
        AgentWorkspaceMode::Mutable,
        root.clone(),
    );
    let isolated = router.workspace().scoped(&location);
    let busy = isolated
        .acquire(ilar::tools::WorkspaceAccess::Mutating)
        .await;
    let route = {
        let router = router.clone();
        tokio::spawn(async move {
            router
                .route_notification(
                    ilar::subagent::Notification {
                        parent_session_id: child_id,
                        description: "stale isolated route".into(),
                        text: "deliver".into(),
                        is_error: false,
                    },
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!route.is_finished());

    remove_worktree(&root, &worktree);
    drop(busy);
    let outcome = route.await.unwrap().unwrap();
    let ilar::subagent::RouteOutcome::Propagate(failure) = outcome else {
        panic!("stale worktree route did not propagate its failure");
    };

    assert!(failure.is_error);
    assert_eq!(failure.parent_session_id, root_id);
    assert!(
        failure.text.contains("workspace could not be restored"),
        "{}",
        failure.text
    );
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn routed_read_only_agent_keeps_mutating_tools_unavailable() {
    let (store, root_id) = temp_store();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(root_id),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap(),
    );
    let workspace = tempfile::tempdir().unwrap();
    let marker = workspace.path().join("must-not-exist");
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "write-1".into(),
                name: "write".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "write-1".into(),
                name: "write".into(),
                input: serde_json::json!({"path": marker, "content": "unsafe"}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("handled".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ],
    ]);
    let router = spawner_for_workspace(
        Arc::new(provider),
        &store,
        AgentWorkspaceMode::ReadOnly,
        workspace.path().to_path_buf(),
    );

    router
        .route_notification(
            ilar::subagent::Notification {
                parent_session_id: child_id,
                description: "read-only route".into(),
                text: "deliver".into(),
                is_error: false,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(!marker.exists());
}

#[tokio::test]
async fn routed_mutable_agent_waits_for_workspace_and_requeues_if_cancelled() {
    let (store, root_id) = temp_store();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(root_id),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap(),
    );
    let provider = MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }]]);
    let router = spawner(Arc::new(provider.clone()), &store);
    let _busy = router
        .workspace()
        .acquire(ilar::tools::WorkspaceAccess::Mutating)
        .await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let handle = {
        let router = router.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            router
                .route_notification(
                    ilar::subagent::Notification {
                        parent_session_id: child_id,
                        description: "blocked route".into(),
                        text: "deliver".into(),
                        is_error: false,
                    },
                    cancel,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(provider.requests().is_empty());
    cancel.cancel();

    assert!(matches!(
        handle.await.unwrap().unwrap(),
        ilar::subagent::RouteOutcome::Requeue(notification)
            if notification.description == "blocked route"
    ));
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
                workspace: None,
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
                workspace: None,
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
                workspace: None,
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
            workspace: None,
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
    let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);
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
        None,
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
