use ilar::agent::{LoopConfig, TurnOutcome, run_turn};
use ilar::provider::{MockProvider, ProviderEvent, StopReason};
use ilar::session::{SessionEvent, SessionMeta, SessionStore, Usage, new_id};
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

#[tokio::test]
async fn oversize_transcript_triggers_compaction() {
    let (store, session_id) = temp_session();
    // Seed a long history: several user/assistant exchanges.
    {
        let mut session = store.load(&session_id).unwrap();
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
    }

    // Script: turn 1 = the compaction summary, turn 2 = the real answer.
    let provider = MockProvider::new(vec![
        text_turn("SUMMARY: user asked about frobnicator; answered 6 times."),
        text_turn("fresh answer"),
    ]);
    let registry = ToolRegistry::builtin();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = run_turn(
        &provider,
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
    assert_eq!(provider.requests().len(), 2);
    // The compaction request carried the old transcript + summarizer prompt.
    let first = &provider.requests()[0];
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
        let mut session = store.load(&session_id).unwrap();
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
