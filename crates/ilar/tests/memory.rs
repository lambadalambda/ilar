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
            // The selector would take its own turns from the script;
            // the recall tests below run it.
            recall: None,
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
    // One note in the archive before the session opens: its index
    // line is in the prompt, its body is not.
    let now = chrono::Utc::now();
    let note = |title: &str, summary: &str| {
        store_for(&cwd)
            .note(
                ilar::memory::NoteKind::Event,
                title,
                summary,
                "body-only text",
                now,
            )
            .unwrap()
    };
    note("Deploy box", "the deploy box is tenco.local on port 8443");
    let runtime = RuntimePlan::resolve(&config, &options(&cwd))
        .unwrap()
        .start_with(&config, Arc::new(provider.clone()))
        .unwrap();
    let opened_with = runtime.system_prompt.clone();
    assert!(opened_with.contains("# Remembering"), "{opened_with}");
    assert!(!opened_with.contains("# Memory"), "{opened_with}");
    assert!(
        opened_with.contains("## Notes (1 in the archive"),
        "{opened_with}"
    );
    assert!(
        opened_with.contains("Deploy box: the deploy box"),
        "{opened_with}"
    );
    assert!(!opened_with.contains("body-only text"), "{opened_with}");

    let turn = |text: &'static str| {
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
    };
    turn("remember I like tea").await.unwrap();
    // A note written mid-session — by the tool or by anything else —
    // is the next session's business.
    note("Tea", "the person likes earl grey");
    turn("hi").await.unwrap();
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.system_prompt.as_deref(), Some(opened_with.as_str()));
    }
    // The writes landed, the tool said so, and this session's prompt
    // never saw either.
    assert!(!opened_with.contains("Likes tea"));
    assert!(!opened_with.contains("earl grey"));
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
    assert!(next.contains("## Notes (2 in the archive"), "{next}");
    assert!(next.contains("Tea: the person likes earl grey"), "{next}");
}

