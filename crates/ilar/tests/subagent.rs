use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use ilar::agent::{LoopConfig, TurnOutcome, run_turn};
use ilar::config::{AgentDefinition, AgentWorkspaceMode};
use ilar::provider::{
    EventStream, FixedProviderResolver, MockProvider, Provider, ProviderEvent, ProviderHandle,
    ProviderResolver, Request, StopReason, resolve_model,
};
use ilar::session::{
    ChatMessage, ContentBlock, SessionEvent, SessionMeta, SessionStore, Usage, new_id,
};
use ilar::subagent::SubagentSpawner;
use ilar::tools::{ToolContext, ToolRegistry};

fn temp_store() -> (SessionStore, String) {
    let dir = std::env::temp_dir().join(format!("ilar-subagent-test-{}", new_id()));
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

/// Child provider that streams text + terminal after a delay.
#[derive(Clone)]
struct ScriptedDelayProvider {
    text: &'static str,
    delay_ms: u64,
}

impl Provider for ScriptedDelayProvider {
    fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
        let text = self.text.to_string();
        let delay = Duration::from_millis(self.delay_ms);
        Ok(Box::pin(futures::stream::unfold(
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

#[derive(Clone)]
struct SynchronizingProvider {
    barrier: Arc<tokio::sync::Barrier>,
}

impl Provider for SynchronizingProvider {
    fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
        let barrier = self.barrier.clone();
        Ok(Box::pin(futures::stream::unfold(0, move |state| {
            let barrier = barrier.clone();
            async move {
                match state {
                    0 => {
                        barrier.wait().await;
                        Some((ProviderEvent::TextDelta("isolated child".into()), 1))
                    }
                    1 => Some((
                        ProviderEvent::TurnComplete {
                            stop_reason: StopReason::EndTurn,
                            usage: Usage::default(),
                        },
                        2,
                    )),
                    _ => None,
                }
            }
        })))
    }
}

fn spawner(
    provider: Arc<dyn Provider>,
    store: &SessionStore,
    max_concurrent: usize,
    max_depth: usize,
) -> Arc<SubagentSpawner> {
    spawner_with_mode(
        provider,
        store,
        max_concurrent,
        max_depth,
        AgentWorkspaceMode::Mutable,
    )
}

fn spawner_with_mode(
    provider: Arc<dyn Provider>,
    store: &SessionStore,
    max_concurrent: usize,
    max_depth: usize,
    workspace_mode: AgentWorkspaceMode,
) -> Arc<SubagentSpawner> {
    Arc::new(SubagentSpawner::new(
        Arc::new(FixedProviderResolver::new(provider)),
        store.clone(),
        vec![AgentDefinition {
            name: "explore".into(),
            description: "explores things".into(),
            model: None,
            prompt: "You are an explorer.".into(),
            workspace_mode,
        }],
        std::env::temp_dir(),
        0,
        max_concurrent,
        max_depth,
    ))
}

fn parent_registry(spawner: Arc<SubagentSpawner>) -> ToolRegistry {
    ToolRegistry::builtin().with_subagents(spawner).unwrap()
}

fn task_call(id: &str, prompt: &str) -> ProviderEvent {
    ProviderEvent::ToolCallCompleted {
        id: id.into(),
        name: "task".into(),
        input: serde_json::json!({
            "description": "explore",
            "prompt": prompt,
            "subagent_type": "explore",
        }),
    }
}

async fn run_two_tasks(workspace_mode: AgentWorkspaceMode) -> (Duration, Vec<ChatMessage>) {
    let (store, session_id) = temp_store();
    // Children answer after 250ms; serial execution would take 500ms.
    let child = Arc::new(ScriptedDelayProvider {
        text: "child says alpha",
        delay_ms: 250,
    });
    let child2 = ScriptedDelayProvider {
        text: "child says beta",
        delay_ms: 250,
    };
    // One shared provider instance answering both tasks:
    let shared: Arc<dyn Provider> = Arc::new(SharedProvider::new(vec![child, Arc::new(child2)]));
    let spawner = spawner_with_mode(shared.clone(), &store, 10, 3, workspace_mode);
    let registry = parent_registry(spawner);

    // Parent: two task calls, then a final text turn.
    let parent = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "t1".into(),
                name: "task".into(),
            },
            task_call("t1", "find alpha"),
            ProviderEvent::ToolCallStarted {
                id: "t2".into(),
                name: "task".into(),
            },
            task_call("t2", "find beta"),
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

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let start = Instant::now();
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
    let elapsed = start.elapsed();

    assert_eq!(outcome, TurnOutcome::Completed);

    let session = store.load(&session_id).unwrap();
    (elapsed, session.transcript())
}

#[tokio::test]
async fn mutable_tasks_sharing_a_checkout_are_serialized_and_merge_in_order() {
    let (elapsed, transcript) = run_two_tasks(AgentWorkspaceMode::Mutable).await;
    assert!(
        elapsed >= Duration::from_millis(450),
        "mutable tasks overlapped: {elapsed:?}"
    );
    // user, assistant(2 tool calls), user(2 tool results), assistant(done)
    assert_eq!(transcript.len(), 4, "{transcript:?}");
    let results = &transcript[2].content;
    assert!(matches!(
        &results[0],
        ContentBlock::ToolResult { tool_use_id, content, is_error: false }
            if tool_use_id == "t1" && content.contains("alpha")
    ));
    assert!(matches!(
        &results[1],
        ContentBlock::ToolResult { tool_use_id, content, is_error: false }
            if tool_use_id == "t2" && content.contains("beta")
    ));
}

#[tokio::test]
async fn enforced_read_only_tasks_may_overlap() {
    let (elapsed, _) = run_two_tasks(AgentWorkspaceMode::ReadOnly).await;
    assert!(
        elapsed < Duration::from_millis(450),
        "read-only tasks looked serial: {elapsed:?}"
    );
}

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

fn repository_with_worktree() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
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
            "resume-test",
            worktree.to_str().unwrap(),
        ],
    );
    (temp, root, worktree)
}

