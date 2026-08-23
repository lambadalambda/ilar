use chrono::Utc;
use std::io::Write;

use ilar::question::{Question, QuestionKind, QuestionRequest};
use ilar::session::{
    ChatMessage, ContentBlock, Role, SessionEvent, SessionId, SessionMeta, SessionState,
    SessionStore, Usage, new_id,
};
use ilar::todo::{Status as TodoStatus, TodoItem, TodoList};

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
        workspace: None,
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
                    item_id: None,
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
            child_session_id: None,
            state: None,
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
fn todo_snapshots_round_trip_and_latest_replacement_wins() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let id = meta.session_id.clone();
    let mut session = store.create(meta).unwrap();
    let ts = Utc::now();
    session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content: vec![
                ContentBlock::ToolCall {
                    id: "todo-1".into(),
                    name: "todo".into(),
                    input: serde_json::json!({}),
                    item_id: None,
                },
                ContentBlock::ToolCall {
                    id: "todo-2".into(),
                    name: "todo".into(),
                    input: serde_json::json!({}),
                    item_id: None,
                },
            ],
            usage: Usage::default(),
            stop_reason: "tool_use".into(),
            ts,
        })
        .unwrap();
    let first = TodoList {
        items: vec![TodoItem {
            content: "first".into(),
            status: TodoStatus::InProgress,
        }],
    };
    let latest = TodoList {
        items: vec![TodoItem {
            content: "second".into(),
            status: TodoStatus::Completed,
        }],
    };
    for (tool_use_id, list) in [("todo-1", first), ("todo-2", latest.clone())] {
        session
            .append(SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: tool_use_id.into(),
                content: "updated".into(),
                is_error: false,
                child_session_id: None,
                state: Some(SessionState::TodoList { list }),
                ts,
            })
            .unwrap();
    }

    assert_eq!(session.todo_list(), Some(&latest));
    assert_eq!(session.transcript().len(), 2);
    drop(session);
    let resumed = store.load(&id).unwrap();
    assert_eq!(resumed.todo_list(), Some(&latest));
}

#[test]
fn legacy_tool_results_without_state_still_deserialize() {
    let event: SessionEvent = serde_json::from_value(serde_json::json!({
        "type": "tool_result",
        "id": new_id(),
        "tool_use_id": "legacy-call",
        "content": "done",
        "is_error": false,
        "ts": Utc::now(),
    }))
    .unwrap();
    assert!(matches!(
        event,
        SessionEvent::ToolResult { state: None, .. }
    ));
}

#[test]
fn replay_rejects_todo_state_from_a_non_todo_tool() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let id = meta.session_id.clone();
    let mut session = store.create(meta).unwrap();
    session
        .append(assistant_with_calls(&new_id(), &["read-call"]))
        .unwrap();
    session
        .append(SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: "read-call".into(),
            content: "done".into(),
            is_error: false,
            child_session_id: None,
            state: Some(SessionState::TodoList {
                list: TodoList::default(),
            }),
            ts: Utc::now(),
        })
        .unwrap();
    drop(session);

    let error = store
        .load(&id)
        .err()
        .expect("invalid state must fail replay");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("non-todo"), "{error}");
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
                item_id: None,
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
fn fallback_window_clamps_compaction_boundary_to_its_event() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "old".into(),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "clamped summary".into(),
            kept_from: usize::MAX,
            ts: Utc::now(),
        })
        .unwrap();
    drop(session);

    let loaded = store.load(&meta.session_id).unwrap();
    let transcript = loaded.transcript();

    assert!(
        loaded
            .events()
            .iter()
            .any(|event| matches!(event, SessionEvent::Compaction { .. }))
    );
    assert!(format!("{transcript:?}").contains("clamped summary"));
}

