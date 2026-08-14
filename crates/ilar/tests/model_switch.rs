use ilar::agent::{LoopConfig, run_turn};
use ilar::provider::{MockProvider, ProviderEvent, StopReason};
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
        .load(&session_id)
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