#[tokio::test]
async fn mutable_tasks_in_distinct_validated_worktrees_may_overlap() {
    let repo_temp = tempfile::tempdir().unwrap();
    let root = repo_temp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.name", "ilar tests"]);
    git(&root, &["config", "user.email", "ilar@example.invalid"]);
    std::fs::write(root.join("README.md"), "test\n").unwrap();
    git(&root, &["add", "README.md"]);
    git(&root, &["commit", "-qm", "initial"]);
    let first = repo_temp.path().join("first-worktree");
    let second = repo_temp.path().join("second-worktree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-qb",
            "first-test",
            first.to_str().unwrap(),
        ],
    );
    git(
        &root,
        &[
            "worktree",
            "add",
            "-qb",
            "second-test",
            second.to_str().unwrap(),
        ],
    );

    let (store, session_id) = temp_store();
    let child: Arc<dyn Provider> = Arc::new(SynchronizingProvider {
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
    });
    let spawner = Arc::new(SubagentSpawner::new(
        Arc::new(FixedProviderResolver::new(child)),
        store.clone(),
        vec![AgentDefinition {
            name: "explore".into(),
            description: "explores".into(),
            model: None,
            prompt: String::new(),
            workspace_mode: AgentWorkspaceMode::Mutable,
        }],
        root.clone(),
        0,
        10,
        3,
    ));
    let workspace_call = |id: &str, cwd: &std::path::Path| ProviderEvent::ToolCallCompleted {
        id: id.into(),
        name: "task".into(),
        input: serde_json::json!({
            "description": "isolated task",
            "prompt": "work",
            "subagent_type": "explore",
            "workspace": {"cwd": cwd, "isolation": "git_worktree"}
        }),
    };
    let parent = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "first".into(),
                name: "task".into(),
            },
            workspace_call("first", &first),
            ProviderEvent::ToolCallStarted {
                id: "second".into(),
                name: "task".into(),
            },
            workspace_call("second", &second),
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
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(2),
        run_turn(
            &parent,
            &parent_registry(spawner.clone()),
            &store,
            &session_id,
            "go",
            None,
            LoopConfig::default(),
            tx,
            tokio_util::sync::CancellationToken::new(),
            ToolContext::root(root).with_subagents(spawner),
        ),
    )
    .await
    .expect("distinct worktree tasks did not overlap")
    .unwrap();

    let transcript = store.load(&session_id).unwrap().transcript();
    let results = &transcript[2].content;
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(results.iter().all(|result| matches!(
        result,
        ContentBlock::ToolResult { content, is_error: false, .. }
            if content.contains("isolated child")
    )));
}

