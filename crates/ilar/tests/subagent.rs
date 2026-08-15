use std::sync::Arc;
use std::time::{Duration, Instant};

use ilar::agent::{LoopConfig, TurnOutcome, run_turn};
use ilar::config::{AgentDefinition, AgentWorkspaceMode};
use ilar::provider::{
    EventStream, FixedProviderResolver, MockProvider, Provider, ProviderEvent, ProviderHandle,
    ProviderResolver, Request, StopReason, resolve_model,
};
use ilar::session::{ChatMessage, ContentBlock, SessionMeta, SessionStore, Usage, new_id};
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
            task_call("t1", "find alpha"),
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
            task_call("t1", "a"),
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
