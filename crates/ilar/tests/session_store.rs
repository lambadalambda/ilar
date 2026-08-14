use chrono::Utc;
use std::io::Write;

use ilar::session::{
    ChatMessage, ContentBlock, Role, SessionEvent, SessionMeta, SessionStore, Usage, new_id,
};

fn temp_store() -> (SessionStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().to_path_buf());
    (store, dir)
}

fn sample_meta() -> SessionMeta {
    SessionMeta {
        session_id: new_id(),
        parent_id: None,
        agent: "build".into(),
        model: "zai/glm-4.7".into(),
    }
}

fn sample_log(meta: &SessionMeta) -> Vec<SessionEvent> {
    let ts = Utc::now();
    vec![
        SessionEvent::Meta {
            meta: meta.clone(),
            ts,
        },
        SessionEvent::UserMessage {
            id: new_id(),
            text: "read the config file".into(),
            ts,
        },
        SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content: vec![
                ContentBlock::Text {
                    text: "Reading it now.".into(),
                },
                ContentBlock::ToolCall {
                    id: "toolu_1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "ilar.toml"}),
                },
            ],
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Default::default()
            },
            stop_reason: "tool_use".into(),
            ts,
        },
        SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: "toolu_1".into(),
            content: "1: model = ...".into(),
            is_error: false,
            ts,
        },
        SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content: vec![ContentBlock::Text {
                text: "Done.".into(),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts,
        },
    ]
}

#[test]
fn round_trip_append_load() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();

    for event in sample_log(&meta).into_iter().skip(1) {
        session.append(event).unwrap();
    }

    let reloaded = store.load(&meta.session_id).unwrap();
    assert_eq!(session.events(), reloaded.events());
    assert_eq!(reloaded.meta(), Some(&meta));
}

#[test]
fn corrupt_trailing_line_tolerated() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    for event in sample_log(&meta).into_iter().skip(1) {
        session.append(event).unwrap();
    }

    // Simulate a torn write: corrupt last line + a partial line.
    let path = store.session_path(&meta.session_id);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(f, "{{not json at all").unwrap();
    writeln!(f, "{{\"type\":\"user_message\"").unwrap();

    let reloaded = store.load(&meta.session_id).unwrap();
    // All pre-corruption events survived; corrupt lines skipped.
    assert_eq!(reloaded.events().len(), sample_log(&meta).len());
}

#[test]
fn missing_session_is_error() {
    let (store, _dir) = temp_store();
    assert!(store.load("no-such-id").is_err());
}

#[test]
fn transcript_groups_tool_results_into_user_message() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    for event in sample_log(&meta).into_iter().skip(1) {
        session.append(event).unwrap();
    }

    let transcript = session.transcript();
    assert_eq!(transcript.len(), 4);
    assert_eq!(
        transcript[0],
        ChatMessage::user_text("read the config file")
    );
    assert_eq!(transcript[1].role, Role::Assistant);
    assert_eq!(transcript[1].content.len(), 2);
    // Tool results become one user message with a tool_result block.
    assert_eq!(transcript[2].role, Role::User);
    assert!(matches!(
        &transcript[2].content[0],
        ContentBlock::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "toolu_1"
    ));
    assert_eq!(transcript[3].role, Role::Assistant);
}

#[test]
fn transcript_honors_compaction_boundary() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    let mut events = sample_log(&meta);
    // User message again so we have something after the boundary.
    events.push(SessionEvent::UserMessage {
        id: new_id(),
        text: "and now?".into(),
        ts: Utc::now(),
    });
    // Compact away everything up to (excluding) the final user message.
    let kept_from = events.len() - 1;
    events.push(SessionEvent::Compaction {
        id: new_id(),
        summary: "Earlier: user asked to read config; assistant read it.".into(),
        kept_from,
        ts: Utc::now(),
    });
    for event in events.into_iter().skip(1) {
        session.append(event).unwrap();
    }

    let transcript = session.transcript();
    // Summary coalesced with the kept user message (alternation preserved).
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].role, Role::User);
    assert_eq!(transcript[0].content.len(), 2);
    assert!(matches!(&transcript[0].content[0],
        ContentBlock::Text { text } if text.contains("Earlier: user asked")));
    assert!(matches!(&transcript[0].content[1],
        ContentBlock::Text { text } if text == "and now?"));
}

#[test]
fn transcript_matches_after_reload_with_corruption_and_compaction() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    let mut events = sample_log(&meta);
    events.push(SessionEvent::UserMessage {
        id: new_id(),
        text: "continue".into(),
        ts: Utc::now(),
    });
    let kept_from = events.len();
    events.push(SessionEvent::Compaction {
        id: new_id(),
        summary: "sum".into(),
        kept_from,
        ts: Utc::now(),
    });
    events.push(SessionEvent::UserMessage {
        id: new_id(),
        text: "after compaction".into(),
        ts: Utc::now(),
    });
    for event in events.into_iter().skip(1) {
        session.append(event).unwrap();
    }
    // Corrupt one line (the first assistant message) to force a skip.
    let path = store.session_path(&meta.session_id);
    let lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    let mut rewritten = lines.clone();
    rewritten[2] = "{\"type\":\"assistant_message\"".into(); // was assistant #1
    std::fs::write(&path, rewritten.join("\n") + "\n").unwrap();

    let reloaded = store.load(&meta.session_id).unwrap();
    let transcript = reloaded.transcript();
    // Alternation invariant holds on the resume path.
    for pair in transcript.windows(2) {
        assert_ne!(pair[0].role, pair[1].role, "adjacent same-role messages");
    }
    let last = transcript.last().unwrap();
    // Summary block coalesced with the post-compaction user text.
    assert_eq!(last.role, Role::User);
    assert!(matches!(&last.content[0],
        ContentBlock::Text { text } if text.contains("sum")));
    assert!(matches!(&last.content[1],
        ContentBlock::Text { text } if text == "after compaction"));
}

#[test]
fn child_session_references_parent() {
    let (store, _dir) = temp_store();
    let parent_meta = sample_meta();
    store.create(parent_meta.clone()).unwrap();
    let child_meta = SessionMeta {
        session_id: new_id(),
        parent_id: Some(parent_meta.session_id.clone()),
        agent: "explore".into(),
        model: "zai/glm-4.7-air".into(),
    };
    store.create(child_meta.clone()).unwrap();

    let child = store.load(&child_meta.session_id).unwrap();
    assert_eq!(
        child.meta().unwrap().parent_id,
        Some(parent_meta.session_id)
    );
}