#[tokio::test]
async fn nested_task_rejects_an_ancestor_workspace_lock_cycle() {
    let (repo, root, first) = repository_with_worktree();
    let second = repo.path().join("second-worktree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-qb",
            "second-resume-test",
            second.to_str().unwrap(),
        ],
    );
    let root_location = ilar::tools::WorkspaceLocation::shared(root);
    let first_location =
        ilar::tools::WorkspaceLocation::validated_git_worktree(&root_location, first.clone())
            .await
            .unwrap();
    let second_location =
        ilar::tools::WorkspaceLocation::validated_git_worktree(&root_location, second)
            .await
            .unwrap();
    let scheduler = ilar::tools::WorkspaceScheduler::for_location(&root_location);
    let first_scheduler = scheduler.scoped(&first_location);
    let second_scheduler = scheduler.scoped(&second_location);
    let _ancestor = first_scheduler
        .acquire_lease(ilar::tools::WorkspaceAccess::Mutating)
        .await;
    let current = second_scheduler
        .acquire_lease(ilar::tools::WorkspaceAccess::Mutating)
        .await;
    let (store, session_id) = temp_store();
    let task = parent_registry(spawner(
        Arc::new(MockProvider::new(vec![vec![]])),
        &store,
        10,
        3,
    ))
    .get("task")
    .unwrap();
    let mut ctx = ToolContext::root(second_location.cwd().to_path_buf());
    ctx.session_id = session_id;
    ctx.cwd = second_location.cwd().to_path_buf();
    ctx.location = second_location;
    ctx.workspace = second_scheduler;
    ctx.workspace_lease = Some(current);
    ctx.workspace_ancestry = vec![first_location.id().clone(), ctx.location.id().clone()];

    let output = tokio::time::timeout(
        Duration::from_secs(1),
        task.run(
            serde_json::json!({
                "description": "cycle",
                "prompt": "return to first",
                "subagent_type": "explore",
                "workspace": {"cwd": first, "isolation": "git_worktree"},
            }),
            ctx,
        ),
    )
    .await
    .expect("ancestor workspace cycle deadlocked");

    assert!(output.is_error);
    assert!(
        output.content.contains("held by an ancestor"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn nested_tasks_do_not_wait_on_busy_sibling_workspaces() {
    let (repo, root, first) = repository_with_worktree();
    let second = repo.path().join("second-worktree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-qb",
            "second-inversion-test",
            second.to_str().unwrap(),
        ],
    );
    let root_location = ilar::tools::WorkspaceLocation::shared(root);
    let first_location =
        ilar::tools::WorkspaceLocation::validated_git_worktree(&root_location, first.clone())
            .await
            .unwrap();
    let second_location =
        ilar::tools::WorkspaceLocation::validated_git_worktree(&root_location, second.clone())
            .await
            .unwrap();
    let scheduler = ilar::tools::WorkspaceScheduler::for_location(&root_location);
    let first_scheduler = scheduler.scoped(&first_location);
    let second_scheduler = scheduler.scoped(&second_location);
    let first_lease = first_scheduler
        .acquire_lease(ilar::tools::WorkspaceAccess::Mutating)
        .await;
    let second_lease = second_scheduler
        .acquire_lease(ilar::tools::WorkspaceAccess::Mutating)
        .await;
    let _first_hold = first_lease.clone();
    let _second_hold = second_lease.clone();
    let (store, first_session) = temp_store();
    let second_session = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: second_session.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap(),
    );
    let task = parent_registry(spawner(
        Arc::new(MockProvider::new(vec![vec![]])),
        &store,
        10,
        3,
    ))
    .get("task")
    .unwrap();
    let mut first_ctx = ToolContext::root(first_location.cwd().to_path_buf());
    first_ctx.session_id = first_session;
    first_ctx.cwd = first_location.cwd().to_path_buf();
    first_ctx.location = first_location.clone();
    first_ctx.workspace = first_scheduler;
    first_ctx.workspace_lease = Some(first_lease);
    first_ctx.workspace_ancestry = vec![first_location.id().clone()];
    let mut second_ctx = ToolContext::root(second_location.cwd().to_path_buf());
    second_ctx.session_id = second_session;
    second_ctx.cwd = second_location.cwd().to_path_buf();
    second_ctx.location = second_location.clone();
    second_ctx.workspace = second_scheduler;
    second_ctx.workspace_lease = Some(second_lease);
    second_ctx.workspace_ancestry = vec![second_location.id().clone()];
    let reverse_task = task.clone();

    let (first_output, second_output) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(
            task.run(
                serde_json::json!({
                    "description": "first to second",
                    "prompt": "cross over",
                    "subagent_type": "explore",
                    "workspace": {"cwd": second, "isolation": "git_worktree"},
                }),
                first_ctx,
            ),
            reverse_task.run(
                serde_json::json!({
                    "description": "second to first",
                    "prompt": "cross over",
                    "subagent_type": "explore",
                    "workspace": {"cwd": first, "isolation": "git_worktree"},
                }),
                second_ctx,
            )
        )
    })
    .await
    .expect("sibling workspace inversion deadlocked");

    for output in [first_output, second_output] {
        assert!(output.is_error);
        assert!(
            output.content.contains("workspace is busy"),
            "{}",
            output.content
        );
    }
}

