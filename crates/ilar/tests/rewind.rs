use std::path::Path;
use std::process::Command;

use chrono::Utc;
use ilar::checkpoint;
use ilar::rewind::rewind_session;
use ilar::session::{ContentBlock, SessionEvent, SessionMeta, SessionStore, Usage, new_id};

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn repository() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.name", "ilar tests"]);
    git(&root, &["config", "user.email", "ilar@example.invalid"]);
    std::fs::write(root.join("code.txt"), "v1\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    (temp, root)
}

fn temp_store() -> (SessionStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().to_path_buf());
    (store, dir)
}

fn create_session(store: &SessionStore) -> String {
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
    id
}

/// Append one checkpointed turn: snapshot the repo, then user + reply.
async fn turn(store: &SessionStore, id: &str, root: &Path, text: &str, checkpointed: bool) {
    let mut session = store.acquire_writer(id).unwrap().load().unwrap();
    let ts = Utc::now();
    if checkpointed {
        let snapshot = checkpoint::snapshot(root, id).await.unwrap().unwrap();
        session
            .append(SessionEvent::Checkpoint {
                id: new_id(),
                commit: snapshot.commit,
                head: snapshot.head,
                ts,
            })
            .unwrap();
    }
    session
        .append(SessionEvent::UserMessage {
            id: new_id(),
            text: text.into(),
            images: Vec::new(),
            ts,
        })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content: vec![ContentBlock::Text {
                text: format!("did {text}"),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            ts,
        })
        .unwrap();
}

fn user_target(store: &SessionStore, id: &str, text: &str) -> (usize, String) {
    store
        .load(id)
        .unwrap()
        .events()
        .iter()
        .enumerate()
        .find_map(|(index, event)| match event {
            SessionEvent::UserMessage { id, text: t, .. } if t == text => Some((index, id.clone())),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn rewind_restores_conversation_and_tree_together() {
    let (store, _sessions) = temp_store();
    let (_temp, root) = repository();
    let id = create_session(&store);

    turn(&store, &id, &root, "first", true).await;
    std::fs::write(root.join("code.txt"), "v2\n").unwrap();
    std::fs::write(root.join("scratch.txt"), "temp\n").unwrap();
    turn(&store, &id, &root, "second", true).await;
    std::fs::write(root.join("code.txt"), "v3\n").unwrap();

    let (cut, target) = user_target(&store, &id, "second");
    let report = rewind_session(&store, &id, cut, &target, &root)
        .await
        .unwrap();

    assert_eq!(report.unsent, "second");
    assert!(report.tree_restored);
    assert!(!report.head_moved);
    // Tree is back to the state "second" started from.
    assert_eq!(
        std::fs::read_to_string(root.join("code.txt")).unwrap(),
        "v2\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("scratch.txt")).unwrap(),
        "temp\n"
    );
    // Conversation is back to just the first turn.
    let reader = store.load(&id).unwrap();
    assert!(!reader.events().iter().any(|event| matches!(
        event,
        SessionEvent::UserMessage { text, .. } if text == "second"
    )));
    // The rewind marker records both commits.
    assert!(store.audit_events(&id).unwrap().iter().any(|event| {
        matches!(
            event,
            SessionEvent::Rewind {
                tree_restored: Some(_),
                tree_saved: Some(_),
                ..
            }
        )
    }));
    // The safety snapshot is reachable from the session ref chain.
    let tip = git(
        &root,
        &["rev-parse", &format!("refs/ilar/checkpoints/{id}")],
    );
    let audit = store.audit_events(&id).unwrap();
    let saved = audit
        .iter()
        .find_map(|event| match event {
            SessionEvent::Rewind {
                tree_saved: Some(saved),
                ..
            } => Some(saved.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(tip, saved);
}

#[tokio::test]
async fn turns_without_a_checkpoint_rewind_conversation_only() {
    let (store, _sessions) = temp_store();
    let (_temp, root) = repository();
    let id = create_session(&store);

    turn(&store, &id, &root, "first", false).await;
    std::fs::write(root.join("code.txt"), "v2\n").unwrap();
    turn(&store, &id, &root, "second", false).await;

    let (cut, target) = user_target(&store, &id, "second");
    let report = rewind_session(&store, &id, cut, &target, &root)
        .await
        .unwrap();

    assert_eq!(report.unsent, "second");
    assert!(!report.tree_restored);
    // The tree was left alone.
    assert_eq!(
        std::fs::read_to_string(root.join("code.txt")).unwrap(),
        "v2\n"
    );
}

#[tokio::test]
async fn a_moved_head_is_reported_but_files_still_restore() {
    let (store, _sessions) = temp_store();
    let (_temp, root) = repository();
    let id = create_session(&store);

    turn(&store, &id, &root, "first", true).await;
    std::fs::write(root.join("code.txt"), "v2\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "user committed meanwhile"]);
    let head_after_commit = git(&root, &["rev-parse", "HEAD"]);

    let (cut, target) = user_target(&store, &id, "first");
    let report = rewind_session(&store, &id, cut, &target, &root)
        .await
        .unwrap();

    assert!(report.tree_restored);
    assert!(report.head_moved);
    assert_eq!(
        std::fs::read_to_string(root.join("code.txt")).unwrap(),
        "v1\n"
    );
    // HEAD itself is never moved by a rewind.
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head_after_commit);
}

#[tokio::test]
async fn a_non_user_target_is_rejected_before_any_git_work() {
    let (store, _sessions) = temp_store();
    let (_temp, root) = repository();
    let id = create_session(&store);
    turn(&store, &id, &root, "first", true).await;
    std::fs::write(root.join("code.txt"), "v2\n").unwrap();

    let error = rewind_session(&store, &id, 0, "no-such-event", &root)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not a user message"));
    // Nothing changed: no marker appended, tree untouched.
    assert!(
        !store
            .audit_events(&id)
            .unwrap()
            .iter()
            .any(|event| matches!(event, SessionEvent::Rewind { .. }))
    );
    assert_eq!(
        std::fs::read_to_string(root.join("code.txt")).unwrap(),
        "v2\n"
    );
}

#[tokio::test]
async fn an_active_writer_rejects_the_rewind_before_any_git_work() {
    let (store, _sessions) = temp_store();
    let (_temp, root) = repository();
    let id = create_session(&store);
    turn(&store, &id, &root, "first", true).await;
    std::fs::write(root.join("code.txt"), "v2\n").unwrap();
    turn(&store, &id, &root, "second", true).await;
    std::fs::write(root.join("code.txt"), "v3\n").unwrap();

    let (cut, target) = user_target(&store, &id, "second");
    let _active = store.acquire_writer(&id).unwrap();
    let error = rewind_session(&store, &id, cut, &target, &root)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already active"), "{error}");
    // The tree was not touched: the lease is taken before any git work.
    assert_eq!(
        std::fs::read_to_string(root.join("code.txt")).unwrap(),
        "v3\n"
    );
    assert!(
        !store
            .audit_events(&id)
            .unwrap()
            .iter()
            .any(|event| matches!(event, SessionEvent::Rewind { .. }))
    );
}

#[tokio::test]
async fn a_stale_target_id_rejects_the_rewind() {
    let (store, _sessions) = temp_store();
    let (_temp, root) = repository();
    let id = create_session(&store);
    turn(&store, &id, &root, "first", true).await;
    turn(&store, &id, &root, "second", true).await;

    let (cut, _) = user_target(&store, &id, "second");
    let error = rewind_session(&store, &id, cut, "some-other-event", &root)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("changed since"), "{error}");
}