#[test]
fn compacted_session_loads_only_the_active_replay_window() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    for index in 0..200 {
        session
            .append(SessionEvent::UserMessage {
                id: new_id(),
                text: format!("old question {index}"),
                ts: Utc::now(),
            })
            .unwrap();
        session
            .append(SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ContentBlock::Text {
                    text: format!("old answer {index}"),
                }],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
                ts: Utc::now(),
            })
            .unwrap();
    }
    let kept_from = session.events().len();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "active question".into(),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "old conversation".into(),
            kept_from,
            ts: Utc::now(),
        })
        .unwrap();
    let expected = session.transcript();
    drop(session);

    let indexed = store.load(&meta.session_id).unwrap();

    assert!(store.replay_index_path(&meta.session_id).unwrap().exists());
    assert!(indexed.events().len() <= 3, "{:?}", indexed.events());
    assert_eq!(indexed.transcript(), expected);
    assert_eq!(store.audit_events(&meta.session_id).unwrap().len(), 403);
}

#[test]
fn corrupt_replay_index_falls_back_to_identical_canonical_replay() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    for event in sample_log(&meta).into_iter().skip(1) {
        session.append(event).unwrap();
    }
    let kept_from = session.events().len();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "active".into(),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "summary".into(),
            kept_from,
            ts: Utc::now(),
        })
        .unwrap();
    let expected_transcript = session.transcript();
    let canonical_event_count = session.events().len();
    drop(session);
    std::fs::write(
        store.replay_index_path(&meta.session_id).unwrap(),
        b"not an index",
    )
    .unwrap();

    let fallback = store.load(&meta.session_id).unwrap();

    assert!(fallback.events().len() <= 3, "{:?}", fallback.events());
    assert_eq!(
        store.audit_events(&meta.session_id).unwrap().len(),
        canonical_event_count
    );
    assert_eq!(fallback.transcript(), expected_transcript);
}

#[test]
fn writer_rebuilds_a_corrupt_replay_index_after_canonical_fallback() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    for event in sample_log(&meta).into_iter().skip(1) {
        session.append(event).unwrap();
    }
    let kept_from = session.events().len();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "active".into(),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "summary".into(),
            kept_from,
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: meta.model.clone(),
            content: vec![ContentBlock::Text {
                text: "post-compaction answer".into(),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts: Utc::now(),
        })
        .unwrap();
    drop(session);
    std::fs::write(
        store.replay_index_path(&meta.session_id).unwrap(),
        b"corrupt",
    )
    .unwrap();

    drop(
        store
            .acquire_writer(&meta.session_id)
            .unwrap()
            .load()
            .unwrap(),
    );
    let indexed = store.load(&meta.session_id).unwrap();

    assert!(indexed.events().len() <= 4, "{:?}", indexed.events());
    assert!(format!("{:?}", indexed.transcript()).contains("post-compaction answer"));
}

#[test]
fn replay_index_preserves_folded_model_and_todo_state() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    session
        .append(SessionEvent::ModelChange {
            id: new_id(),
            model: "openai/gpt-5.1".into(),
            variant: Some("high".into()),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: "openai/gpt-5.1".into(),
            content: vec![ContentBlock::ToolCall {
                id: "todo-before-cut".into(),
                name: "todo".into(),
                input: serde_json::json!({}),
                item_id: None,
            }],
            usage: Usage::default(),
            stop_reason: "tool_use".into(),
            ts: Utc::now(),
        })
        .unwrap();
    let expected_todos = TodoList {
        items: vec![TodoItem {
            content: "survive compaction".into(),
            status: TodoStatus::InProgress,
        }],
    };
    session
        .append(SessionEvent::ToolResult {
            id: new_id(),
            tool_use_id: "todo-before-cut".into(),
            content: "updated".into(),
            is_error: false,
            child_session_id: None,
            state: Some(SessionState::TodoList {
                list: expected_todos.clone(),
            }),
            ts: Utc::now(),
        })
        .unwrap();
    let kept_from = session.events().len();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "active".into(),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "folded state".into(),
            kept_from,
            ts: Utc::now(),
        })
        .unwrap();
    drop(session);

    let indexed = store.load(&meta.session_id).unwrap();

    assert_eq!(indexed.effective_model(), "openai/gpt-5.1");
    assert_eq!(indexed.effective_variant(), Some("high".into()));
    assert_eq!(indexed.todo_list(), Some(&expected_todos));
}

