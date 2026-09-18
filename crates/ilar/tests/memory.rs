//! Memory on the wire: a session that writes its memory mid-way sends
//! the same prompt before and after, and the next session opens with
//! what was written.

use std::sync::Arc;

use ilar::agent::{LOOP_EVENT_CAPACITY, loop_event_channel, run_turn};
use ilar::memory::{CoreFile, MemoryStore, PROMPT_SECTION, SUMMARY_RULE, dir_for};
use ilar::provider::{MockProvider, ProviderEvent, StopReason};
use ilar::runtime::{RuntimeOptions, RuntimePlan};
use ilar::session::SessionEvent;
use ilar::tools::Tool;
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

/// The prompt is decided at start and never re-read: every request of
/// a session that writes its memory mid-way carries the prompt the
/// session opened with, and the next session from the same directory
/// opens with the write in it.
#[tokio::test]
async fn a_write_mid_session_reaches_the_next_session_s_prompt_and_not_this_one_s() {
    let guard = tempfile::tempdir().unwrap();
    let cwd = guard.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let config = ilar::config::Loader::with_env(vec![("ILAR_ZAI_API_KEY", "zk".to_string())])
        .config_dir(guard.path().join("config"))
        .state_dir(guard.path().join("state"))
        .resolve()
        .unwrap();
    let store_for =
        |cwd: &std::path::Path| Arc::new(MemoryStore::new(dir_for(config.state_dir(), cwd)));
    let options = |cwd: &std::path::Path| RuntimeOptions {
        cwd: cwd.to_path_buf(),
        memory: Some(store_for(cwd)),
        memory_prompt: true,
        ..RuntimeOptions::default()
    };
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
    let runtime = RuntimePlan::resolve(&config, &options(&cwd))
        .unwrap()
        .start_with(&config, Arc::new(provider.clone()))
        .unwrap();
    let opened_with = runtime.system_prompt.clone();
    assert!(opened_with.contains("# Remembering"), "{opened_with}");
    assert!(!opened_with.contains("# Memory"), "{opened_with}");

    for text in ["remember I like tea", "hi"] {
        let (tx, _rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
        run_turn(
            &provider,
            &runtime.registry,
            &runtime.store,
            &runtime.session_id,
            text,
            &[],
            Some(&runtime.system_prompt),
            runtime.loop_config.clone(),
            tx,
            CancellationToken::new(),
            runtime.tool_ctx.clone(),
            None,
        )
        .await
        .unwrap();
    }
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.system_prompt.as_deref(), Some(opened_with.as_str()));
    }
    // The write landed, the tool said so, and this session's prompt
    // never saw it.
    assert!(!opened_with.contains("Likes tea"));
    assert_eq!(store_for(&cwd).core(CoreFile::User).unwrap(), "Likes tea\n");
    let session = runtime.store.load(&runtime.session_id).unwrap();
    assert!(session.events().iter().any(|event| matches!(
        event,
        SessionEvent::ToolResult { content, is_error: false, .. } if content == "added"
    )));

    // The next session from the same directory, spelled another way,
    // opens with it.
    let respelled = cwd.join(".").join("..").join("project");
    let next = RuntimePlan::resolve(&config, &options(&respelled))
        .unwrap()
        .preview(&config)
        .unwrap()
        .system_prompt;
    assert!(next.contains("# Remembering"), "{next}");
    assert!(
        next.contains("# Memory") && next.contains("Likes tea"),
        "{next}"
    );
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