/// Serves each stream() call from the next inner provider once, then
/// repeats the last. Instance-scoped counter (no cross-test statics).
#[derive(Clone)]
struct SharedProvider {
    inner: Vec<Arc<dyn Provider>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl SharedProvider {
    fn new(inner: Vec<Arc<dyn Provider>>) -> Self {
        Self {
            inner,
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl Provider for SharedProvider {
    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let idx = if self.inner.is_empty() {
            0
        } else {
            n.min(self.inner.len() - 1)
        };
        self.inner[idx].stream(req)
    }
}

struct ModelResolver {
    zai: MockProvider,
    openai: MockProvider,
}

impl ProviderResolver for ModelResolver {
    fn resolve_provider(&self, model: &str) -> anyhow::Result<ProviderHandle<'_>> {
        match resolve_model(model)?.0 {
            "zai" => Ok(ProviderHandle::Borrowed(&self.zai)),
            "openai" => Ok(ProviderHandle::Borrowed(&self.openai)),
            provider => anyhow::bail!("unknown provider {provider}"),
        }
    }
}

#[tokio::test]
async fn concurrency_cap_errors_with_guidance() {
    let (store, session_id) = temp_store();
    let child: Arc<dyn Provider> = Arc::new(ScriptedDelayProvider {
        text: "slow child",
        delay_ms: 400,
    });
    // Reuse the same child for both tasks: SharedProvider repeats last.
    let spawner = spawner(
        Arc::new(SharedProvider::new(vec![child.clone(), child])),
        &store,
        1,
        3,
    );
    let registry = parent_registry(spawner);

    let parent = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "t1".into(),
                name: "task".into(),
            },
            task_call("t1", "a"),
            ProviderEvent::ToolCallStarted {
                id: "t2".into(),
                name: "task".into(),
            },
            task_call("t2", "b"),
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

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    run_turn(
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

    let session = store.load(&session_id).unwrap();
    let transcript = session.transcript();
    let results = &transcript[2].content;
    // One succeeds, one is an error telling the model not to retry.
    let errors: Vec<&ContentBlock> = results
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolResult { is_error: true, .. }))
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one capped call: {results:?}"
    );
    assert!(matches!(
        errors[0],
        ContentBlock::ToolResult { content, .. } if content.to_lowercase().contains("do not retry")
    ));
}

#[tokio::test]
async fn depth_cap_errors_with_guidance() {
    let (store, _session_id) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![vec![]])), &store, 10, 2);
    // A spawner already at max depth.
    let deep = SubagentSpawner::new(
        spawner.resolver(),
        store.clone(),
        spawner.agents().to_vec(),
        std::env::temp_dir(),
        2,
        10,
        2,
    );
    let registry = ToolRegistry::builtin()
        .with_subagents(Arc::new(deep))
        .unwrap();
    let task = registry.get("task").unwrap();

    let out = task
        .run(
            serde_json::json!({
                "description": "too deep",
                "prompt": "hello",
                "subagent_type": "explore",
            }),
            ToolContext::root(std::env::temp_dir()).with_subagents(Arc::new(SubagentSpawner::new(
                spawner.resolver(),
                store.clone(),
                spawner.agents().to_vec(),
                std::env::temp_dir(),
                2,
                10,
                2,
            ))),
        )
        .await;
    assert!(out.is_error);
    assert!(
        out.content.to_lowercase().contains("depth"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn explicit_task_id_errors_instead_of_starting_a_replacement() {
    let (store, _session_id) = temp_store();
    let child: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![vec![]]));
    let spawner = spawner(child, &store, 10, 3);
    let task = parent_registry(spawner).get("task").unwrap();

    for task_id in ["../escape".to_string(), new_id()] {
        let out = task
            .run(
                serde_json::json!({
                    "description": "resume",
                    "prompt": "continue",
                    "subagent_type": "explore",
                    "task_id": task_id,
                }),
                ToolContext::root(std::env::temp_dir()),
            )
            .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("resuming task session"),
            "{}",
            out.content
        );
    }
}

fn task_context(parent_id: &str) -> ToolContext {
    let mut context = ToolContext::root(std::env::temp_dir());
    context.session_id = parent_id.to_string();
    context
}