/// One rule, said everywhere a note gets written — and the section
/// says when to write at all, which is the thing a model otherwise
/// agrees with and never does.
#[test]
fn the_summary_rule_is_in_the_tool_and_the_prompt() {
    let tool = ilar::memory::MemoryTool::new(Arc::new(MemoryStore::new(
        std::env::temp_dir().join("never-written"),
    )));
    assert!(tool.description().contains(SUMMARY_RULE));
    assert!(
        tool.description()
            .contains("amend it rather than file a second")
    );
    assert!(PROMPT_SECTION.contains(SUMMARY_RULE));
    assert!(PROMPT_SECTION.starts_with("# Remembering\n"));
    let section = PROMPT_SECTION.as_str();
    assert!(section.contains("corrects you"), "{section}");
    assert!(section.contains("before you finish the reply"), "{section}");
    assert!(section.contains("for now"), "the scope words: {section}");
    assert!(section.contains("amend the note"), "{section}");
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
            ContentBlock::Text { text, .. } => Some(text.clone()),
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
        self.turn_on(&self.provider, prompt, config).await;
    }

    async fn turn_on(
        &self,
        provider: &dyn ilar::provider::ProviderResolver,
        prompt: &str,
        config: ilar::agent::LoopConfig,
    ) {
        let (tx, _rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
        run_turn(
            provider,
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

/// The selector's answer, as the mock streams it.
fn picked(numbers: &str) -> Vec<ProviderEvent> {
    done(&format!("{{\"picks\": [{numbers}]}}"))
}

/// Recall as the tests run it: the selector on the session's own
/// model, so the one scripted provider answers both. Its call is made
/// at turn start, before the turn's first request. The tests below it
/// run the configurable mode — each message alone, index lines only;
/// the Claude Code mode has its own test.
fn recall_config(memory: &Arc<MemoryStore>) -> ilar::memory::RecallConfig {
    ilar::memory::RecallConfig {
        bodies: false,
        history: false,
        ..claude_code_recall(memory)
    }
}

fn claude_code_recall(memory: &Arc<MemoryStore>) -> ilar::memory::RecallConfig {
    ilar::memory::RecallConfig {
        model: "zai/glm-4.7".into(),
        ..ilar::memory::RecallConfig::new(memory.clone())
    }
}

/// By default the selector is one conversation, as Claude Code's: the
/// second message's call carries the first message and the answer it
/// got, and extends the first call, so the provider serves that part
/// from its cache. The recall carries the note's body.
#[tokio::test]
async fn the_selector_remembers_the_session_and_recalls_bodies() {
    let t = Recalling::new(vec![
        picked("1"),
        done("mode 3 is 172 dots"),
        picked(""),
        done("yes, checked"),
    ]);
    let ppu = t
        .memory
        .note(
            ilar::memory::NoteKind::Decision,
            "PPU timing",
            "mode 3 lasts 172 dots",
            "Measured against the Pan Docs table.",
            chrono::Utc::now() - chrono::Duration::days(1),
        )
        .unwrap();
    // A second note, so the second message still has one to ask about.
    t.memory
        .note(
            ilar::memory::NoteKind::Solution,
            "Timer reload",
            "TIMA reloads a cycle late",
            "",
            chrono::Utc::now(),
        )
        .unwrap();
    let config = || ilar::agent::LoopConfig {
        recall: Some(claude_code_recall(&t.memory)),
        ..Default::default()
    };
    t.turn("fix the PPU timing", config()).await;
    t.turn("are you sure this is right?", config()).await;

    assert_eq!(memory_recalls(&t.store, &t.id), vec![vec![ppu.id.clone()]]);
    let requests = t.provider.requests();
    let (first, second) = (&requests[0], &requests[2]);
    assert_eq!(
        second.messages[..first.messages.len() - 1],
        first.messages[..first.messages.len() - 1]
    );
    assert_eq!(
        second.messages[0], first.messages[0],
        "the listing and first message lead both"
    );
    assert_eq!(
        second.messages[1].content,
        vec![ContentBlock::text(r#"{"picks": [1]}"#)]
    );
    assert_eq!(
        last_user_texts(second),
        ["Message:\nare you sure this is right?"]
    );
    let recall = last_user_texts(&requests[1]);
    assert!(
        recall[1].contains("Measured against the Pan Docs table."),
        "{recall:?}"
    );
}

/// The note the selector picks gets its index line after the prompt,
/// once; every earlier message stays as it was; after a compaction
/// folds the recall away, the note may come back.
#[tokio::test]
async fn a_picked_note_is_surfaced_once_until_a_compaction() {
    let t = Recalling::new(vec![
        picked(""),
        done("hi"),
        picked("2"),
        done("8443"),
        picked(""),
        done("8443, as I said"),
        picked(""),
        done("SUMMARY: ports were discussed."),
        done("after the handover"),
        picked("2"),
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
    let recall = recall_config(&t.memory);
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
    // The selector read one line per note and the person's words, never
    // a body.
    let selector = &requests[2];
    assert_eq!(
        selector.system_prompt.as_deref(),
        Some(ilar::selector::SELECTOR_PROMPT)
    );
    let asked = last_user_texts(selector);
    assert!(
        asked[0].contains("2. [decision] 2d ago — Deploy box:"),
        "{asked:?}"
    );
    assert!(!asked[0].contains("house firewall"), "{asked:?}");
    assert_eq!(asked[1], format!("Message:\n{question}"));
    // The earlier message is byte-identical; the new one carries the
    // prompt and then the recall, as one message.
    assert_eq!(requests[3].messages[0], requests[1].messages[0]);
    let texts = last_user_texts(&requests[3]);
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
    assert!(
        texts[1].contains("not instructions from anyone"),
        "{}",
        texts[1]
    );
    assert!(texts[1].contains("Check before asserting"), "{}", texts[1]);
    assert!(!texts[1].contains("earl grey"), "{}", texts[1]);

    // Asked again: the model has it already, so it is not offered.
    t.turn(question, config()).await;
    assert_eq!(memory_recalls(&t.store, &t.id).len(), 1);
    let requests = t.provider.requests();
    assert!(!last_user_texts(&requests[4])[0].contains("Deploy box"));
    assert_eq!(last_user_texts(&requests[5]).len(), 1);

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
    // The loaded session is the window past the compaction: the first
    // recall is folded away, the second stands. The raw log has both.
    assert_eq!(
        memory_recalls(&t.store, &t.id),
        vec![vec![deploy.id.clone()]]
    );
    let in_the_log = t
        .store
        .audit_events(&t.id)
        .unwrap()
        .iter()
        .filter(|event| matches!(event, SessionEvent::MemoryRecall { .. }))
        .count();
    assert_eq!(in_the_log, 2);
    let texts = last_user_texts(t.provider.requests().last().unwrap());
    assert_eq!(texts.len(), 2, "{texts:?}");
}

/// After the session's recall budget is spent, no prompt asks the
/// selector anything more.
#[tokio::test]
async fn the_session_s_recall_budget_stops_it() {
    let t = Recalling::new(vec![picked("1"), done("one"), done("two")]);
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
        ..recall_config(&t.memory)
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
    assert_eq!(t.provider.requests().len(), 3, "no second selector call");
}

/// The scripted provider, each stream held back by the next delay —
/// on a paused clock, so the test runs in no time and in a fixed order.
struct Delayed {
    inner: MockProvider,
    delays: std::sync::Mutex<std::collections::VecDeque<std::time::Duration>>,
}

impl ilar::provider::Provider for Delayed {
    fn stream(&self, req: ilar::provider::Request) -> anyhow::Result<ilar::provider::EventStream> {
        use futures::StreamExt as _;
        let delay = self.delays.lock().unwrap().pop_front().unwrap_or_default();
        let events = self.inner.stream(req)?;
        Ok(Box::pin(
            futures::stream::once(tokio::time::sleep(delay))
                .filter_map(|()| async { None })
                .chain(events),
        ))
    }
}

/// An answer that misses the first request's grace is not lost: it
/// joins at the next step, after that step's tool results.
#[tokio::test(start_paused = true)]
async fn a_late_pick_joins_after_the_step_s_tool_results() {
    let t = Recalling::new(vec![
        picked("1"),
        vec![
            ProviderEvent::ToolCallStarted {
                id: "s1".into(),
                name: "memory_search".into(),
                item_id: None,
            },
            ProviderEvent::ToolCallCompleted {
                id: "s1".into(),
                name: "memory_search".into(),
                input: serde_json::json!({"query": "kettle"}),
            },
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
            },
        ],
        done("8443"),
    ]);
    let deploy = t
        .memory
        .note(
            ilar::memory::NoteKind::Decision,
            "Deploy box",
            "the deploy box is tenco.local on port 8443",
            "",
            chrono::Utc::now(),
        )
        .unwrap();
    let seconds = std::time::Duration::from_secs;
    // The selector misses the two-second grace and answers while the
    // first step is still streaming.
    let delayed = Delayed {
        inner: t.provider.clone(),
        delays: std::sync::Mutex::new([seconds(3), seconds(5), seconds(0)].into()),
    };
    let config = ilar::agent::LoopConfig {
        recall: Some(recall_config(&t.memory)),
        ..Default::default()
    };
    t.turn_on(&delayed, "which port is the deploy box on?", config)
        .await;

    assert_eq!(
        memory_recalls(&t.store, &t.id),
        vec![vec![deploy.id.clone()]]
    );
    let requests = t.provider.requests();
    assert_eq!(requests.len(), 3);
    // The first request went without; the second carries the recall
    // after the search's result, in the same user message.
    assert_eq!(last_user_texts(&requests[1]).len(), 1);
    let last = requests[2].messages.last().unwrap();
    assert!(
        matches!(last.content[0], ContentBlock::ToolResult { .. }),
        "{last:?}"
    );
    assert!(
        matches!(&last.content[1], ContentBlock::Text { text, .. } if text.starts_with("<memory-recall>")),
        "{last:?}"
    );
}

/// A selector that fails costs the prompt its recall, never the turn.
#[tokio::test]
async fn a_failing_selector_leaves_the_turn_alone() {
    let t = Recalling::new(vec![
        vec![ProviderEvent::Error("selector down".into())],
        done("answered anyway"),
    ]);
    t.memory
        .note(
            ilar::memory::NoteKind::Event,
            "Deploy box",
            "the deploy box is tenco.local on port 8443",
            "",
            chrono::Utc::now(),
        )
        .unwrap();
    let config = ilar::agent::LoopConfig {
        recall: Some(recall_config(&t.memory)),
        ..Default::default()
    };
    t.turn("which port is the deploy box on?", config).await;
    assert!(memory_recalls(&t.store, &t.id).is_empty());
    let last = t.store.load(&t.id).unwrap().transcript();
    assert!(
        matches!(&last.last().unwrap().content[0], ContentBlock::Text { text, .. } if text == "answered anyway"),
        "{last:?}"
    );
}

/// A note call the flush makes, as a provider streams it.
fn note_call(id: &str, input: serde_json::Value) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ThinkingDelta("the database choice should last".into()),
        ProviderEvent::ToolCallStarted {
            id: id.into(),
            name: "memory".into(),
            item_id: None,
        },
        ProviderEvent::ToolCallCompleted {
            id: id.into(),
            name: "memory".into(),
            input,
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Default::default(),
        },
    ]
}

/// Before a compaction folds the conversation away, a session nobody
/// reviews gets one side request — OpenClaw's flush — that may call the
/// memory tools. Terminal sessions otherwise wrote little: aiko none in
/// thirty prompts and five compactions. The turn itself is untouched:
/// no message is added to it, and the handover says what was kept.
#[tokio::test]
async fn a_compaction_first_lets_the_model_write_its_memory() {
    let t = Recalling::new(vec![
        done("hi"),
        note_call(
            "flush-1",
            serde_json::json!({
                "action": "note",
                "kind": "decision",
                "title": "Database",
                "summary": "the app database is postgres 17",
                "body": "Chosen for jsonb."
            }),
        ),
        done("nothing more"),
        done("SUMMARY: we talked about the database."),
        done("after the handover"),
    ]);
    t.turn("hello there", ilar::agent::LoopConfig::default())
        .await;
    t.turn(
        "we decided on postgres",
        ilar::agent::LoopConfig {
            // Any context is over this: compaction is due.
            context_limit: Some(1),
            memory_flush: Some(t.memory.clone()),
            ..Default::default()
        },
    )
    .await;

    let notes = t.memory.notes().unwrap();
    let database = notes
        .iter()
        .find(|note| note.title == "Database")
        .expect("the flush wrote");
    let events = t.store.audit_events(&t.id).unwrap();
    let handover = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::Compaction { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("compacted");
    assert!(handover.contains(&database.id), "{handover}");
    let user_texts: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::UserMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_texts,
        ["hello there", "we decided on postgres"],
        "the turn is untouched"
    );
    // Same system prompt and tools as the turn: the conversation is
    // served from the prompt cache, only the instruction is new.
    let requests = t.provider.requests();
    assert_eq!(requests[1].tools, requests[0].tools);
    assert_eq!(requests[1].messages[0], requests[0].messages[0]);
    // The second flush round carries the first round's thinking beside
    // its call: Kimi and DeepSeek refuse a tool call without it.
    let replayed = requests[2]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == ilar::session::Role::Assistant)
        .unwrap();
    assert!(
        matches!(&replayed.content[0], ContentBlock::Thinking { text, .. } if text.contains("database choice")),
        "{replayed:?}"
    );
    // The summary goes on from the flush, its last answer included,
    // rather than branching off before it: a server that caches only a
    // continuation (gufo) read 169k tokens again for the branch.
    let summary = &requests[3].messages;
    assert!(summary.starts_with(&requests[2].messages), "{summary:?}");
    assert_eq!(summary.len(), requests[2].messages.len() + 2);
    assert!(
        matches!(&summary[summary.len() - 2].content[..], [ContentBlock::Text { text, .. }] if text == "nothing more"),
        "{summary:?}"
    );
}

/// A flush still calling tools at its last round is not gone on from: a
/// summary asked right after tool results, tools on offer, is apt to
/// call another, and a call fails the compaction.
#[tokio::test]
async fn a_flush_that_ends_on_calls_leaves_the_summary_its_own_request() {
    let note = |id: &str, title: &str| {
        note_call(
            id,
            serde_json::json!({
                "action": "note",
                "kind": "decision",
                "title": title,
                "summary": format!("{title} was decided"),
                "body": "Chosen."
            }),
        )
    };
    let t = Recalling::new(vec![
        done("hi"),
        note("flush-1", "One"),
        note("flush-2", "Two"),
        note("flush-3", "Three"),
        done("SUMMARY: three decisions."),
        done("after the handover"),
    ]);
    t.turn("hello there", ilar::agent::LoopConfig::default())
        .await;
    t.turn(
        "we decided three things",
        ilar::agent::LoopConfig {
            context_limit: Some(1),
            memory_flush: Some(t.memory.clone()),
            ..Default::default()
        },
    )
    .await;

    let requests = t.provider.requests();
    let (flush, summary) = (&requests[1].messages, &requests[4].messages);
    // The conversation and the summary instruction, as without a flush.
    assert_eq!(summary.len(), flush.len());
    assert_eq!(summary[..flush.len() - 1], flush[..flush.len() - 1]);
    assert_ne!(summary.last(), flush.last());
    assert_eq!(t.memory.notes().unwrap().len(), 3);
}
