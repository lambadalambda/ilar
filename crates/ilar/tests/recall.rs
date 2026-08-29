//! Cross-session search: the second front door over the recall walk.
//! The `history` tool searches one session from the inside; this walks
//! every root session for the picker's content search.

use chrono::Utc;
use ilar::recall::{SessionHits, search_sessions};
use ilar::session::{ContentBlock, SessionEvent, SessionMeta, SessionStore, Usage, new_id};

fn temp_store() -> (SessionStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().to_path_buf());
    (store, dir)
}

fn meta(parent_id: Option<&str>) -> SessionMeta {
    SessionMeta {
        session_id: new_id(),
        parent_id: parent_id.map(str::to_string),
        agent: "build".into(),
        model: "zai/glm-4.7".into(),
        workspace: None,
        cwd: None,
    }
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserMessage {
        id: new_id(),
        text: text.into(),
        images: Vec::new(),
        ts: Utc::now(),
    }
}

fn assistant(text: &str) -> SessionEvent {
    SessionEvent::AssistantMessage {
        id: new_id(),
        model: "zai/glm-4.7".into(),
        content: vec![ContentBlock::Text { text: text.into() }],
        usage: Usage::default(),
        stop_reason: "end_turn".into(),
        ts: Utc::now(),
    }
}

fn topic(text: &str) -> SessionEvent {
    SessionEvent::Topic {
        id: new_id(),
        text: text.into(),
        ts: Utc::now(),
    }
}

/// Create a root session holding `events`, returning its id.
fn session_with(store: &SessionStore, events: Vec<SessionEvent>) -> String {
    let meta = meta(None);
    let id = meta.session_id.clone();
    let mut session = store.create(meta).unwrap();
    for event in events {
        session.append(event).unwrap();
    }
    id
}

fn collect_all(store: &SessionStore, query: &str, per_session: usize) -> Vec<SessionHits> {
    let mut all = Vec::new();
    search_sessions(store, query, per_session, |_, hits| {
        all.push(hits);
        true
    });
    all
}

#[test]
fn a_phrase_from_the_middle_finds_its_session() {
    let (store, _dir) = temp_store();
    session_with(&store, vec![user("fix the login page")]);
    let wanted = session_with(
        &store,
        vec![
            user("look at the firmware"),
            topic("GM1 firmware dig"),
            assistant("offset 0x4f11b4 holds the AES table"),
        ],
    );

    let found = collect_all(&store, "aes table", 5);

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].session_id, wanted);
    // Identified by topic, not opening message.
    assert_eq!(found[0].title.as_deref(), Some("GM1 firmware dig"));
    assert!(found[0].hits[0].excerpt.contains("AES table"), "{found:?}");

    // The entries handed to the callback are enough to build a preview
    // around the hit — its neighbours, not just the matched line.
    let mut context = Vec::new();
    ilar::recall::search_sessions(&store, "aes table", 5, |entries, hits| {
        context = ilar::recall::around(entries, hits.hits[0].event, 2, 400);
        true
    });
    let rendered = format!("{context:?}");
    assert!(rendered.contains("look at the firmware"), "{rendered}");
    assert!(rendered.contains("0x4f11b4"), "{rendered}");
}

#[test]
fn sessions_without_a_topic_fall_back_to_their_opening() {
    let (store, _dir) = temp_store();
    session_with(
        &store,
        vec![user("debug the payment flow"), assistant("found the race")],
    );

    let found = collect_all(&store, "the race", 5);

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].title.as_deref(), Some("debug the payment flow"));
}

#[test]
fn one_session_cannot_flood_the_list() {
    let (store, _dir) = temp_store();
    let events = (0..20)
        .map(|index| assistant(&format!("needle appearance {index}")))
        .collect::<Vec<_>>();
    session_with(&store, events);

    let found = collect_all(&store, "needle", 3);

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].hits.len(), 3, "per-session cap ignored");
}

#[test]
fn emissions_follow_listing_order_and_stop_on_demand() {
    let (store, _dir) = temp_store();
    for index in 0..3 {
        session_with(&store, vec![user(&format!("shared needle {index}"))]);
    }

    // The walk visits sessions in listing order (newest first) and
    // stops the moment the caller loses interest — a new keystroke.
    let listing: Vec<String> = store.list().into_iter().map(|s| s.id).collect();
    let mut seen = Vec::new();
    search_sessions(&store, "shared needle", 5, |_, hits| {
        seen.push(hits.session_id.clone());
        false
    });
    assert_eq!(seen.len(), 1, "kept walking after the caller stopped");
    assert_eq!(seen[0], listing[0]);

    let all = collect_all(&store, "shared needle", 5);
    let ids: Vec<&str> = all.iter().map(|hits| hits.session_id.as_str()).collect();
    assert_eq!(ids, listing.iter().map(String::as_str).collect::<Vec<_>>());
}

#[test]
fn subagent_children_stay_hidden() {
    let (store, _dir) = temp_store();
    let parent = session_with(&store, vec![user("spawn a helper")]);
    let mut child = store.create(meta(Some(&parent))).unwrap();
    child.append(user("child-only secret phrase")).unwrap();

    let found = collect_all(&store, "child-only secret", 5);

    assert!(
        found.is_empty(),
        "matched inside a child session: {found:?}"
    );
}

#[test]
fn an_empty_query_emits_nothing() {
    let (store, _dir) = temp_store();
    session_with(&store, vec![user("anything at all")]);
    assert!(collect_all(&store, "   ", 5).is_empty());
}

#[test]
fn hits_carry_their_session_modification_time() {
    let (store, _dir) = temp_store();
    session_with(&store, vec![user("needle here")]);
    let listed = store.list();

    let found = collect_all(&store, "needle", 5);
    assert_eq!(found[0].modified, listed[0].modified);
}
