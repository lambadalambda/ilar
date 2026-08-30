//! The durable outbox: a published notification survives on disk until
//! its delivery is provable from the parent session's own log.

use std::sync::Arc;
use std::time::Duration;

use futures::stream;
use ilar::config::{AgentDefinition, AgentWorkspaceMode, ProjectInstructions};
use ilar::outbox;
use ilar::provider::{
    EventStream, FixedProviderResolver, Provider, ProviderEvent, Request, StopReason,
};
use ilar::session::{SessionEvent, SessionMeta, SessionStore, Usage, new_id};
use ilar::subagent::{Notification, SubagentSpawner};
use ilar::tools::{ToolContext, ToolRegistry};

fn temp_store() -> SessionStore {
    SessionStore::new(std::env::temp_dir().join(format!("ilar-outbox-test-{}", new_id())))
}

fn create_session(store: &SessionStore, parent_id: Option<&str>) -> String {
    let id = new_id();
    store
        .create(SessionMeta {
            session_id: id.clone(),
            parent_id: parent_id.map(str::to_string),
            agent: "build".into(),
            model: "zai/glm-4.7".into(),
            workspace: None,
            cwd: None,
        })
        .unwrap();
    id
}

fn notification(parent_session_id: &str, text: &str) -> Notification {
    Notification {
        parent_session_id: parent_session_id.to_string(),
        description: "bg survey".into(),
        text: text.to_string(),
        is_error: false,
    }
}

fn append_user_message(store: &SessionStore, session_id: &str, text: &str) {
    let mut session = store.acquire_writer(session_id).unwrap().load().unwrap();
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: text.to_string(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        })
        .unwrap();
}

/// The ancestry filter: a recorded entry is pending for the tree it
/// belongs to and invisible — and untouched — for every other root
/// sharing the store.
#[test]
fn a_recorded_notification_is_pending_for_its_own_root_only() {
    let store = temp_store();
    let dir = tempfile::tempdir().unwrap();
    let root = create_session(&store, None);
    let child = create_session(&store, Some(&root));
    let unrelated_root = create_session(&store, None);
    let text = "<task-notification>\nTask \"bg survey\" completed.\n</task-notification>";

    outbox::record(dir.path(), &notification(&child, text));

    let strangers = outbox::pending(&store, dir.path(), &unrelated_root);
    assert!(strangers.is_empty(), "adopted another tree's entry");
    // Not ours, so not ours to compact either.
    assert!(dir.path().join(format!("{child}.jsonl")).exists());

    let ours = outbox::pending(&store, dir.path(), &root);
    assert_eq!(ours.len(), 1);
    assert_eq!(ours[0].parent_session_id, child);
    assert_eq!(ours[0].text, text);
    // Undelivered, so it stays for the next scan too.
    assert_eq!(outbox::pending(&store, dir.path(), &root).len(), 1);
}

/// Delivery is a `UserMessage` in the parent's log that contains the
/// text — contains, not equals, because a delivering turn may prepend
/// queued steers to the prompt it appends. Once delivered, the entry
/// compacts away and the empty file goes with it.
#[test]
fn a_delivered_notification_compacts_away() {
    let store = temp_store();
    let dir = tempfile::tempdir().unwrap();
    let root = create_session(&store, None);
    let text = "<task-notification>\nTask \"bg survey\" completed.\n</task-notification>";
    outbox::record(dir.path(), &notification(&root, text));
    let undelivered = notification(&root, "<task-notification>\nstill waiting\n</task-notification>");
    outbox::record(dir.path(), &undelivered);
    assert_eq!(outbox::pending(&store, dir.path(), &root).len(), 2);

    append_user_message(&store, &root, &format!("a queued steer first\n\n{text}"));

    let left = outbox::pending(&store, dir.path(), &root);
    assert_eq!(left.len(), 1, "the delivered entry still reads as pending");
    assert_eq!(left[0].text, undelivered.text);

    append_user_message(&store, &root, &undelivered.text);
    assert!(outbox::pending(&store, dir.path(), &root).is_empty());
    assert!(
        !dir.path().join(format!("{root}.jsonl")).exists(),
        "an empty outbox file was left behind"
    );
}

/// A retired entry — one whose text was salvaged into a transcript
/// after a terminal delivery failure — stops pending: the salvage was
/// the delivery of last resort, and the next open must not repeat it.
/// Entries that were never retired (a transient failure holds and
/// retries) keep pending exactly as before.
#[test]
fn a_retired_notification_stops_pending_and_the_rest_persist() {
    let store = temp_store();
    let dir = tempfile::tempdir().unwrap();
    let root = create_session(&store, None);
    let doomed = notification(
        &root,
        "<task-notification>\nTask \"bg survey\" failed: its agent is gone\n</task-notification>",
    );
    let transient = notification(
        &root,
        "<task-notification>\nTask \"bg survey\" completed.\n</task-notification>",
    );
    outbox::record(dir.path(), &doomed);
    outbox::record(dir.path(), &transient);

    outbox::retire(dir.path(), &doomed);

    let left = outbox::pending(&store, dir.path(), &root);
    assert_eq!(left.len(), 1, "the retired entry still reads as pending");
    assert_eq!(left[0].text, transient.text);
    // The transient one persists across scans; the retired one stays gone.
    let left = outbox::pending(&store, dir.path(), &root);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].text, transient.text);
}