#[tokio::test]
async fn foreground_subagent_max_iterations_is_error() {
    let (store, parent_id) = temp_store();
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
    let task = parent_registry(spawner).get("task").unwrap();

    let output = task
        .run(
            serde_json::json!({
                "description": "bounded child",
                "prompt": "work",
                "subagent_type": "explore",
            }),
            task_context(&parent_id),
        )
        .await;

    assert!(output.is_error, "{}", output.content);
    assert!(output.content.contains("iteration"), "{}", output.content);
}

#[tokio::test]
async fn foreground_subagent_abort_is_error() {
    #[derive(Clone)]
    struct PendingAfterText {
        started: Arc<tokio::sync::Notify>,
    }
    impl Provider for PendingAfterText {
        fn stream(&self, _request: Request) -> anyhow::Result<EventStream> {
            self.started.notify_one();
            Ok(Box::pin(
                futures::stream::once(async { ProviderEvent::TextDelta("partial".into()) })
                    .chain(futures::stream::pending()),
            ))
        }
    }

    let (store, parent_id) = temp_store();
    let started = Arc::new(tokio::sync::Notify::new());
    let spawner = spawner(
        Arc::new(PendingAfterText {
            started: started.clone(),
        }),
        &store,
        10,
        3,
    );
    let task = parent_registry(spawner).get("task").unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut context = task_context(&parent_id);
    context.cancel = cancel.clone();
    let running = tokio::spawn(async move {
        task.run(
            serde_json::json!({
                "description": "cancel child",
                "prompt": "work",
                "subagent_type": "explore",
            }),
            context,
        )
        .await
    });
    started.notified().await;
    cancel.cancel();
    let output = running.await.unwrap();

    assert!(output.is_error, "{}", output.content);
    assert!(output.content.contains("aborted"), "{}", output.content);
}

#[tokio::test]
async fn tool_only_child_does_not_return_its_prompt() {
    let (store, parent_id) = temp_store();
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "missing".into(),
                name: "not_a_tool".into(),
            },
            ProviderEvent::ToolCallCompleted {
                id: "missing".into(),
                name: "not_a_tool".into(),
                input: serde_json::json!({}),
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
    let task = parent_registry(spawner(Arc::new(provider), &store, 10, 3))
        .get("task")
        .unwrap();
    let prompt = "do not echo this child prompt";

    let output = task
        .run(
            serde_json::json!({
                "description": "tool only",
                "prompt": prompt,
                "subagent_type": "explore",
            }),
            task_context(&parent_id),
        )
        .await;

    assert!(!output.is_error, "{}", output.content);
    assert!(!output.content.contains(prompt), "{}", output.content);
    assert!(output.content.contains("no text"), "{}", output.content);
}

#[tokio::test]
async fn resumed_subagent_rejects_an_already_active_session() {
    let (store, parent_id) = temp_store();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap(),
    );
    let spawner = spawner(
        Arc::new(ScriptedDelayProvider {
            text: "eventual answer",
            delay_ms: 300,
        }),
        &store,
        10,
        3,
    );
    let task = parent_registry(spawner.clone()).get("task").unwrap();
    let first = task
        .run(
            serde_json::json!({
                "description": "first resume",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": child_id,
                "background": true,
            }),
            task_context(&parent_id),
        )
        .await;
    assert!(!first.is_error, "{}", first.content);

    let second = task
        .run(
            serde_json::json!({
                "description": "duplicate resume",
                "prompt": "continue again",
                "subagent_type": "explore",
                "task_id": child_id,
            }),
            task_context(&parent_id),
        )
        .await;
    assert!(second.is_error, "{}", second.content);
    assert!(
        second.content.contains("already active"),
        "{}",
        second.content
    );

    spawner.shutdown().await;
}

#[tokio::test]
async fn resumed_subagent_rejects_persisted_agent_mismatch() {
    let (store, _parent_id) = temp_store();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: None,
                agent: "other".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap(),
    );
    let task = parent_registry(spawner(
        Arc::new(MockProvider::new(vec![vec![]])),
        &store,
        10,
        3,
    ))
    .get("task")
    .unwrap();

    let output = task
        .run(
            serde_json::json!({
                "description": "resume",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": child_id,
            }),
            ToolContext::root(std::env::temp_dir()),
        )
        .await;

    assert!(output.is_error);
    assert!(output.content.contains("persisted agent"));
}

