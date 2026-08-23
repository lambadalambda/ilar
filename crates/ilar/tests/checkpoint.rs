use std::path::Path;
use std::process::Command;

use ilar::checkpoint;

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn git_fails(cwd: &Path, args: &[&str]) -> bool {
    !Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap()
        .status
        .success()
}

fn repository() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.name", "ilar tests"]);
    git(&root, &["config", "user.email", "ilar@example.invalid"]);
    std::fs::write(root.join("tracked.txt"), "original\n").unwrap();
    std::fs::write(root.join("doomed.txt"), "delete me\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    (temp, root)
}

fn ref_name(session_id: &str) -> String {
    format!("refs/ilar/checkpoints/{session_id}")
}

#[tokio::test]
async fn snapshot_captures_tree_without_touching_repo() {
    let (_temp, root) = repository();
    std::fs::write(root.join("tracked.txt"), "modified\n").unwrap();
    std::fs::write(root.join("fresh.txt"), "untracked\n").unwrap();
    std::fs::write(root.join(".gitignore"), "secret.txt\n").unwrap();
    std::fs::write(root.join("secret.txt"), "ignored\n").unwrap();
    std::fs::remove_file(root.join("doomed.txt")).unwrap();

    let head_before = git_stdout(&root, &["rev-parse", "HEAD"]);
    let status_before = git_stdout(&root, &["status", "--porcelain"]);
    let index_before = git_stdout(&root, &["ls-files", "--stage"]);

    let snapshot = checkpoint::snapshot(&root, "session-a")
        .await
        .unwrap()
        .expect("git repository should produce a snapshot");

    assert_eq!(snapshot.head.as_deref(), Some(head_before.as_str()));
    assert_eq!(git_stdout(&root, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_stdout(&root, &["status", "--porcelain"]), status_before);
    assert_eq!(git_stdout(&root, &["ls-files", "--stage"]), index_before);

    let show = |path: &str| git_stdout(&root, &["show", &format!("{}:{path}", snapshot.commit)]);
    assert_eq!(show("tracked.txt"), "modified");
    assert_eq!(show("fresh.txt"), "untracked");
    assert!(git_fails(
        &root,
        &["cat-file", "-e", &format!("{}:secret.txt", snapshot.commit)]
    ));
    assert!(git_fails(
        &root,
        &["cat-file", "-e", &format!("{}:doomed.txt", snapshot.commit)]
    ));
    assert_eq!(
        git_stdout(&root, &["rev-parse", &ref_name("session-a")]),
        snapshot.commit
    );
}

#[tokio::test]
async fn snapshots_chain_under_the_session_ref() {
    let (_temp, root) = repository();

    let first = checkpoint::snapshot(&root, "session-b")
        .await
        .unwrap()
        .unwrap();
    std::fs::write(root.join("tracked.txt"), "second draft\n").unwrap();
    let second = checkpoint::snapshot(&root, "session-b")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        git_stdout(&root, &["rev-parse", &format!("{}^", second.commit)]),
        first.commit
    );
    assert_eq!(
        git_stdout(&root, &["rev-parse", &ref_name("session-b")]),
        second.commit
    );
    // Independent sessions chain independently.
    let other = checkpoint::snapshot(&root, "session-c")
        .await
        .unwrap()
        .unwrap();
    assert!(git_fails(
        &root,
        &["rev-parse", &format!("{}^", other.commit)]
    ));
}

#[tokio::test]
async fn non_git_directory_yields_none() {
    let temp = tempfile::tempdir().unwrap();
    // A plain directory, and one whose parent chain also holds no repo.
    let plain = temp.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let snapshot = checkpoint::snapshot(&plain, "session-d").await.unwrap();
    assert!(snapshot.is_none());
}

#[tokio::test]
async fn snapshot_before_the_first_commit_captures_untracked_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("newborn");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("draft.txt"), "hello\n").unwrap();

    let snapshot = checkpoint::snapshot(&root, "session-e")
        .await
        .unwrap()
        .expect("unborn HEAD is still a repository");

    assert_eq!(snapshot.head, None);
    assert_eq!(
        git_stdout(&root, &["show", &format!("{}:draft.txt", snapshot.commit)]),
        "hello"
    );
}

#[tokio::test]
async fn tracked_but_ignored_files_stay_in_the_snapshot() {
    let (_temp, root) = repository();
    // tracked.txt becomes ignored *after* being committed: it is still
    // tracked, so the snapshot must keep following its content.
    std::fs::write(root.join(".gitignore"), "tracked.txt\n").unwrap();
    std::fs::write(root.join("tracked.txt"), "ignored but tracked\n").unwrap();

    let snapshot = checkpoint::snapshot(&root, "session-f")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        git_stdout(&root, &["show", &format!("{}:tracked.txt", snapshot.commit)]),
        "ignored but tracked"
    );
}
