//! Memory on the wire: a session that writes its memory mid-way sends
//! the same prompt before and after, and the next session opens with
//! what was written.

use std::sync::Arc;

use ilar::agent::{LOOP_EVENT_CAPACITY, loop_event_channel, run_turn};
use ilar::memory::{CoreFile, MemoryStore, PROMPT_SECTION, SUMMARY_RULE, dir_for};
use ilar::provider::{MockProvider, ProviderEvent, StopReason};
use ilar::runtime::{MemoryOptions, RuntimeOptions, RuntimePlan};
use ilar::session::{ContentBlock, SessionEvent};
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
        memory: Some(MemoryOptions {
            store: store_for(cwd),
            standing_prompt: true,
            opening_index: true,
            recall: true,
        }),
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

fn memory_recalls(store: &ilar::session::SessionStore, id: &str) -> Vec<Vec<String>> {
    store
        .load(id)
        .unwrap()
        .events()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::MemoryRecall { ids, .. } => Some(ids.clone()),
            _ => None,
        })
        .collect()
}

/// The user message's blocks on the last request: the prompt, and the
/// recall block when one was surfaced.
fn last_user_texts(request: &ilar::provider::Request) -> Vec<String> {
    let message = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == ilar::session::Role::User)
        .unwrap();
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

struct Recalling {
    memory: Arc<MemoryStore>,
    registry: ilar::tools::ToolRegistry,
    store: ilar::session::SessionStore,
    id: String,
    provider: MockProvider,
    dir: tempfile::TempDir,
}

impl Recalling {
    fn new(turns: Vec<Vec<ProviderEvent>>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(MemoryStore::new(dir.path().join("memory")));
        let registry = ilar::tools::ToolRegistry::builtin()
            .with_memory(memory.clone())
            .unwrap();
        let store = ilar::session::SessionStore::new(dir.path().join("sessions"));
        let id = ilar::session::new_id();
        store
            .create(ilar::session::SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        Self {
            memory,
            registry,
            store,
            id,
            provider: MockProvider::new(turns),
            dir,
        }
    }

    async fn turn(&self, prompt: &str, config: ilar::agent::LoopConfig) {
        let (tx, _rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
        run_turn(
            &self.provider,
            &self.registry,
            &self.store,
            &self.id,
            prompt,
            &[],
            Some("You are a test."),
            config,
            tx,
            CancellationToken::new(),
            ilar::tools::ToolContext::root(self.dir.path().to_path_buf()),
            None,
        )
        .await
        .unwrap();
    }
}

/// A prompt that shares words with a note gets that note's index line
/// after it, once; every earlier message stays as it was; after a
/// compaction folds the recall away, the note may come back.
#[tokio::test]
async fn a_prompt_surfaces_the_notes_it_matches_once_until_a_compaction() {
    let t = Recalling::new(vec![
        done("hi"),
        done("8443"),
        done("8443, as I said"),
        done("SUMMARY: ports were discussed."),
        done("after the handover"),
        done("8443, again"),
    ]);
    let now = chrono::Utc::now();
    let deploy = t
        .memory
        .note(
            ilar::memory::NoteKind::Decision,
            "Deploy box",
            "the deploy box is tenco.local on port 8443",
            "Behind the house firewall.",
            now - chrono::Duration::days(2),
        )
        .unwrap();
    t.memory
        .note(
            ilar::memory::NoteKind::Preference,
            "Tea",
            "the person likes earl grey",
            "Never coffee after noon.",
            now,
        )
        .unwrap();
    let recall = ilar::memory::RecallConfig::new(t.memory.clone());
    let config = || ilar::agent::LoopConfig {
        recall: Some(recall.clone()),
        ..Default::default()
    };

    t.turn("hello there", config()).await;
    assert!(memory_recalls(&t.store, &t.id).is_empty());

    let question = "which port does the deploy box on tenco.local use?";
    t.turn(question, config()).await;
    assert_eq!(
        memory_recalls(&t.store, &t.id),
        vec![vec![deploy.id.clone()]]
    );
    let requests = t.provider.requests();
    // The earlier message is byte-identical; the new one carries the
    // prompt and then the recall, as one message.
    assert_eq!(requests[1].messages[0], requests[0].messages[0]);
    let texts = last_user_texts(&requests[1]);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert_eq!(texts[0], question);
    assert!(texts[1].starts_with("<memory-recall>\n"), "{}", texts[1]);
    assert!(texts[1].contains(&deploy.id), "{}", texts[1]);
    assert!(
        texts[1].contains("[decision] 2d ago — Deploy box:"),
        "{}",
        texts[1]
    );
    assert!(
        !texts[1].contains("house firewall"),
        "bodies never travel: {}",
        texts[1]
    );
    assert!(texts[1].contains("verify before asserting"), "{}", texts[1]);
    assert!(!texts[1].contains("earl grey"), "{}", texts[1]);

    // Asked again: the model has it already.
    t.turn(question, config()).await;
    assert_eq!(memory_recalls(&t.store, &t.id).len(), 1);
    assert_eq!(last_user_texts(&t.provider.requests()[2]).len(), 1);

    // A compaction folds the recall away with the rest; the note comes
    // back for the next prompt that wants it.
    t.turn(
        "carry on",
        ilar::agent::LoopConfig {
            context_limit: Some(1_000_000),
            force_compaction: true,
            ..config()
        },
    )
    .await;
    t.turn(question, config()).await;
    let recalls = memory_recalls(&t.store, &t.id);
    let shape: Vec<String> = t
        .store
        .load(&t.id)
        .unwrap()
        .events()
        .iter()
        .map(|event| match event {
            SessionEvent::UserMessage { text, .. } => format!("user({text})"),
            SessionEvent::AssistantMessage { .. } => "assistant".into(),
            SessionEvent::Compaction { kept_from, .. } => format!("compaction(from {kept_from})"),
            SessionEvent::MemoryRecall { .. } => "recall".into(),
            other => format!("{other:?}")
                .split_whitespace()
                .next()
                .unwrap()
                .to_string(),
        })
        .collect();
    assert_eq!(recalls.len(), 2, "{recalls:?}; the log: {shape:?}");
    assert_eq!(recalls[1], vec![deploy.id.clone()]);
    let texts = last_user_texts(t.provider.requests().last().unwrap());
    assert_eq!(texts.len(), 2, "{texts:?}");
}

/// After the session's recall budget is spent, no prompt surfaces
/// anything more.
#[tokio::test]
async fn the_session_s_recall_budget_stops_it() {
    let t = Recalling::new(vec![done("one"), done("two")]);
    let now = chrono::Utc::now();
    for (title, summary) in [
        ("Deploy box", "the deploy box is tenco.local on port 8443"),
        ("Tea", "the person likes earl grey with milk"),
    ] {
        t.memory
            .note(ilar::memory::NoteKind::Event, title, summary, summary, now)
            .unwrap();
    }
    let recall = ilar::memory::RecallConfig {
        session_bytes: 64,
        ..ilar::memory::RecallConfig::new(t.memory.clone())
    };
    let config = || ilar::agent::LoopConfig {
        recall: Some(recall.clone()),
        ..Default::default()
    };
    t.turn("which port is the deploy box on?", config()).await;
    assert_eq!(memory_recalls(&t.store, &t.id).len(), 1);
    t.turn("does the person take milk in their earl grey?", config())
        .await;
    assert_eq!(
        memory_recalls(&t.store, &t.id).len(),
        1,
        "the budget was spent"
    );
}