#[tokio::test]
async fn subagent_turns_use_the_configured_compaction_threshold() {
    let (store, parent_id) = temp_store();
    let child_id = new_id();
    let mut child_session = store
        .create(SessionMeta {
            session_id: child_id.clone(),
            parent_id: Some(parent_id.clone()),
            agent: "explore".into(),
            model: "zai/glm-4.7".into(),
            workspace: None,
        })
        .unwrap();
    child_session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "old question ".repeat(40),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    child_session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content: vec![ContentBlock::Text {
                text: "old answer ".repeat(40),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    drop(child_session);
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::TextDelta("summary".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ],
        vec![
            ProviderEvent::TextDelta("fresh answer".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ],
    ]);
    let spawner = Arc::new(
        SubagentSpawner::new(
            Arc::new(FixedProviderResolver::new(Arc::new(provider.clone()))),
            store.clone(),
            vec![AgentDefinition {
                name: "explore".into(),
                description: "explores".into(),
                model: None,
                prompt: String::new(),
                workspace_mode: AgentWorkspaceMode::Mutable,
            }],
            std::env::temp_dir(),
            0,
            10,
            3,
        )
        .with_loop_config(LoopConfig {
            context_limit: Some(120),
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        }),
    );
    let task = parent_registry(spawner).get("task").unwrap();
    let mut ctx = ToolContext::root(std::env::temp_dir());
    ctx.session_id = parent_id;

    let output = task
        .run(
            serde_json::json!({
                "description": "resume compacted child",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": child_id,
            }),
            ctx,
        )
        .await;

    assert!(!output.is_error, "{}", output.content);
    assert_eq!(output.content, "fresh answer");
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn isolated_resume_requires_an_explicit_workspace() {
    let (_repo, root, worktree) = repository_with_worktree();
    let (store, parent_id) = temp_store();
    let persisted = ilar::tools::WorkspaceLocation::validated_git_worktree(
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
                parent_id: Some(parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: Some(persisted),
            })
            .unwrap(),
    );
    let task = parent_registry(spawner(
        Arc::new(MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }]])),
        &store,
        10,
        3,
    ))
    .get("task")
    .unwrap();
    let mut ctx = ToolContext::root(root);
    ctx.session_id = parent_id;

    let output = task
        .run(
            serde_json::json!({
                "description": "resume",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": child_id,
            }),
            ctx,
        )
        .await;

    assert!(output.is_error);
    assert!(
        output.content.contains("explicit workspace"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn nested_resume_may_inherit_its_parents_validated_worktree() {
    let (_repo, root, worktree) = repository_with_worktree();
    let (store, root_id) = temp_store();
    let location = ilar::tools::WorkspaceLocation::validated_git_worktree(
        &ilar::tools::WorkspaceLocation::shared(root),
        worktree.clone(),
    )
    .await
    .unwrap();
    let parent_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: parent_id.clone(),
                parent_id: Some(root_id),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: Some(location.clone()),
            })
            .unwrap(),
    );
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: Some(location.clone()),
            })
            .unwrap(),
    );
    let task = parent_registry(spawner(
        Arc::new(MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }]])),
        &store,
        10,
        3,
    ))
    .get("task")
    .unwrap();
    let mut ctx = ToolContext::root(worktree);
    ctx.session_id = parent_id;
    ctx.cwd = location.cwd().to_path_buf();
    ctx.workspace = ilar::tools::WorkspaceScheduler::for_location(&location);
    ctx.location = location;

    let output = task
        .run(
            serde_json::json!({
                "description": "resume inherited",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": child_id,
            }),
            ctx,
        )
        .await;

    assert!(!output.is_error, "{}", output.content);
}