#[test]
fn compaction_after_indexed_load_writes_an_absolute_boundary() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    for index in 0..20 {
        session
            .append(SessionEvent::UserMessage {
                id: new_id(),
                text: format!("old {index}"),
                ts: Utc::now(),
            })
            .unwrap();
        session
            .append(SessionEvent::AssistantMessage {
                id: new_id(),
                model: meta.model.clone(),
                content: vec![ContentBlock::Text {
                    text: format!("answer {index}"),
                }],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
                ts: Utc::now(),
            })
            .unwrap();
    }
    let first_cut = session.events().len();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "first active".into(),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "first summary".into(),
            kept_from: first_cut,
            ts: Utc::now(),
        })
        .unwrap();
    drop(session);

    let mut indexed = store
        .acquire_writer(&meta.session_id)
        .unwrap()
        .load()
        .unwrap();
    indexed
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: meta.model.clone(),
            content: vec![ContentBlock::Text {
                text: "first answer".into(),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts: Utc::now(),
        })
        .unwrap();
    let second_cut = indexed.events().len();
    indexed
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "second active".into(),
            ts: Utc::now(),
        })
        .unwrap();
    indexed
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "second summary".into(),
            kept_from: second_cut,
            ts: Utc::now(),
        })
        .unwrap();
    let indexed_transcript = indexed.transcript();
    drop(indexed);

    let canonical = std::fs::read_to_string(store.session_path(&meta.session_id).unwrap()).unwrap();
    let final_event: SessionEvent =
        serde_json::from_str(canonical.lines().last().unwrap()).unwrap();
    assert!(matches!(
        final_event,
        SessionEvent::Compaction { kept_from, .. } if kept_from > second_cut
    ));
    std::fs::remove_file(store.replay_index_path(&meta.session_id).unwrap()).unwrap();
    assert_eq!(
        store.load(&meta.session_id).unwrap().transcript(),
        indexed_transcript
    );
}

#[test]
fn active_writer_rejects_atomic_replacement_of_canonical_log() {
    let (store, dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    let path = store.session_path(&meta.session_id).unwrap();
    let replacement = dir.path().join("replacement.jsonl");
    std::fs::write(&replacement, std::fs::read(&path).unwrap()).unwrap();
    std::fs::rename(&replacement, &path).unwrap();

    let error = session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "must not disappear".into(),
            ts: Utc::now(),
        })
        .expect_err("replaced canonical path must invalidate the writer");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        !std::fs::read_to_string(path)
            .unwrap()
            .contains("must not disappear")
    );
}

#[test]
fn invalid_history_is_never_hidden_by_checkpoint_creation() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: "duplicate-event".into(),
            text: "first".into(),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: meta.model.clone(),
            content: vec![ContentBlock::Text {
                text: "answer".into(),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: "duplicate-event".into(),
            text: "duplicate".into(),
            ts: Utc::now(),
        })
        .unwrap();
    let kept_from = session.events().len() - 1;
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "must not hide invalid history".into(),
            kept_from,
            ts: Utc::now(),
        })
        .unwrap();
    drop(session);

    assert!(!store.replay_index_path(&meta.session_id).unwrap().exists());
    let error = store
        .load(&meta.session_id)
        .err()
        .expect("canonical duplicate must remain visible");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn corrupted_historical_id_page_falls_back_to_canonical_validation() {
    let (store, dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: "historical-id".into(),
            text: "old".into(),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: meta.model.clone(),
            content: vec![ContentBlock::Text { text: "old".into() }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts: Utc::now(),
        })
        .unwrap();
    let kept_from = session.events().len();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "active".into(),
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "old".into(),
            kept_from,
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: "historical-id".into(),
            text: "duplicate tail".into(),
            ts: Utc::now(),
        })
        .unwrap();
    drop(session);
    let checkpoint: serde_json::Value = serde_json::from_slice(
        &std::fs::read(store.replay_index_path(&meta.session_id).unwrap()).unwrap(),
    )
    .unwrap();
    let generation = checkpoint["generation"].as_str().unwrap();
    let ids_path = dir
        .path()
        .join(format!("{}.replay.{generation}.ids", meta.session_id));
    let mut ids = std::fs::read(&ids_path).unwrap();
    *ids.last_mut().unwrap() ^= 0xff;
    std::fs::write(ids_path, ids).unwrap();

    let error = store
        .load(&meta.session_id)
        .err()
        .expect("corrupt id index must not hide canonical duplicates");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
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

