use chrono::Utc;
use std::io::Write;

use ilar::session::{
    ChatMessage, ContentBlock, Role, SessionEvent, SessionId, SessionMeta, SessionStore, Usage,
    new_id,
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

fn assistant_with_calls(event_id: &str, call_ids: &[&str]) -> SessionEvent {
    SessionEvent::AssistantMessage {
        id: event_id.into(),
        model: "zai/glm-4.7".into(),
        content: call_ids
            .iter()
            .map(|id| ContentBlock::ToolCall {
                id: (*id).into(),
                name: "read".into(),
                input: serde_json::json!({"path": "ilar.toml"}),
            })
            .collect(),
        usage: Usage::default(),
        stop_reason: "tool_use".into(),
        ts: Utc::now(),
    }
}

fn assert_replay_invalid(store: &SessionStore, id: &str) {
    let path = store.session_path(id).unwrap();
    let before = std::fs::read(&path).unwrap();
    let read_error = store
        .load(id)
        .err()
        .expect("reader must reject invalid replay");
    assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
    let write_error = store
        .acquire_writer(id)
        .unwrap()
        .load()
        .err()
        .expect("writer must reject invalid replay");
    assert_eq!(write_error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(std::fs::read(path).unwrap(), before);
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
fn torn_final_record_is_ignored_by_readers_and_repaired_before_append() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    for event in sample_log(&meta).into_iter().skip(1) {
        session.append(event).unwrap();
    }

    drop(session);
    let path = store.session_path(&meta.session_id).unwrap();
    let valid_len = std::fs::metadata(&path).unwrap().len();
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(b"{\"type\":\"user_message\"").unwrap();
    drop(f);

    let reloaded = store.load(&meta.session_id).unwrap();
    assert_eq!(reloaded.events().len(), sample_log(&meta).len());
    assert!(std::fs::metadata(&path).unwrap().len() > valid_len);

    let mut writable = store
        .acquire_writer(&meta.session_id)
        .unwrap()
        .load()
        .unwrap();
    writable
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "after recovery".into(),
            ts: Utc::now(),
        })
        .unwrap();
    drop(writable);

    let bytes = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("{\"type\":\"user_message\"{"));
    assert_eq!(
        store.load(&meta.session_id).unwrap().events().len(),
        sample_log(&meta).len() + 1
    );
}

#[test]
fn valid_final_record_without_newline_is_uncommitted() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    drop(store.create(meta.clone()).unwrap());
    let path = store.session_path(&meta.session_id).unwrap();
    let committed = std::fs::read(&path).unwrap();
    let event = SessionEvent::UserMessage {
        id: new_id(),
        text: "not committed".into(),
        ts: Utc::now(),
    };
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(&serde_json::to_vec(&event).unwrap())
        .unwrap();
    drop(file);

    assert_eq!(store.load(&meta.session_id).unwrap().events().len(), 1);
    drop(
        store
            .acquire_writer(&meta.session_id)
            .unwrap()
            .load()
            .unwrap(),
    );
    assert_eq!(std::fs::read(&path).unwrap(), committed);
    assert_eq!(store.load(&meta.session_id).unwrap().events().len(), 1);
}

#[test]
fn invalid_utf8_final_tail_is_repaired_by_writer() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    drop(store.create(meta.clone()).unwrap());
    let path = store.session_path(&meta.session_id).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(&[0xff, 0xfe]).unwrap();
    drop(file);

    assert_eq!(store.load(&meta.session_id).unwrap().events().len(), 1);
    drop(
        store
            .acquire_writer(&meta.session_id)
            .unwrap()
            .load()
            .unwrap(),
    );
    assert!(std::str::from_utf8(&std::fs::read(path).unwrap()).is_ok());
}

#[test]
fn files_without_committed_events_are_rejected_without_mutation() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    drop(store.create(meta.clone()).unwrap());
    let path = store.session_path(&meta.session_id).unwrap();
    let uncommitted_meta = serde_json::to_vec(&SessionEvent::Meta {
        meta: meta.clone(),
        ts: Utc::now(),
    })
    .unwrap();

    for contents in [Vec::new(), uncommitted_meta, vec![0xff, b'\n']] {
        std::fs::write(&path, &contents).unwrap();
        let read_error = store
            .load(&meta.session_id)
            .err()
            .expect("reader must reject unrecoverable log");
        assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
        let write_error = store
            .acquire_writer(&meta.session_id)
            .unwrap()
            .load()
            .err()
            .expect("writer must reject unrecoverable log");
        assert_eq!(write_error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), contents);
    }
}

