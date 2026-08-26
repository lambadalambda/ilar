use ilar::agent::{LOOP_EVENT_CAPACITY, LoopConfig, TurnOutcome, loop_event_channel, run_turn};
use std::sync::Arc;
use std::time::Duration;

use ilar::compaction::{ManualCompactionOutcome, compact_session};
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
            cwd: None,
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
                images: Vec::new(),
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
async fn manual_compaction_of_empty_session_is_a_local_no_op() {
    let (store, session_id) = temp_session();
    let provider = MockProvider::new(Vec::new());
    let cancel = tokio_util::sync::CancellationToken::new();

    let outcome = compact_session(&provider, &store, &session_id, None, &[], &cancel)
        .await
        .unwrap();

    assert_eq!(outcome, ManualCompactionOutcome::NothingToCompact);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn manual_active_history_compaction_persists_without_a_new_user_message() {
    let (store, session_id) = temp_session();
    seed_compactable_history(&store, &session_id);
    let provider = MockProvider::new(vec![
        text_turn("handover: all important context"),
        text_turn("handover: compacted context remains"),
    ]);
    let cancel = tokio_util::sync::CancellationToken::new();

    let outcome = compact_session(&provider, &store, &session_id, Some("system"), &[], &cancel)
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        ManualCompactionOutcome::Compacted { ref summary, .. }
            if summary.contains("handover: all important context")
    ));
    assert_eq!(provider.requests().len(), 1);
    let session = store.load(&session_id).unwrap();
    let canonical_events = std::fs::read_to_string(store.session_path(&session_id).unwrap())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<SessionEvent>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        canonical_events
            .iter()
            .filter(|event| matches!(event, SessionEvent::UserMessage { .. }))
            .count(),
        6,
        "manual compaction appended a user message"
    );
    assert!(matches!(
        canonical_events.last(),
        Some(SessionEvent::Compaction { summary, .. })
            // The summary is the whole of the handover: no pins, no
            // window, nothing stapled on.
            if summary == "handover: all important context"
    ));
    let transcript = session.transcript();
    assert_eq!(transcript.len(), 1);
    assert!(format!("{transcript:?}").contains("handover: all important context"));
    drop(session);

    let second = compact_session(&provider, &store, &session_id, Some("system"), &[], &cancel)
        .await
        .unwrap();
    assert!(matches!(
        second,
        ManualCompactionOutcome::Compacted { ref summary, .. }
            if summary.contains("handover: compacted context remains")
    ));
    assert_eq!(provider.requests().len(), 2);
    assert!(
        format!("{:?}", store.load(&session_id).unwrap().transcript())
            .contains("handover: compacted context remains")
    );
}