fn question_request() -> QuestionRequest {
    QuestionRequest {
        questions: vec![Question {
            id: "choice".into(),
            prompt: "Choose one".into(),
            description: None,
            required: true,
            kind: QuestionKind::FreeText,
        }],
    }
}

fn assistant_with_question(call_id: &str, input: serde_json::Value) -> SessionEvent {
    SessionEvent::AssistantMessage {
        id: new_id(),
        model: "zai/glm-4.7".into(),
        content: vec![ContentBlock::ToolCall {
            id: call_id.into(),
            name: "question".into(),
            input,
            item_id: None,
        }],
        usage: Usage::default(),
        stop_reason: "tool_use".into(),
        ts: Utc::now(),
    }
}

#[test]
fn sole_valid_pending_question_survives_writer_load_and_is_typed_for_readers() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let request = question_request();
    let mut session = store.create(meta.clone()).unwrap();
    session
        .append(assistant_with_question(
            "pending-question",
            serde_json::to_value(&request).unwrap(),
        ))
        .unwrap();
    drop(session);

    let resumed = store
        .acquire_writer(&meta.session_id)
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(resumed.events().len(), 2);
    drop(resumed);

    let reader = store.load(&meta.session_id).unwrap();
    let pending = reader.pending_question().expect("pending question");
    assert_eq!(pending.tool_call_id, "pending-question");
    assert_eq!(pending.request, request);
}

#[test]
fn malformed_or_invalid_pending_question_is_repaired_normally() {
    for input in [
        serde_json::json!({"questions": "not-an-array"}),
        serde_json::json!({"questions": []}),
    ] {
        let (store, _dir) = temp_store();
        let meta = sample_meta();
        let mut session = store.create(meta.clone()).unwrap();
        session
            .append(assistant_with_question("bad-question", input))
            .unwrap();
        drop(session);

        let reader = store.load(&meta.session_id).unwrap();
        assert!(reader.pending_question().is_none());
        assert_eq!(reader.events().len(), 2);

        let resumed = store
            .acquire_writer(&meta.session_id)
            .unwrap()
            .load()
            .unwrap();
        assert!(matches!(
            resumed.events().last(),
            Some(SessionEvent::ToolResult {
                tool_use_id,
                is_error: true,
                ..
            }) if tool_use_id == "bad-question"
        ));
        drop(resumed);
        assert!(
            store
                .load(&meta.session_id)
                .unwrap()
                .pending_question()
                .is_none()
        );
    }
}

#[test]
fn multiple_or_mixed_pending_calls_are_repaired_normally() {
    for names in [["question", "question"], ["question", "read"]] {
        let (store, _dir) = temp_store();
        let meta = sample_meta();
        let request = serde_json::to_value(question_request()).unwrap();
        let mut session = store.create(meta.clone()).unwrap();
        session
            .append(SessionEvent::AssistantMessage {
                id: new_id(),
                model: meta.model.clone(),
                content: names
                    .into_iter()
                    .enumerate()
                    .map(|(index, name)| ContentBlock::ToolCall {
                        id: format!("call-{index}"),
                        name: name.into(),
                        input: request.clone(),
                        item_id: None,
                    })
                    .collect(),
                usage: Usage::default(),
                stop_reason: "tool_use".into(),
                ts: Utc::now(),
            })
            .unwrap();
        drop(session);

        let reader = store.load(&meta.session_id).unwrap();
        assert!(reader.pending_question().is_none());
        assert_eq!(reader.events().len(), 2);

        let resumed = store
            .acquire_writer(&meta.session_id)
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(resumed.events().len(), 4);
        drop(resumed);
        assert!(
            store
                .load(&meta.session_id)
                .unwrap()
                .pending_question()
                .is_none()
        );
    }
}

