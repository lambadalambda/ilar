use ilar::agent::{LoopConfig, TurnOutcome, run_turn};
use std::sync::Arc;
use std::time::Duration;

use ilar::provider::{
    EventStream, MockProvider, Provider, ProviderEvent, ProviderHandle, ProviderResolver, Request,
    StopReason, ToolDefinition, resolve_model,
};
use ilar::session::{InputTokenAccounting, SessionEvent, SessionMeta, SessionStore, Usage, new_id};
use ilar::tools::{ToolContext, ToolRegistry};

fn temp_session() -> (SessionStore, String) {
    let dir = std::env::temp_dir().join(format!("ilar-compaction-test-{}", new_id()));
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

fn text_turn(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 50,
                output_tokens: 5,
                ..Default::default()
            },
        },
    ]
}

fn tiny_config() -> LoopConfig {
    LoopConfig {
        context_limit: Some(120), // tokens: tiny → compaction on every turn
        compaction_threshold: 0.5,
        ..LoopConfig::default()
    }
}

fn seed_compactable_history(store: &SessionStore, session_id: &str) {
    let mut session = store.acquire_writer(session_id).unwrap().load().unwrap();
    for i in 0..6 {
        session
            .append(SessionEvent::UserMessage {
                id: new_id(),
                text: format!("old question {i} {}", "context ".repeat(20)),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::Text {
                    text: format!("old answer {i} {}", "detail ".repeat(20)),
                }],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
    }
}

#[tokio::test]
async fn estimate_ignores_usage_before_latest_compaction_boundary() {
    let (store, session_id) = temp_session();
    let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "old context".repeat(20),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content: vec![ilar::session::ContentBlock::Text {
                text: "old answer".into(),
            }],
            usage: Usage {
                input_tokens: 10_000,
                input_token_accounting: Some(InputTokenAccounting::ExcludesCached),
                ..Default::default()
            },
            stop_reason: "end_turn".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    let kept_from = session.events().len();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "current question".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "small summary".into(),
            kept_from,
            ts: chrono::Utc::now(),
        })
        .unwrap();

    assert!(ilar::compaction::estimate_tokens(&session) < 100);
    drop(session);

    let provider = MockProvider::new(vec![text_turn("answer")]);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    run_turn(
        &provider,
        &ToolRegistry::read_only(),
        &store,
        &session_id,
        "follow-up",
        None,
        LoopConfig {
            context_limit: Some(10_000),
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        },
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();

    assert_eq!(
        provider.requests().len(),
        1,
        "pre-boundary usage retriggered compaction"
    );
}

#[test]
fn estimate_includes_system_prompt_and_tool_definitions() {
    let (store, session_id) = temp_session();
    let session = store.acquire_writer(&session_id).unwrap().load().unwrap();
    let system_prompt = "s".repeat(400);
    let tools = vec![ToolDefinition {
        name: "large".into(),
        description: "d".repeat(400),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"payload": {"type": "string", "description": "x".repeat(400)}}
        }),
    }];

    assert!(
        ilar::compaction::estimate_tokens_with_request(&session, Some(&system_prompt), &tools)
            >= 250
    );
}

#[test]
fn estimate_treats_unversioned_legacy_usage_as_ambiguous() {
    let (store, session_id) = temp_session();
    let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
    let legacy: Usage = serde_json::from_value(serde_json::json!({
        "input_tokens": 10_000,
        "output_tokens": 5,
        "cache_read_input_tokens": 9_000
    }))
    .unwrap();
    assert!(legacy.input_token_accounting.is_none());
    session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: "openai/gpt-5.6-sol".into(),
            content: vec![ilar::session::ContentBlock::Text {
                text: "small legacy answer".into(),
            }],
            usage: legacy,
            stop_reason: "end_turn".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();

    assert!(ilar::compaction::estimate_tokens(&session) < 100);
}

struct RoutingResolver {
    zai: MockProvider,
    openai: MockProvider,
}