#[tokio::test]
async fn estimate_ignores_usage_before_latest_compaction_boundary() {
    let (store, session_id) = temp_session();
    let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "old context".repeat(20),
            images: Vec::new(),
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
            images: Vec::new(),
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
    let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);
    run_turn(
        &provider,
        &ToolRegistry::read_only(),
        &store,
        &session_id,
        "follow-up",
        &[],
        None,
        LoopConfig {
            context_limit: Some(10_000),
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        },
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
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

/// A screenshot handed back by a tool is real context. Base64
/// tokenizes roughly like text, so the estimate has to grow with the
/// payload — otherwise an image-heavy session sails past the
/// compaction threshold reporting a few hundred tokens.
#[test]
fn estimate_counts_tool_result_image_payloads() {
    let data = "A".repeat(4_000);
    let text_only = estimate_with_tool_result_images(Vec::new());
    let with_image = estimate_with_tool_result_images(vec![ilar::session::ImageContent {
        media_type: "image/png".into(),
        data: data.clone(),
    }]);

    assert_eq!(with_image - text_only, (data.len() / 4) as u64);
}

fn estimate_with_tool_result_images(images: Vec<ilar::session::ImageContent>) -> u64 {
    let (store, session_id) = temp_session();
    let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content: vec![ilar::session::ContentBlock::ToolCall {
                id: "shot-1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "shot.png"}),
                item_id: None,
            }],
            usage: Usage::default(),
            stop_reason: "tool_use".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: "shot-1".into(),
            content: "the image itself follows".into(),
            is_error: false,
            images,
            child_session_id: None,
            state: None,
            ts: chrono::Utc::now(),
        })
        .unwrap();
    ilar::compaction::estimate_tokens(&session)
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
                    images: Vec::new(),
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
                variant: Some("high".into()),
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

    let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);
    let outcome = run_turn(
        &resolver,
        &registry,
        &store,
        &session_id,
        "next question",
        &[],
        None,
        tiny_config(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    // Two provider calls: compaction + real.
    assert!(resolver.zai.requests().is_empty());
    assert_eq!(resolver.openai.requests().len(), 2);
    // The compaction request is the turn's own request with the
    // summarization instruction appended: same system prompt, same
    // tools, same cache key, so the conversation is served from the
    // provider's prompt cache instead of being re-read at full price.
    let requests = resolver.openai.requests();
    let first = &requests[0];
    assert_eq!(first.model, "openai/gpt-5.2");
    assert_eq!(first.system_prompt, requests[1].system_prompt);
    assert_eq!(first.tools.len(), requests[1].tools.len());
    assert_eq!(first.cache_key, requests[1].cache_key);
    let instruction = format!("{:?}", first.messages.last().unwrap());
    assert!(
        instruction.contains("Stop working on the task") && instruction.contains("## Objective"),
        "compaction instruction missing: {instruction}"
    );
    assert_eq!(requests[1].model, "openai/gpt-5.2");
    let expected_options = serde_json::json!({"reasoning": {"effort": "high"}});
    assert_eq!(requests[0].options, expected_options);
    assert_eq!(requests[1].options, expected_options);

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
    // Everything before the current message is the summary's job now,
    // questions included; the archive keeps the originals.
    assert!(
        !rendered.contains("answer number 0"),
        "old turns leaked past compaction: {rendered}"
    );
    assert!(
        !rendered.contains("question number 0"),
        "a recency window survived: {rendered}"
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
    let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);

    let error = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "new question",
        &[],
        None,
        tiny_config(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
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
        let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);

        let error = run_turn(
            &provider,
            &ToolRegistry::builtin(),
            &store,
            &session_id,
            "new question",
            &[],
            None,
            tiny_config(),
            tx,
            tokio_util::sync::CancellationToken::new(),
            ToolContext::root(std::env::temp_dir()),
            None,
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
    let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);
    let turn = tokio::spawn(async move {
        let outcome = run_turn(
            &provider,
            &ToolRegistry::builtin(),
            &store,
            &session_id,
            "new question",
            &[],
            None,
            tiny_config(),
            tx,
            turn_cancel,
            ToolContext::root(std::env::temp_dir()),
            None,
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

    let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);
    let _ = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "hi",
        &[],
        None,
        LoopConfig {
            context_limit: Some(1_000_000),
            ..LoopConfig::default()
        },
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
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

    let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);
    let _ = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "hi",
        &[],
        None,
        LoopConfig::default(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
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
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
    }
    let provider = MockProvider::new(vec![text_turn("SUMMARY"), text_turn("ok")]);
    let registry = ToolRegistry::builtin();
    let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);
    let _ = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "again",
        &[],
        None,
        tiny_config(),
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        provider.requests().len(),
        2,
        "chars/4 fallback did not trigger"
    );
}