#[test]
fn pending_question_is_restored_from_active_checkpoint_replay() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let request = question_request();
    let mut session = store.create(meta.clone()).unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "old".into(),
            ts: Utc::now(),
        })
        .unwrap();
    let kept_from = session.events().len();
    session
        .append(SessionEvent::Compaction {
            id: new_id(),
            summary: "old context".into(),
            kept_from,
            ts: Utc::now(),
        })
        .unwrap();
    session
        .append(assistant_with_question(
            "checkpoint-question",
            serde_json::to_value(&request).unwrap(),
        ))
        .unwrap();
    drop(session);

    assert!(store.replay_index_path(&meta.session_id).unwrap().exists());
    let reader = store.load(&meta.session_id).unwrap();
    let pending = reader.pending_question().expect("checkpoint question");
    assert_eq!(pending.tool_call_id, "checkpoint-question");
    assert_eq!(pending.request, request);

    drop(
        store
            .acquire_writer(&meta.session_id)
            .unwrap()
            .load()
            .unwrap(),
    );
    assert!(
        store
            .load(&meta.session_id)
            .unwrap()
            .pending_question()
            .is_some()
    );
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
            child_session_id: None,
            state: None,
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
            child_session_id: None,
            state: None,
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
        workspace: None,
    };
    store.create(child_meta.clone()).unwrap();

    let child = store.load(&child_meta.session_id).unwrap();
    assert_eq!(
        child.meta().unwrap().parent_id,
        Some(parent_meta.session_id)
    );
}

#[test]
fn older_metadata_without_workspace_still_deserializes() {
    let (store, _dir) = temp_store();
    let id = new_id();
    let path = store.session_path(&id).unwrap();
    std::fs::write(
        &path,
        format!(
            "{{\"type\":\"meta\",\"session_id\":\"{id}\",\"agent\":\"build\",\"model\":\"zai/glm-4.7\",\"ts\":\"2026-08-15T00:00:00Z\"}}\n"
        ),
    )
    .unwrap();

    let session = store.load(&id).unwrap();

    assert_eq!(session.meta().unwrap().workspace, None);
}

// ---- session listing ----

#[test]
fn list_returns_root_sessions_most_recent_first_with_titles() {
    let (store, dir) = temp_store();
    let meta_a = sample_meta();
    let mut a = store.create(meta_a.clone()).unwrap();
    a.append(SessionEvent::UserMessage {
        id: new_id(),
        text: "  first   question\nacross lines  ".into(),
        ts: Utc::now(),
    })
    .unwrap();
    drop(a);

    let meta_b = sample_meta();
    let mut b = store.create(meta_b.clone()).unwrap();
    b.append(SessionEvent::UserMessage {
        id: new_id(),
        text: "second".into(),
        ts: Utc::now(),
    })
    .unwrap();
    drop(b);

    // Explicit mtimes: robust against coarse filesystem timestamp granularity.
    let now = std::time::SystemTime::now();
    let set_mtime = |id: &str, when: std::time::SystemTime| {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(store.session_path(id).unwrap())
            .unwrap();
        file.set_modified(when).unwrap();
    };
    set_mtime(
        &meta_a.session_id,
        now - std::time::Duration::from_secs(100),
    );
    set_mtime(&meta_b.session_id, now);

    let mut child_meta = sample_meta();
    child_meta.parent_id = Some(meta_a.session_id.clone());
    drop(store.create(child_meta.clone()).unwrap());

    std::fs::write(dir.path().join("corrupt.jsonl"), "not json\n").unwrap();
    std::fs::write(dir.path().join("empty.jsonl"), "").unwrap();

    let sessions = store.list();
    let ids: Vec<_> = sessions.iter().map(|session| session.id.as_str()).collect();
    assert_eq!(
        ids,
        [meta_b.session_id.as_str(), meta_a.session_id.as_str()]
    );
    assert_eq!(sessions[0].title.as_deref(), Some("second"));
    assert_eq!(
        sessions[1].title.as_deref(),
        Some("first question across lines")
    );
    assert_eq!(
        store.latest().map(|session| session.id),
        Some(meta_b.session_id.clone())
    );
}