/// Retiring the last entry sweeps the whole outbox file and its
/// tombstones: nothing is left to announce at the next open.
#[test]
fn retiring_the_last_entry_leaves_no_files_behind() {
    let store = temp_store();
    let dir = tempfile::tempdir().unwrap();
    let root = create_session(&store, None);
    let doomed = notification(
        &root,
        "<task-notification>\nTask \"bg survey\" failed: its agent is gone\n</task-notification>",
    );
    outbox::record(dir.path(), &doomed);

    outbox::retire(dir.path(), &doomed);

    assert!(outbox::pending(&store, dir.path(), &root).is_empty());
    assert!(
        !dir.path().join(format!("{root}.jsonl")).exists(),
        "an empty outbox file was left behind"
    );
    assert!(
        !dir.path().join(format!("{root}.retired")).exists(),
        "a consumed tombstone file was left behind"
    );
}

/// A file for a session that no longer exists can never be delivered
/// and is removed for whoever scans past it.
#[test]
fn a_dead_sessions_file_is_swept() {
    let store = temp_store();
    let dir = tempfile::tempdir().unwrap();
    let root = create_session(&store, None);
    outbox::record(
        dir.path(),
        &notification("no-such-session", "<task-notification>\nlost\n</task-notification>"),
    );

    assert!(outbox::pending(&store, dir.path(), &root).is_empty());
    assert!(!dir.path().join("no-such-session.jsonl").exists());
}

/// Streams one text turn immediately.
#[derive(Clone)]
struct InstantText;

impl Provider for InstantText {
    fn stream(&self, _req: Request) -> anyhow::Result<EventStream> {
        Ok(Box::pin(stream::iter(vec![
            ProviderEvent::TextDelta("outboxed answer".into()),
            ProviderEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ])))
    }
}

/// The publish hook end to end: a background task completing through
/// the real spawner leaves an outbox entry behind its channel send, and
/// the entry disappears from pending once the parent's log shows the
/// text delivered.
#[tokio::test]
async fn a_background_completion_rides_the_outbox_until_delivered() {
    let store = temp_store();
    let dir = tempfile::tempdir().unwrap();
    let root = create_session(&store, None);
    let spawner = Arc::new(
        SubagentSpawner::new(
            Arc::new(FixedProviderResolver::new(Arc::new(InstantText))),
            store.clone(),
            vec![AgentDefinition {
                name: "explore".into(),
                description: "explores".into(),
                model: None,
                prompt: "".into(),
                workspace_mode: AgentWorkspaceMode::ReadOnly,
                tools: None,
            }],
            std::env::temp_dir(),
            0,
            10,
            3,
            ProjectInstructions::Include,
        )
        .with_outbox_dir(dir.path().to_path_buf()),
    );
    let mut notifications = spawner.subscribe();
    let task = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .unwrap()
        .get("task")
        .unwrap();
    let mut ctx = ToolContext::root(std::env::temp_dir()).with_subagents(spawner.clone());
    ctx.session_id = root.clone();

    let output = task
        .run(
            serde_json::json!({
                "description": "outboxed survey",
                "prompt": "find things",
                "subagent_type": "explore",
                "background": true,
            }),
            ctx,
        )
        .await;
    assert!(!output.is_error, "{}", output.content);
    let published = tokio::time::timeout(Duration::from_secs(5), notifications.recv())
        .await
        .expect("notification within timeout")
        .expect("notification present");
    assert!(published.text.contains("outboxed answer"), "{}", published.text);

    // The channel delivered in-process, but nothing reached the
    // parent's log yet: the durable copy still counts as pending.
    let parked = outbox::pending(&store, dir.path(), &root);
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].text, published.text);

    append_user_message(&store, &root, &published.text);
    assert!(outbox::pending(&store, dir.path(), &root).is_empty());

    spawner.shutdown().await;
}

/// The compaction inside `pending` is a read-filter-rewrite, and a
/// publish landing between the read and the rename used to be erased —
/// silent loss of a finished child's only durable record, and strictly
/// worse than the double-delivery the design does admit to. Both sides
/// take the directory lock now, so the interleaving cannot happen; this
/// drives them at each other to say so.
#[test]
fn a_publish_during_compaction_is_not_erased() {
    let store = temp_store();
    let parent = create_session(&store, None);
    let dir = std::env::temp_dir().join(format!("ilar-outbox-race-{}", new_id()));

    // Something to compact away on every pass: without a delivered
    // entry, `pending` has no reason to rewrite anything.
    append_user_message(&store, &parent, "delivered already");
    outbox::record(&dir, &notification(&parent, "delivered already"));

    let published: Vec<String> = (0..200).map(|index| format!("live entry {index}")).collect();
    let writer = {
        let dir = dir.clone();
        let parent = parent.clone();
        let published = published.clone();
        std::thread::spawn(move || {
            for text in &published {
                outbox::record(&dir, &notification(&parent, text));
            }
        })
    };

    let mut adopted: Vec<String> = Vec::new();
    let mut round = 0;
    while !writer.is_finished() {
        // A pass only rewrites when it has something to drop, so every
        // pass gets its own delivered entry: otherwise the writer races
        // one compaction at the start and nothing after it.
        let delivered = format!("delivered in round {round}");
        append_user_message(&store, &parent, &delivered);
        outbox::record(&dir, &notification(&parent, &delivered));
        round += 1;
        adopted.extend(
            outbox::pending(&store, &dir, &parent)
                .into_iter()
                .map(|notification| notification.text),
        );
    }
    writer.join().unwrap();
    adopted.extend(
        outbox::pending(&store, &dir, &parent)
            .into_iter()
            .map(|notification| notification.text),
    );

    for text in &published {
        assert!(
            adopted.contains(text),
            "a publish was erased by a concurrent compaction: {text}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