#[test]
fn missing_session_is_error() {
    let (store, _dir) = temp_store();
    let error = store.load(&new_id()).err().expect("missing session");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn session_ids_must_be_canonical_uuids() {
    let (store, dir) = temp_store();
    for id in [
        "../escape",
        "nested/id",
        "",
        "not-a-uuid",
        "550E8400-E29B-41D4-A716-446655440000",
        "550e8400e29b41d4a716446655440000",
        "{550e8400-e29b-41d4-a716-446655440000}",
    ] {
        let error = store.load(id).err().expect("invalid load must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "id: {id}");

        let mut meta = sample_meta();
        meta.session_id = id.into();
        let error = store.create(meta).err().expect("invalid create must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "id: {id}");
    }
    assert!(!dir.path().parent().unwrap().join("escape.jsonl").exists());
}

#[test]
fn session_id_is_validated_before_use() {
    let raw = new_id();
    let id = SessionId::parse(&raw).unwrap();
    assert_eq!(id.as_str(), raw);

    for invalid in [
        "../escape",
        "not-a-uuid",
        "550E8400-E29B-41D4-A716-446655440000",
    ] {
        assert!(SessionId::parse(invalid).is_err(), "id: {invalid}");
    }
}

#[test]
fn invalid_create_does_not_create_session_root() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("not-created");
    let store = SessionStore::new(root.clone());
    let mut meta = sample_meta();
    meta.session_id = "../escape".into();

    let error = store.create(meta).err().expect("invalid create must fail");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(!root.exists());
}

#[test]
fn writer_lease_rejects_contention_and_releases_on_drop() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    store.create(meta.clone()).unwrap();

    let first = store.acquire_writer(&meta.session_id).unwrap();
    assert!(
        store.load(&meta.session_id).is_ok(),
        "read-only load must remain available"
    );
    let error = store
        .acquire_writer(&meta.session_id)
        .err()
        .expect("second writer must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(error.to_string().contains("already active"));

    drop(first);
    store.acquire_writer(&meta.session_id).unwrap();
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
fn middle_corruption_with_torn_tail_is_rejected_without_mutating_log() {
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
    drop(session);
    // Corrupt one line (the first assistant message) to force a skip.
    let path = store.session_path(&meta.session_id).unwrap();
    let lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    let mut rewritten = lines.clone();
    rewritten[2] = "{\"type\":\"assistant_message\"".into(); // was assistant #1
    std::fs::write(&path, rewritten.join("\n") + "\nunterminated tail").unwrap();

    let before = std::fs::read(&path).unwrap();
    let error = store
        .load(&meta.session_id)
        .err()
        .expect("middle corruption");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let error = store
        .acquire_writer(&meta.session_id)
        .unwrap()
        .load()
        .err()
        .expect("writer must reject middle corruption");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn unanswered_tool_calls_are_repaired_once_by_writer() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    session
        .append(assistant_with_calls(
            &new_id(),
            &["answered", "interrupted"],
        ))
        .unwrap();
    session
        .append(SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: "answered".into(),
            content: "ok".into(),
            is_error: false,
            ts: Utc::now(),
        })
        .unwrap();
    drop(session);

    assert_eq!(store.load(&meta.session_id).unwrap().events().len(), 3);
    let repaired = store
        .acquire_writer(&meta.session_id)
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(repaired.events().len(), 4);
    assert!(matches!(
        repaired.events().last(),
        Some(SessionEvent::ToolResult {
            tool_use_id,
            is_error: true,
            ..
        }) if tool_use_id == "interrupted"
    ));
    drop(repaired);

    let reloaded = store
        .acquire_writer(&meta.session_id)
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(reloaded.events().len(), 4);
}

#[test]
fn orphan_tool_results_are_rejected_without_mutation() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    session
        .append(SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: "missing-call".into(),
            content: "impossible".into(),
            is_error: false,
            ts: Utc::now(),
        })
        .unwrap();
    drop(session);

    assert_replay_invalid(&store, &meta.session_id);
}

#[test]
fn duplicate_event_and_tool_call_ids_are_rejected() {
    let (event_store, _event_dir) = temp_store();
    let event_meta = sample_meta();
    let mut event_session = event_store.create(event_meta.clone()).unwrap();
    let duplicate_id = new_id();
    for text in ["one", "two"] {
        event_session
            .append(SessionEvent::UserMessage {
                id: duplicate_id.clone(),
                text: text.into(),
                ts: Utc::now(),
            })
            .unwrap();
    }
    drop(event_session);
    assert_replay_invalid(&event_store, &event_meta.session_id);

    let (call_store, _call_dir) = temp_store();
    let call_meta = sample_meta();
    let mut call_session = call_store.create(call_meta.clone()).unwrap();
    call_session
        .append(assistant_with_calls(&new_id(), &["same-call", "same-call"]))
        .unwrap();
    drop(call_session);
    assert_replay_invalid(&call_store, &call_meta.session_id);
}

#[test]
fn metadata_must_be_unique_first_and_match_the_filename() {
    let (duplicate_store, _duplicate_dir) = temp_store();
    let duplicate_meta = sample_meta();
    let mut duplicate_session = duplicate_store.create(duplicate_meta.clone()).unwrap();
    duplicate_session
        .append(SessionEvent::Meta {
            meta: duplicate_meta.clone(),
            ts: Utc::now(),
        })
        .unwrap();
    drop(duplicate_session);
    assert_replay_invalid(&duplicate_store, &duplicate_meta.session_id);

    let (mismatch_store, _mismatch_dir) = temp_store();
    let mismatch_meta = sample_meta();
    drop(mismatch_store.create(mismatch_meta.clone()).unwrap());
    let path = mismatch_store
        .session_path(&mismatch_meta.session_id)
        .unwrap();
    let mut event: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim_end()).unwrap();
    event["session_id"] = new_id().into();
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&event).unwrap()),
    )
    .unwrap();
    assert_replay_invalid(&mismatch_store, &mismatch_meta.session_id);
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
