//! /btw: a question over the session that leaves no trace in it.

use ilar::aside::ask;
use ilar::provider::{MockProvider, ProviderEvent, StopReason};
use ilar::session::{ContentBlock, SessionEvent, SessionMeta, SessionStore, Usage, new_id};
use tokio_util::sync::CancellationToken;

fn temp_session() -> (SessionStore, String) {
    let dir = std::env::temp_dir().join(format!("ilar-aside-test-{}", new_id()));
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

fn seed_conversation(store: &SessionStore, session_id: &str) {
    let mut session = store.acquire_writer(session_id).unwrap().load().unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "wire up the auth service on port 8080".into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content: vec![ContentBlock::Text {
                text: "Done; it listens on 8080 behind the nginx proxy.".into(),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
}

fn text_turn(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]
}

#[tokio::test]
async fn an_aside_is_answered_over_the_conversation_and_never_recorded() {
    let (store, session_id) = temp_session();
    seed_conversation(&store, &session_id);
    let provider = MockProvider::new(vec![text_turn("Port 8080, behind nginx.")]);
    let events_before = store.audit_events(&session_id).unwrap().len();

    let answer = ask(
        &provider,
        &store,
        &session_id,
        Some("system"),
        &[],
        "which port was it again?",
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(answer.as_deref(), Some("Port 8080, behind nginx."));
    // No trace: the session log is byte-for-byte what it was.
    assert_eq!(
        store.audit_events(&session_id).unwrap().len(),
        events_before,
        "an aside wrote to the session"
    );

    // The request is the turn's own shape — untouched transcript, the
    // question appended as the final user message, the session's cache
    // key — so the provider serves the conversation from cache.
    let request = provider.requests().pop().unwrap();
    assert_eq!(request.cache_key.as_deref(), Some(session_id.as_str()));
    let rendered = format!("{:?}", request.messages);
    assert!(rendered.contains("wire up the auth service"), "{rendered}");
    assert!(rendered.contains("nginx proxy"), "{rendered}");
    let last = format!("{:?}", request.messages.last().unwrap());
    assert!(last.contains("which port was it again?"), "{last}");
    assert!(last.contains("aside"), "no aside framing: {last}");
    assert_eq!(request.messages.len(), 3, "{rendered}");
}

#[tokio::test]
async fn an_aside_that_reaches_for_tools_is_an_error() {
    let (store, session_id) = temp_session();
    seed_conversation(&store, &session_id);
    let provider = MockProvider::new(vec![vec![
        ProviderEvent::TextDelta("let me check".into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ]]);

    let error = ask(
        &provider,
        &store,
        &session_id,
        None,
        &[],
        "what does the config say?",
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("tool"), "{error}");
}

#[tokio::test]
async fn an_empty_question_is_refused_before_any_call() {
    let (store, session_id) = temp_session();
    let provider = MockProvider::new(Vec::new());

    let error = ask(
        &provider,
        &store,
        &session_id,
        None,
        &[],
        "   ",
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("question"), "{error}");
    assert!(provider.requests().is_empty(), "a call was made anyway");
}

#[tokio::test]
async fn a_mid_turn_aside_cuts_back_to_the_last_settled_point() {
    let (store, session_id) = temp_session();
    seed_conversation(&store, &session_id);
    // The turn is mid-flight: the log ends with an assistant message
    // whose tool calls have no results yet. Sending that to a provider
    // is a rejected request, not a question.
    {
        let mut session = store.acquire_writer(&session_id).unwrap().load().unwrap();
        session
            .append(SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![
                    ContentBlock::Text {
                        text: "let me check the config".into(),
                    },
                    ContentBlock::ToolCall {
                        id: "dangling-1".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": "ilar.toml"}),
                        item_id: None,
                    },
                ],
                usage: Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
    }
    let provider = MockProvider::new(vec![text_turn("Port 8080.")]);

    let answer = ask(
        &provider,
        &store,
        &session_id,
        None,
        &[],
        "which port again?",
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(answer.as_deref(), Some("Port 8080."));
    let request = provider.requests().pop().unwrap();
    let rendered = format!("{:?}", request.messages);
    assert!(
        !rendered.contains("dangling-1"),
        "unpaired tool call sent to the provider: {rendered}"
    );
    // The settled conversation before it still rides along.
    assert!(rendered.contains("nginx proxy"), "{rendered}");
    let last = format!("{:?}", request.messages.last().unwrap());
    assert!(last.contains("which port again?"), "{last}");
}

#[tokio::test]
async fn cancellation_returns_no_answer() {
    let (store, session_id) = temp_session();
    seed_conversation(&store, &session_id);
    let provider = MockProvider::new(vec![text_turn("too late")]);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let answer = ask(
        &provider,
        &store,
        &session_id,
        None,
        &[],
        "anything?",
        &cancel,
    )
    .await
    .unwrap();

    assert_eq!(answer, None);
}