#[test]
fn list_titles_are_bounded_and_optional() {
    let (store, _dir) = temp_store();
    let long_meta = sample_meta();
    let mut long = store.create(long_meta.clone()).unwrap();
    long.append(SessionEvent::UserMessage {
        id: new_id(),
        text: "x".repeat(500),
        ts: Utc::now(),
    })
    .unwrap();
    drop(long);
    let untitled_meta = sample_meta();
    drop(store.create(untitled_meta.clone()).unwrap());

    let sessions = store.list();
    assert_eq!(sessions.len(), 2);
    for session in &sessions {
        if session.id == long_meta.session_id {
            let title = session.title.as_deref().unwrap();
            assert!(title.chars().count() <= 81, "{}", title.chars().count());
            assert!(title.ends_with('…'), "{title}");
        } else {
            assert_eq!(session.id, untitled_meta.session_id);
            assert_eq!(session.title, None);
        }
    }
}

#[test]
fn list_of_missing_root_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("nonexistent"));
    assert!(store.list().is_empty());
    assert!(store.latest().is_none());
}

#[test]
fn delete_removes_session_files_and_refuses_active_sessions() {
    let (store, dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "hello".into(),
            ts: Utc::now(),
        })
        .unwrap();

    // Active (writer held): refused.
    let error = store.delete(&meta.session_id).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    drop(session);

    store.delete(&meta.session_id).unwrap();
    assert!(store.load(&meta.session_id).is_err());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(&meta.session_id))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn fork_copies_history_under_a_new_id() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let mut session = store.create(meta.clone()).unwrap();
    for event in sample_log(&meta).into_iter().skip(1) {
        session.append(event).unwrap();
    }
    drop(session);

    let fork_id = store.fork(&meta.session_id).unwrap();
    assert_ne!(fork_id, meta.session_id);
    let original = store.load(&meta.session_id).unwrap();
    let fork = store.load(&fork_id).unwrap();
    assert_eq!(fork.meta().unwrap().session_id, fork_id);
    assert_eq!(fork.events().len(), original.events().len());
    assert_eq!(fork.transcript().len(), original.transcript().len());
    // The fork is independently writable.
    let mut fork_session = store.acquire_writer(&fork_id).unwrap().load().unwrap();
    fork_session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "diverge".into(),
            ts: Utc::now(),
        })
        .unwrap();
    drop(fork_session);
    assert_eq!(
        store.load(&fork_id).unwrap().events().len(),
        original.events().len() + 1
    );
}

#[test]
fn checkpoint_events_round_trip_and_render_nothing() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let id = meta.session_id.clone();
    let mut session = store.create(meta).unwrap();
    let ts = Utc::now();
    session
        .append(SessionEvent::Checkpoint {
            id: new_id(),
            commit: "abc123".into(),
            head: Some("def456".into()),
            ts,
        })
        .unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: "rewrite the parser".into(),
            ts,
        })
        .unwrap();
    drop(session);

    let reader = store.load(&id).unwrap();
    assert!(reader.events().iter().any(|event| matches!(
        event,
        SessionEvent::Checkpoint { commit, head: Some(head), .. }
            if commit == "abc123" && head == "def456"
    )));
    let transcript = reader.transcript();
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].role, Role::User);
}

#[test]
fn checkpoint_without_head_omits_the_field_on_the_wire() {
    let event = SessionEvent::Checkpoint {
        id: "cp-1".into(),
        commit: "abc123".into(),
        head: None,
        ts: Utc::now(),
    };
    let line = serde_json::to_string(&event).unwrap();
    assert!(line.contains("\"type\":\"checkpoint\""), "{line}");
    assert!(!line.contains("head"), "{line}");
    assert_eq!(serde_json::from_str::<SessionEvent>(&line).unwrap(), event);
}

#[test]
fn checkpoint_between_call_and_result_is_rejected() {
    let (store, _dir) = temp_store();
    let meta = sample_meta();
    let id = meta.session_id.clone();
    let mut session = store.create(meta).unwrap();
    session
        .append(assistant_with_calls("assistant-1", &["call-1"]))
        .unwrap();
    session
        .append(SessionEvent::Checkpoint {
            id: new_id(),
            commit: "abc123".into(),
            head: None,
            ts: Utc::now(),
        })
        .unwrap();
    drop(session);

    assert_replay_invalid(&store, &id);
}