#[tokio::test]
async fn resumed_subagent_rejects_a_different_parent_session() {
    let (store, parent_id) = temp_store();
    let other_parent_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: other_parent_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap(),
    );
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(other_parent_id),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap(),
    );
    let task = parent_registry(spawner(
        Arc::new(MockProvider::new(vec![vec![]])),
        &store,
        10,
        3,
    ))
    .get("task")
    .unwrap();
    let mut ctx = ToolContext::root(std::env::temp_dir());
    ctx.session_id = parent_id;

    let output = task
        .run(
            serde_json::json!({
                "description": "resume",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": child_id,
            }),
            ctx,
        )
        .await;

    assert!(output.is_error);
    assert!(
        output.content.contains("persisted parent"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn isolated_resume_rejects_a_different_cwd_in_the_same_worktree() {
    let (_repo, root, worktree) = repository_with_worktree();
    let nested = worktree.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let (store, parent_id) = temp_store();
    let persisted = ilar::tools::WorkspaceLocation::validated_git_worktree(
        &ilar::tools::WorkspaceLocation::shared(root.clone()),
        worktree,
    )
    .await
    .unwrap();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: Some(persisted),
            })
            .unwrap(),
    );
    let spawner = Arc::new(SubagentSpawner::new(
        Arc::new(FixedProviderResolver::new(Arc::new(MockProvider::new(
            vec![vec![ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }]],
        )))),
        store.clone(),
        vec![AgentDefinition {
            name: "explore".into(),
            description: "explores".into(),
            model: None,
            prompt: String::new(),
            workspace_mode: AgentWorkspaceMode::Mutable,
        }],
        root.clone(),
        0,
        10,
        3,
    ));
    let task = parent_registry(spawner).get("task").unwrap();
    let mut ctx = ToolContext::root(root);
    ctx.session_id = parent_id;

    let output = task
        .run(
            serde_json::json!({
                "description": "resume",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": child_id,
                "workspace": {"cwd": nested, "isolation": "git_worktree"},
            }),
            ctx,
        )
        .await;

    assert!(output.is_error);
    assert!(
        output.content.contains("workspace does not match"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn isolated_resume_rejects_tampered_persisted_workspace_metadata() {
    let (_repo, root, worktree) = repository_with_worktree();
    let (store, parent_id) = temp_store();
    let location = ilar::tools::WorkspaceLocation::validated_git_worktree(
        &ilar::tools::WorkspaceLocation::shared(root.clone()),
        worktree.clone(),
    )
    .await
    .unwrap();
    let mut serialized = serde_json::to_value(&location).unwrap();
    serialized["root"] = serde_json::json!(root.clone());
    let tampered = serde_json::from_value(serialized).unwrap();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: Some(tampered),
            })
            .unwrap(),
    );
    let task = parent_registry(spawner(
        Arc::new(MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }]])),
        &store,
        10,
        3,
    ))
    .get("task")
    .unwrap();
    let mut ctx = ToolContext::root(root);
    ctx.session_id = parent_id;

    let output = task
        .run(
            serde_json::json!({
                "description": "resume",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": child_id,
                "workspace": {"cwd": worktree, "isolation": "git_worktree"},
            }),
            ctx,
        )
        .await;

    assert!(output.is_error);
    assert!(
        output.content.contains("persisted workspace metadata"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn legacy_resume_cannot_adopt_an_isolated_workspace() {
    let (_repo, root, worktree) = repository_with_worktree();
    let (store, parent_id) = temp_store();
    let child_id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: child_id.clone(),
                parent_id: Some(parent_id.clone()),
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap(),
    );
    let spawner = Arc::new(SubagentSpawner::new(
        Arc::new(FixedProviderResolver::new(Arc::new(MockProvider::new(
            vec![vec![ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }]],
        )))),
        store.clone(),
        vec![AgentDefinition {
            name: "explore".into(),
            description: "explores".into(),
            model: None,
            prompt: String::new(),
            workspace_mode: AgentWorkspaceMode::Mutable,
        }],
        root.clone(),
        0,
        10,
        3,
    ));
    let task = parent_registry(spawner).get("task").unwrap();
    let mut ctx = ToolContext::root(root);
    ctx.session_id = parent_id;

    let output = task
        .run(
            serde_json::json!({
                "description": "resume",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": child_id,
                "workspace": {"cwd": worktree, "isolation": "git_worktree"},
            }),
            ctx,
        )
        .await;

    assert!(output.is_error);
    assert!(
        output.content.contains("no workspace metadata"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn child_session_created_with_parent_link() {
    let (store, session_id) = temp_store();
    store
        .acquire_writer(&session_id)
        .unwrap()
        .load()
        .unwrap()
        .append(ilar::session::SessionEvent::ModelChange {
            id: new_id(),
            model: "openai/gpt-5.2".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    let child: Arc<dyn Provider> = Arc::new(ScriptedDelayProvider {
        text: "found it",
        delay_ms: 10,
    });
    let spawner = spawner(child, &store, 10, 3);
    let registry = parent_registry(spawner);

    let parent = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "t1".into(),
                name: "task".into(),
            },
            task_call("t1", "look"),
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

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    run_turn(
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

    // Find the child session file and verify the parent link + prompt.
    let sessions_dir = store
        .session_path(&session_id)
        .unwrap()
        .parent()
        .unwrap()
        .read_dir()
        .unwrap();
    let mut child_ids: Vec<_> = sessions_dir
        .flatten()
        .filter(|e| {
            e.path().extension().is_some_and(|x| x == "jsonl")
                && e.path().file_stem().and_then(|s| s.to_str()) != Some(session_id.as_str())
        })
        .collect();
    assert_eq!(child_ids.len(), 1, "expected exactly one child session");
    let child_id = child_ids
        .pop()
        .unwrap()
        .path()
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let child_session = store.load(&child_id).unwrap();
    assert_eq!(
        child_session.meta().unwrap().parent_id,
        Some(session_id.clone())
    );
    assert_eq!(child_session.meta().unwrap().agent, "explore");
    assert_eq!(child_session.meta().unwrap().model, "openai/gpt-5.2");
    let child_transcript = child_session.transcript();
    assert!(matches!(
        &child_transcript[0].content[0],
        ContentBlock::Text { text } if text == "look"
    ));
}

#[tokio::test]
async fn subagent_model_override_resolves_its_own_provider() {
    let (store, session_id) = temp_store();
    let resolver = Arc::new(ModelResolver {
        zai: MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }]]),
        openai: MockProvider::new(vec![vec![
            ProviderEvent::TextDelta("openai child".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ]]),
    });
    let spawner = Arc::new(SubagentSpawner::new(
        resolver.clone(),
        store.clone(),
        vec![AgentDefinition {
            name: "explore".into(),
            description: "explores".into(),
            model: Some("openai/gpt-5.2".into()),
            prompt: String::new(),
            workspace_mode: AgentWorkspaceMode::Mutable,
        }],
        std::env::temp_dir(),
        0,
        10,
        3,
    ));
    let task = parent_registry(spawner).get("task").unwrap();
    let mut ctx = ToolContext::root(std::env::temp_dir());
    ctx.session_id = session_id;

    let output = task
        .run(
            serde_json::json!({
                "description": "route",
                "prompt": "use your model",
                "subagent_type": "explore",
            }),
            ctx,
        )
        .await;

    assert!(!output.is_error, "{}", output.content);
    assert!(resolver.zai.requests().is_empty());
    assert_eq!(resolver.openai.requests()[0].model, "openai/gpt-5.2");
}

#[tokio::test]
async fn resumed_subagent_uses_its_persisted_model() {
    let (store, parent_id) = temp_store();
    let child_id = new_id();
    let mut child = store
        .create(SessionMeta {
            session_id: child_id.clone(),
            parent_id: Some(parent_id.clone()),
            agent: "explore".into(),
            model: "zai/original".into(),
            workspace: None,
        })
        .unwrap();
    child
        .append(ilar::session::SessionEvent::ModelChange {
            id: new_id(),
            model: "openai/gpt-5.2".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    drop(child);
    let resolver = Arc::new(ModelResolver {
        zai: MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }]]),
        openai: MockProvider::new(vec![vec![ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }]]),
    });
    let spawner = Arc::new(SubagentSpawner::new(
        resolver.clone(),
        store,
        vec![AgentDefinition {
            name: "explore".into(),
            description: "explores".into(),
            model: Some("zai/current-default".into()),
            prompt: String::new(),
            workspace_mode: AgentWorkspaceMode::Mutable,
        }],
        std::env::temp_dir(),
        0,
        10,
        3,
    ));
    let task = parent_registry(spawner).get("task").unwrap();
    let mut ctx = ToolContext::root(std::env::temp_dir());
    ctx.session_id = parent_id;

    let output = task
        .run(
            serde_json::json!({
                "description": "resume",
                "prompt": "continue",
                "subagent_type": "explore",
                "task_id": child_id,
            }),
            ctx,
        )
        .await;

    assert!(!output.is_error, "{}", output.content);
    assert!(resolver.zai.requests().is_empty());
    assert_eq!(resolver.openai.requests()[0].model, "openai/gpt-5.2");
}

#[tokio::test]
async fn unknown_subagent_type_lists_available() {
    let (store, _s) = temp_store();
    let spawner = spawner(Arc::new(MockProvider::new(vec![vec![]])), &store, 10, 3);
    let registry = parent_registry(spawner);
    let task = registry.get("task").unwrap();
    let out = task
        .run(
            serde_json::json!({
                "description": "x",
                "prompt": "y",
                "subagent_type": "nonexistent",
            }),
            ToolContext::root(std::env::temp_dir()).with_subagents(spawner_for(&store)),
        )
        .await;
    assert!(out.is_error);
    assert!(
        out.content.contains("explore"),
        "should list available agents: {}",
        out.content
    );
}

fn spawner_for(store: &SessionStore) -> Arc<SubagentSpawner> {
    spawner(Arc::new(MockProvider::new(vec![vec![]])), store, 10, 3)
}
