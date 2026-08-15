use ilar::agent::{LoopConfig, run_turn};
use ilar::provider::openai::OpenAIProvider;
use ilar::provider::{
    MockProvider, ProviderEvent, ProviderHandle, ProviderResolver, StopReason, resolve_model,
};
use ilar::session::{SessionEvent, SessionMeta, SessionStore, Usage, new_id};
use ilar::tools::{ToolContext, ToolRegistry};

fn temp_session(model: &str) -> (SessionStore, String) {
    let dir = std::env::temp_dir().join(format!("ilar-model-test-{}", new_id()));
    let store = SessionStore::new(dir);
    let id = new_id();
    store
        .create(SessionMeta {
            session_id: id.clone(),
            parent_id: None,
            agent: "build".into(),
            model: model.into(),
            workspace: None,
        })
        .unwrap();
    (store, id)
}

fn text_turn() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta("ok".into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]
}

struct RoutingResolver {
    zai: MockProvider,
    openai: MockProvider,
}

impl ProviderResolver for RoutingResolver {
    fn resolve_provider(&self, model: &str) -> anyhow::Result<ProviderHandle<'_>> {
        let (provider, _) = resolve_model(model)?;
        match provider {
            "zai" => Ok(ProviderHandle::Borrowed(&self.zai)),
            "openai" => Ok(ProviderHandle::Borrowed(&self.openai)),
            _ => anyhow::bail!("unknown provider {provider}"),
        }
    }
}

#[test]
fn effective_model_defaults_to_meta() {
    let (store, id) = temp_session("zai/glm-4.7");
    let session = store.load(&id).unwrap();
    assert_eq!(session.effective_model(), "zai/glm-4.7");
}

#[tokio::test]
async fn model_change_applies_from_next_provider_call() {
    let (store, session_id) = temp_session("zai/glm-4.7");
    let provider = MockProvider::new(vec![text_turn(), text_turn(), text_turn()]);
    let registry = ToolRegistry::builtin();

    // Turn 1 on the original model.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "first",
        None,
        LoopConfig::default(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();
    assert_eq!(provider.requests()[0].model, "zai/glm-4.7");

    // Switch mid-session (never mid-stream: between turns).
    store
        .acquire_writer(&session_id)
        .unwrap()
        .load()
        .unwrap()
        .append(SessionEvent::ModelChange {
            id: new_id(),
            model: "zai/glm-4.7-air".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();

    // Turn 2 uses the new model, and the change is audited in the JSONL.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "second",
        None,
        LoopConfig::default(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();
    assert_eq!(provider.requests()[1].model, "zai/glm-4.7-air");

    let session = store.load(&session_id).unwrap();
    assert_eq!(session.effective_model(), "zai/glm-4.7-air");
    let has_change = session.events().iter().any(
        |e| matches!(e, SessionEvent::ModelChange { model, .. } if model == "zai/glm-4.7-air"),
    );
    assert!(has_change, "model change not audited");

    // Assistant messages record the model they ran on.
    let assistant_models: Vec<String> = session
        .events()
        .iter()
        .filter_map(|e| match e {
            SessionEvent::AssistantMessage { model, .. } => Some(model.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(assistant_models, vec!["zai/glm-4.7", "zai/glm-4.7-air"]);
}

#[tokio::test]
async fn turn_resolves_provider_from_persisted_effective_model() {
    let (store, session_id) = temp_session("zai/glm-4.7");
    store
        .acquire_writer(&session_id)
        .unwrap()
        .load()
        .unwrap()
        .append(SessionEvent::ModelChange {
            id: new_id(),
            model: "openai/gpt-5.2".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    let resolver = RoutingResolver {
        zai: MockProvider::new(vec![text_turn()]),
        openai: MockProvider::new(vec![text_turn()]),
    };

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    run_turn(
        &resolver,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "continue",
        None,
        LoopConfig::default(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();

    assert!(resolver.zai.requests().is_empty());
    assert_eq!(resolver.openai.requests()[0].model, "openai/gpt-5.2");
}

#[tokio::test]
async fn provider_resolution_failure_does_not_append_user_message() {
    struct RejectingResolver;
    impl ProviderResolver for RejectingResolver {
        fn resolve_provider(&self, model: &str) -> anyhow::Result<ProviderHandle<'_>> {
            anyhow::bail!("no credentials for {model}")
        }
    }

    let (store, session_id) = temp_session("openai/gpt-5.2");
    let before = store.load(&session_id).unwrap().events().len();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let error = run_turn(
        &RejectingResolver,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "must not persist",
        None,
        LoopConfig::default(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("no credentials"));
    assert_eq!(store.load(&session_id).unwrap().events().len(), before);
}

#[tokio::test]
async fn concrete_provider_prefix_mismatch_fails_before_user_append() {
    let (store, session_id) = temp_session("zai/glm-4.7");
    let provider = OpenAIProvider::new("test-key".into(), None);
    let before = store.load(&session_id).unwrap().events().len();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

    let error = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "must not persist",
        None,
        LoopConfig::default(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("cannot serve"));
    assert_eq!(store.load(&session_id).unwrap().events().len(), before);
}