#[tokio::test]
async fn forced_compaction_runs_below_the_threshold_and_reports_its_summary() {
    let (store, session_id) = temp_session();
    {
        let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
        session
            .append(SessionEvent::UserMessage {
                id: new_id(),
                text: "earlier question".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::Text {
                    text: "earlier answer".into(),
                }],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
    }
    let provider = MockProvider::new(vec![
        text_turn("SUMMARY: one earlier exchange."),
        text_turn("fresh answer"),
    ]);
    let registry = ToolRegistry::builtin();

    let (tx, mut rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
    let outcome = run_turn(
        &provider,
        &registry,
        &store,
        &session_id,
        "new question",
        &[],
        None,
        LoopConfig {
            context_limit: Some(1_000_000),
            force_compaction: true,
            ..LoopConfig::default()
        },
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(std::env::temp_dir()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome, TurnOutcome::Completed);

    // The tiny transcript compacted anyway, and the event carries the summary.
    let session = store.load(&session_id).unwrap();
    assert!(
        session
            .events()
            .iter()
            .any(|e| matches!(e, SessionEvent::Compaction { summary, .. }
                if summary.contains("one earlier exchange")))
    );
    let mut saw_summary = false;
    while let Ok(event) = rx.try_recv() {
        if let ilar::agent::LoopEvent::Compacted { summary, .. } = event {
            assert!(summary.contains("one earlier exchange"), "{summary}");
            saw_summary = true;
        }
    }
    assert!(saw_summary, "Compacted event with summary published");
}

/// A single agentic turn can outgrow the window on its own: the loop
/// runs many provider steps per user message, and compaction that only
/// fires at turn start never gets a chance. This is the shape that
/// killed session 4466f66d (3 user messages, 44 assistant steps,
/// 1.5k -> 127k tokens, no compaction).
#[tokio::test]
async fn context_growing_between_steps_compacts_without_a_new_user_message() {
    let (store, session_id) = temp_session();
    let workspace = tempfile::tempdir().unwrap();
    // A file large enough that reading it blows past the tiny threshold.
    for (name, marker) in [("big1.txt", "alpha-marker"), ("big2.txt", "beta-marker")] {
        std::fs::write(
            workspace.path().join(name),
            format!("{marker} dolor sit amet consectetur\n").repeat(400),
        )
        .unwrap();
    }

    let read_call = |id: &str, file: &str| {
        vec![
            ProviderEvent::ToolCallStarted {
                id: id.into(),
                name: "read".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: id.into(),
                name: "read".into(),
                input: serde_json::json!({ "path": file }),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ]
    };

    let resolver = RoutingResolver {
        zai: MockProvider::new(vec![
            read_call("call-1", "big1.txt"),
            // The loop re-enters over the threshold, so the next
            // provider call is the compaction summary rather than a
            // step — no new user message involved.
            text_turn("SUMMARY: read big1.txt while investigating."),
            read_call("call-2", "big2.txt"),
            text_turn("SUMMARY: read both files while investigating."),
            text_turn("done investigating"),
        ]),
        openai: MockProvider::error("wrong provider"),
    };
    let registry = ToolRegistry::builtin();
    let (tx, _) = loop_event_channel(LOOP_EVENT_CAPACITY);
    let outcome = run_turn(
        &resolver,
        &registry,
        &store,
        &session_id,
        "investigate big.txt",
        &[],
        None,
        LoopConfig {
            context_limit: Some(400),
            compaction_threshold: 0.5,
            ..LoopConfig::default()
        },
        tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(workspace.path().to_path_buf()),
        None,
    )
    .await
    .unwrap();

    assert_eq!(outcome, TurnOutcome::Completed);
    let requests = resolver.zai.requests();
    assert_eq!(
        requests.len(),
        5,
        "expected step, compaction, step, compaction, step; got {}",
        requests.len()
    );
    // The third call is the summarizer. It is identified by its final
    // message, not by a different system prompt: the whole prefix is
    // deliberately identical to the surrounding steps so the provider
    // serves it from its prompt cache.
    let instruction = format!("{:?}", requests[1].messages.last().unwrap());
    assert!(
        instruction.contains("Stop working on the task"),
        "second call was not a compaction: {instruction}"
    );
    assert_eq!(requests[1].system_prompt, requests[2].system_prompt);
    assert_eq!(requests[1].tools.len(), requests[2].tools.len());
    assert_eq!(requests[1].cache_key, requests[2].cache_key);

    let session = store.load(&session_id).unwrap();
    assert!(
        session
            .events()
            .iter()
            .any(|e| matches!(e, SessionEvent::Compaction { .. })),
        "no compaction event persisted"
    );
    // The final step ran on the compacted transcript: a summary and
    // nothing else, with the bulky results gone.
    let rendered = format!("{:?}", requests[4].messages);
    assert!(rendered.contains("SUMMARY: read both files"), "{rendered}");
    // Both bulky results are gone — the recent one too. That is the
    // point of the handover: what survives is the summary, and the
    // originals stay in the archive for the history tool.
    assert!(!rendered.contains("alpha-marker"), "{rendered}");
    assert!(!rendered.contains("beta-marker"), "{rendered}");
    for pair in session.transcript().windows(2) {
        assert_ne!(pair[0].role, pair[1].role);
    }
}

/// Providers reject on input tokens, not total context. Compaction must
/// trigger below the input cap, not below the whole window.
#[test]
fn every_catalog_model_compacts_below_its_input_cap() {
    let threshold = LoopConfig::default().compaction_threshold;
    let offenders: Vec<String> = ilar::model::catalog()
        .iter()
        .filter_map(|model| {
            let trigger =
                ilar::compaction::trigger_tokens(ilar::model::compaction_limit(model), threshold);
            (trigger > model.input_limit).then(|| {
                format!(
                    "{}/{}: fires at {trigger} but input cap is {}",
                    model.provider, model.id, model.input_limit
                )
            })
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "compaction fires above the provider's input cap:\n{}",
        offenders.join("\n")
    );
}

#[tokio::test]
async fn turn_boundary_compaction_keeps_the_checkpoint_with_its_message() {
    let (store, session_id) = temp_session();
    seed_compactable_history(&store, &session_id);
    // A git repo cwd makes the turn record a tree checkpoint just
    // before its user message; the boundary cut must not separate them,
    // or a later rewind to this turn loses its tree snapshot.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    std::fs::create_dir(&root).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    let provider = MockProvider::new(vec![
        text_turn("summary of the old turns"),
        text_turn("done"),
    ]);
    let (events_tx, _events_rx) = loop_event_channel(LOOP_EVENT_CAPACITY);

    let outcome = run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "new question",
        &[],
        Some("system"),
        tiny_config(),
        events_tx,
        tokio_util::sync::CancellationToken::new(),
        ToolContext::root(root),
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome, TurnOutcome::Completed);

    let reader = store.load(&session_id).unwrap();
    let events = reader.events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::Compaction { .. })),
        "the tiny context limit must actually force a compaction"
    );
    let user = events
        .iter()
        .position(
            |event| matches!(event, SessionEvent::UserMessage { text, .. } if text == "new question"),
        )
        .unwrap();
    assert!(
        matches!(events[user - 1], SessionEvent::Checkpoint { .. }),
        "the kept window must open with the turn's checkpoint, got {:?}",
        events[user - 1]
    );
}

