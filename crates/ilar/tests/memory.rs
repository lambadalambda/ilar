//! Memory on the wire: a session that writes its memory mid-way sends
//! the same prompt before and after, and the write is on disk for the
//! next session to read.

use std::sync::Arc;

use ilar::agent::{LOOP_EVENT_CAPACITY, LoopConfig, loop_event_channel, run_turn};
use ilar::memory::{CoreFile, MemoryStore, PROMPT_SECTION, SUMMARY_RULE};
use ilar::provider::{MockProvider, ProviderEvent, StopReason};
use ilar::session::{SessionEvent, SessionMeta, SessionStore, new_id};
use ilar::tools::{Tool, ToolContext, ToolRegistry};
use tokio_util::sync::CancellationToken;

fn done(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        },
    ]
}

/// The prompt is decided at start and never re-read: a write reaches
/// the next session, and this one's cached prefix stays put.
#[tokio::test]
async fn a_write_mid_session_leaves_this_session_s_prompt_alone() {
    let dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(MemoryStore::new(dir.path().join("memory")));
    let registry = ToolRegistry::builtin().with_memory(memory.clone()).unwrap();
    let store = SessionStore::new(dir.path().join("sessions"));
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
    let provider = MockProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStarted {
                id: "m1".into(),
                name: "memory".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "m1".into(),
                name: "memory".into(),
                input: serde_json::json!({"action": "add", "file": "user", "text": "Likes tea"}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        done("kept"),
        done("hello again"),
    ]);
    // What a session with an empty memory opens with.
    let prompt = format!("You are a test.\n\n{}", *PROMPT_SECTION);
    for text in ["remember I like tea", "hi"] {
        let (tx, _rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
        run_turn(
            &provider,
            &registry,
            &store,
            &id,
            text,
            &[],
            Some(&prompt),
            LoopConfig::default(),
            tx,
            CancellationToken::new(),
            ToolContext::root(dir.path().to_path_buf()),
            None,
        )
        .await
        .unwrap();
    }
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.system_prompt.as_deref(), Some(prompt.as_str()));
    }
    // The write landed, and the tool said so.
    assert_eq!(memory.core(CoreFile::User).unwrap(), "Likes tea\n");
    let session = store.load(&id).unwrap();
    assert!(session.events().iter().any(|event| matches!(
        event,
        SessionEvent::ToolResult { content, is_error: false, .. } if content == "added"
    )));
}

/// One rule, said everywhere a note gets written.
#[test]
fn the_summary_rule_is_in_the_tool_and_the_prompt() {
    let tool = ilar::memory::MemoryTool::new(Arc::new(MemoryStore::new(
        std::env::temp_dir().join("never-written"),
    )));
    assert!(tool.description().contains(SUMMARY_RULE));
    assert!(PROMPT_SECTION.contains(SUMMARY_RULE));
    assert!(PROMPT_SECTION.starts_with("# Remembering\n"));
}