impl ProviderResolver for RoutingResolver {
    fn resolve_provider(&self, model: &str) -> anyhow::Result<ProviderHandle<'_>> {
        match resolve_model(model)?.0 {
            "zai" => Ok(ProviderHandle::Borrowed(&self.zai)),
            "openai" => Ok(ProviderHandle::Borrowed(&self.openai)),
            provider => anyhow::bail!("unknown provider {provider}"),
        }
    }
}

#[tokio::test]
async fn oversize_transcript_triggers_compaction() {
    let (store, session_id) = temp_session();
    // Seed a long history: several user/assistant exchanges.
    {
        let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
        for i in 0..6 {
            session
                .append(SessionEvent::UserMessage {
                    id: new_id(),
                    text: format!(
                        "question number {i} about the frobnicator module and its config"
                    ),
                    ts: chrono::Utc::now(),
                })
                .unwrap();
            session
                .append(SessionEvent::AssistantMessage {
                    id: new_id(),
                    model: "zai/glm-4.7".into(),
                    content: vec![ilar::session::ContentBlock::Text {
                        text: format!("answer number {i} with substantial explanation text"),
                    }],
                    usage: Usage::default(),
                    stop_reason: "end_turn".into(),
                    ts: chrono::Utc::now(),
                })
                .unwrap();
        }
        session
            .append(SessionEvent::ModelChange {
                id: new_id(),
                model: "openai/gpt-5.2".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
    }

    // Script: turn 1 = the compaction summary, turn 2 = the real answer.
    let resolver = RoutingResolver {
        zai: MockProvider::error("wrong provider"),
        openai: MockProvider::new(vec![
            text_turn("SUMMARY: user asked about frobnicator; answered 6 times."),
            text_turn("fresh answer"),
        ]),
    };
    let registry = ToolRegistry::builtin();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = run_turn(
        &resolver,
        &registry,
        &store,
        &session_id,
        "next question",
        None,
        tiny_config(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    // Two provider calls: compaction + real.
    assert!(resolver.zai.requests().is_empty());
    assert_eq!(resolver.openai.requests().len(), 2);
    // The compaction request carried the old transcript + summarizer prompt.
    let requests = resolver.openai.requests();
    let first = &requests[0];
    assert_eq!(first.model, "openai/gpt-5.2");
    assert!(
        first
            .system_prompt
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("summar"),
        "compaction system prompt missing: {:?}",
        first.system_prompt
    );
    assert!(
        first.tools.is_empty(),
        "compaction call must not carry tools"
    );
    assert_eq!(requests[1].model, "openai/gpt-5.2");

    // Session contains a compaction event before the new exchange.
    let session = store.load(&session_id).unwrap();
    let events = session.events();
    let has_compaction = events
        .iter()
        .any(|e| matches!(e, SessionEvent::Compaction { .. }));
    assert!(has_compaction, "no compaction event: {events:?}");

    // Reload produces the compacted view: summary + current tail only.
    let transcript = session.transcript();
    let rendered = format!("{transcript:?}");
    assert!(rendered.contains("SUMMARY: user asked"), "{rendered}");
    assert!(rendered.contains("next question"), "{rendered}");
    assert!(
        !rendered.contains("question number 0"),
        "old turns leaked past compaction: {rendered}"
    );
    assert!(rendered.contains("fresh answer"), "{rendered}");
    // Alternation invariant holds.
    for pair in transcript.windows(2) {
        assert_ne!(pair[0].role, pair[1].role);
    }
}

#[tokio::test]
async fn compaction_rejects_partial_summary_at_eof() {
    let (store, session_id) = temp_session();
    seed_compactable_history(&store, &session_id);
    let provider = MockProvider::new(vec![vec![ProviderEvent::TextDelta("partial".into())]]);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

    let error = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "new question",
        None,
        tiny_config(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("before completion"), "{error:#}");
    assert!(
        !store
            .load(&session_id)
            .unwrap()
            .events()
            .iter()
            .any(|event| { matches!(event, SessionEvent::Compaction { .. }) })
    );
}

#[tokio::test]
async fn compaction_rejects_non_end_turn_terminal_states() {
    for stop_reason in [
        StopReason::ToolUse,
        StopReason::MaxTokens,
        StopReason::Refusal,
        StopReason::Paused,
        StopReason::Stopped,
    ] {
        let (store, session_id) = temp_session();
        seed_compactable_history(&store, &session_id);
        let provider = MockProvider::new(vec![vec![
            ProviderEvent::TextDelta("partial".into()),
            ProviderEvent::TurnComplete {
                stop_reason: stop_reason.clone(),
                usage: Usage::default(),
            },
        ]]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let error = run_turn(
            &provider,
            &ToolRegistry::builtin(),
            &store,
            &session_id,
            "new question",
            None,
            tiny_config(),
            tx,
            tokio_util::sync::CancellationToken::new(),
            ToolContext::root(std::env::temp_dir()),
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("invalid stop reason"),
            "{error:#}"
        );
        assert!(
            !store
                .load(&session_id)
                .unwrap()
                .events()
                .iter()
                .any(|event| { matches!(event, SessionEvent::Compaction { .. }) })
        );
    }
}

struct PendingCompactionProvider {
    started: Arc<tokio::sync::Notify>,
}

impl Provider for PendingCompactionProvider {
    fn stream(&self, _request: Request) -> anyhow::Result<EventStream> {
        self.started.notify_one();
        Ok(Box::pin(futures::stream::pending()))
    }
}

#[tokio::test]
async fn cancellation_stops_in_flight_compaction_without_persisting() {
    let (store, session_id) = temp_session();
    seed_compactable_history(&store, &session_id);
    let started = Arc::new(tokio::sync::Notify::new());
    let provider = PendingCompactionProvider {
        started: started.clone(),
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let turn_cancel = cancel.clone();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let turn = tokio::spawn(async move {
        let outcome = run_turn(
            &provider,
            &ToolRegistry::builtin(),
            &store,
            &session_id,
            "new question",
            None,
            tiny_config(),
            tx,
            turn_cancel,
            ToolContext::root(std::env::temp_dir()),
        )
        .await;
        (outcome, store, session_id)
    });
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("compaction did not start");

    cancel.cancel();
    let (outcome, store, session_id) = tokio::time::timeout(Duration::from_secs(1), turn)
        .await
        .expect("compaction ignored cancellation")
        .unwrap();

    assert_eq!(outcome.unwrap(), TurnOutcome::Aborted);
    assert!(
        !store
            .load(&session_id)
            .unwrap()
            .events()
            .iter()
            .any(|event| { matches!(event, SessionEvent::Compaction { .. }) })
    );
}

#[tokio::test]
async fn small_transcript_skips_compaction() {
    let (store, session_id) = temp_session();
    let provider = MockProvider::new(vec![text_turn("answer")]);
    let registry = ToolRegistry::builtin();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "hi",
        None,
        LoopConfig {
            context_limit: Some(1_000_000),
            ..LoopConfig::default()
        },
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();

    assert_eq!(
        provider.requests().len(),
        1,
        "compaction ran on a small session"
    );
    let session = store.load(&session_id).unwrap();
    assert!(
        !session
            .events()
            .iter()
            .any(|e| matches!(e, SessionEvent::Compaction { .. }))
    );
}

#[tokio::test]
async fn no_context_limit_disables_compaction() {
    let (store, session_id) = temp_session();
    let provider = MockProvider::new(vec![text_turn("answer")]);
    let registry = ToolRegistry::builtin();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "hi",
        None,
        LoopConfig::default(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();

    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn compaction_falls_back_to_chars_estimate_without_usage() {
    // No prior usage on events; the estimator must still fire on raw size.
    let (store, session_id) = temp_session();
    {
        let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
        session
            .append(SessionEvent::UserMessage {
                id: new_id(),
                text: "x".repeat(4000),
                ts: chrono::Utc::now(),
            })
            .unwrap();
    }
    let provider = MockProvider::new(vec![text_turn("SUMMARY"), text_turn("ok")]);
    let registry = ToolRegistry::builtin();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "again",
        None,
        tiny_config(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
    )
    .await
    .unwrap();
    assert_eq!(
        provider.requests().len(),
        2,
        "chars/4 fallback did not trigger"
    );
}