#[tokio::test]
async fn an_apology_is_reported_not_repaired() {
    let (store, session_id) = temp_session();
    seed_compactable_history(&store, &session_id);
    // The failure observed in session 3d494ad6: the summarizer answered
    // the conversation instead of summarizing it.
    let provider = MockProvider::new(vec![text_turn(
        "I'm sorry, but I wasn't able to complete and push all four fixes.",
    )]);
    let cancel = tokio_util::sync::CancellationToken::new();

    let error = compact_session(&provider, &store, &session_id, Some("system"), &[], &cancel)
        .await
        .expect_err("a session must not be replaced by an apology");

    assert!(
        format!("{error:#}").contains("answered the conversation"),
        "{error:#}"
    );
    // One call: no retry, no repair, nothing clever. The summary is the
    // whole of what would have survived, so a bad one is reported and
    // the session is left exactly as it was.
    assert_eq!(provider.requests().len(), 1);
    assert!(
        !store
            .load(&session_id)
            .unwrap()
            .events()
            .iter()
            .any(|event| matches!(event, SessionEvent::Compaction { .. })),
        "a refused compaction still touched the session"
    );
}

#[tokio::test]
async fn what_the_summary_drops_stays_findable() {
    let (store, session_id) = temp_session();
    let request = "Can we do the necessary changes to firehose to support bundled \
                   payments? https://github.com/yodlpay/yodl-lite/pull/396";
    {
        let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
        session
            .append(SessionEvent::UserMessage {
                id: new_id(),
                text: request.into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        for i in 0..6 {
            session
                .append(SessionEvent::AssistantMessage {
                    id: new_id(),
                    model: "zai/glm-4.7".into(),
                    content: vec![ilar::session::ContentBlock::Text {
                        text: format!("step {i} {}", "detail ".repeat(50)),
                    }],
                    usage: Usage::default(),
                    stop_reason: "end_turn".into(),
                    ts: chrono::Utc::now(),
                })
                .unwrap();
        }
    }
    // A summarizer that writes a work log and forgets what was asked —
    // which is what the real one did.
    let provider = MockProvider::new(vec![text_turn(
        "Implemented bundled-payment support across Firehose and its producers.",
    )]);
    let cancel = tokio_util::sync::CancellationToken::new();

    compact_session(&provider, &store, &session_id, Some("system"), &[], &cancel)
        .await
        .unwrap();

    // The handover is the summary alone: the request is gone from what
    // the model is sent, deliberately, and nothing is pinned to it.
    let transcript = format!("{:?}", store.load(&session_id).unwrap().transcript());
    assert!(!transcript.contains("pull/396"), "{transcript}");
    assert!(
        transcript.contains("Implemented bundled-payment"),
        "{transcript}"
    );

    // Gone from sight is not gone: the archive still has it, and
    // listing the user's own messages is one call away.
    let entries = ilar::recall::session_entries(&store, &session_id).unwrap();
    let asked = ilar::recall::by_speaker(&entries, ilar::recall::Speaker::User, 4_000);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].text.contains("pull/396"), "{asked:?}");
}

#[tokio::test]
async fn the_plan_is_state_not_conversation() {
    let (store, session_id) = temp_session();
    seed_compactable_history(&store, &session_id);
    {
        let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
        session
            .append(SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "todo-1".into(),
                    name: "todo".into(),
                    input: serde_json::json!({}),
                    item_id: None,
                }],
                usage: Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "todo-1".into(),
                content: "[x] read the config\n[>] fix the parser\n[ ] add a test".into(),
                is_error: false,
                images: Vec::new(),
                child_session_id: None,
                state: Some(ilar::session::SessionState::TodoList {
                    list: ilar::todo::TodoList {
                        items: vec![
                            ilar::todo::TodoItem {
                                content: "read the config".into(),
                                status: ilar::todo::Status::Completed,
                            },
                            ilar::todo::TodoItem {
                                content: "fix the parser".into(),
                                status: ilar::todo::Status::InProgress,
                            },
                            ilar::todo::TodoItem {
                                content: "add a test".into(),
                                status: ilar::todo::Status::Pending,
                            },
                        ],
                    },
                }),
                ts: chrono::Utc::now(),
            })
            .unwrap();
    }
    // A summarizer that writes a decent summary and never mentions the plan.
    let provider = MockProvider::new(vec![text_turn(
        "## Objective\nfix the parser\n## Next Move\n1. keep going",
    )]);
    let cancel = tokio_util::sync::CancellationToken::new();

    compact_session(&provider, &store, &session_id, Some("system"), &[], &cancel)
        .await
        .unwrap();

    // The list is state: it survives compaction untouched...
    let session = store.load(&session_id).unwrap();
    assert_eq!(session.todo_list().unwrap().items.len(), 3);
    // ...while the handover carries only the summary. The model reads
    // the plan back by calling the todo tool with no arguments, which
    // is what it is told to do — see tests/todo.rs.
    let transcript = format!("{:?}", session.transcript());
    assert!(!transcript.contains("[>] fix the parser"), "{transcript}");
    assert!(
        transcript.contains("fix the parser"),
        "the summary is gone too: {transcript}"
    );
}

// ---- compaction and what the model has seen of the workspace ----

fn tool_call_turn(id: &str, name: &str, input: serde_json::Value) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallStarted {
            id: id.into(),
            name: name.into(),
            item_id: None,
        },
        ProviderEvent::ToolCallCompleted {
            id: id.into(),
            name: name.into(),
            input,
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ]
}

fn read_then_edit_script(read_id: &str, edit_id: &str) -> Vec<Vec<ProviderEvent>> {
    vec![
        tool_call_turn(read_id, "read", serde_json::json!({"path": "a.txt"})),
        text_turn("read it"),
        tool_call_turn(
            edit_id,
            "edit",
            serde_json::json!({
                "path": "a.txt",
                "old_string": "alpha",
                "new_string": "beta"
            }),
        ),
        text_turn("edited it"),
    ]
}

fn tool_result(store: &SessionStore, session_id: &str, call_id: &str) -> (String, bool) {
    store
        .load(session_id)
        .unwrap()
        .events()
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } if tool_use_id == call_id => Some((content.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool result for {call_id}"))
}

/// Control for the two tests below: without a compaction in between, the
/// read in one turn still licenses the edit in the next.
#[tokio::test]
async fn a_read_licenses_an_edit_in_a_later_turn() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let (store, session_id) = temp_session();
    let provider = MockProvider::new(read_then_edit_script("read-1", "edit-1"));
    let context = ToolContext::root(dir.path().to_path_buf());

    for prompt in ["look at a.txt", "now change it"] {
        run_turn(
            &provider,
            &ToolRegistry::builtin(),
            &store,
            &session_id,
            prompt,
            &[],
            None,
            LoopConfig::default(),
            loop_event_channel(LOOP_EVENT_CAPACITY).0,
            tokio_util::sync::CancellationToken::new(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
    }

    let (content, is_error) = tool_result(&store, &session_id, "edit-1");
    assert!(!is_error, "{content}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "beta\n"
    );
}

/// The summary truncated the model's memory of what the file said, so
/// the first edit after a compaction has to re-read.
#[tokio::test]
async fn a_compaction_makes_the_next_edit_re_read_the_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let (store, session_id) = temp_session();
    let mut script = read_then_edit_script("read-1", "edit-1");
    // The forced compaction of turn 2 consumes a provider response of
    // its own, ahead of the edit.
    script.insert(2, text_turn("handover: we were editing a.txt"));
    let provider = MockProvider::new(script);
    let context = ToolContext::root(dir.path().to_path_buf());

    for (prompt, config) in [
        ("look at a.txt", LoopConfig::default()),
        (
            "now change it",
            LoopConfig {
                force_compaction: true,
                ..LoopConfig::default()
            },
        ),
    ] {
        run_turn(
            &provider,
            &ToolRegistry::builtin(),
            &store,
            &session_id,
            prompt,
            &[],
            None,
            config,
            loop_event_channel(LOOP_EVENT_CAPACITY).0,
            tokio_util::sync::CancellationToken::new(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
    }

    let (content, is_error) = tool_result(&store, &session_id, "edit-1");
    assert!(is_error, "{content}");
    assert!(
        content.contains("you have not read this file in this session"),
        "{content}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
}

/// Same for a compaction the user asked for between turns: the next turn
/// notices the session was compacted and drops what the model had seen.
///
/// Compacted twice on purpose. A loaded session carries only the events
/// after its replay checkpoint, and publishing that checkpoint drops
/// every compaction but the last — so anything that tries to notice a
/// compaction by *counting* them sees 1 forever and stops clearing after
/// the first one.
#[tokio::test]
async fn a_manual_compaction_makes_the_next_edit_re_read_the_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let (store, session_id) = temp_session();
    let provider = MockProvider::new(vec![
        tool_call_turn("read-1", "read", serde_json::json!({"path": "a.txt"})),
        text_turn("read it"),
        text_turn("handover: we were looking at a.txt"),
        tool_call_turn("read-2", "read", serde_json::json!({"path": "a.txt"})),
        text_turn("read it again"),
        text_turn("handover: we are still looking at a.txt"),
        tool_call_turn(
            "edit-1",
            "edit",
            serde_json::json!({
                "path": "a.txt",
                "old_string": "alpha",
                "new_string": "beta"
            }),
        ),
        text_turn("edited it"),
    ]);
    let context = ToolContext::root(dir.path().to_path_buf());
    let cancel = tokio_util::sync::CancellationToken::new();

    for prompt in ["look at a.txt", "look again"] {
        run_turn(
            &provider,
            &ToolRegistry::builtin(),
            &store,
            &session_id,
            prompt,
            &[],
            None,
            LoopConfig::default(),
            loop_event_channel(LOOP_EVENT_CAPACITY).0,
            cancel.clone(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        compact_session(&provider, &store, &session_id, None, &[], &cancel)
            .await
            .unwrap();
    }

    run_turn(
        &provider,
        &ToolRegistry::builtin(),
        &store,
        &session_id,
        "now change it",
        &[],
        None,
        LoopConfig::default(),
        loop_event_channel(LOOP_EVENT_CAPACITY).0,
        cancel,
        context,
        None,
    )
    .await
    .unwrap();

    let (content, is_error) = tool_result(&store, &session_id, "edit-1");
    assert!(is_error, "{content}");
    assert!(
        content.contains("you have not read this file in this session"),
        "{content}"
    );
}
