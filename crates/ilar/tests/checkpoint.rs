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
        git_stdout(
            &root,
            &["show", &format!("{}:tracked.txt", snapshot.commit)]
        ),
        "ignored but tracked"
    );
}

#[tokio::test]
async fn restore_makes_the_tree_match_the_snapshot() {
    let (_temp, root) = repository();
    std::fs::write(root.join("tracked.txt"), "snapshot state\n").unwrap();
    std::fs::write(root.join("fresh.txt"), "untracked at snapshot\n").unwrap();
    std::fs::remove_file(root.join("doomed.txt")).unwrap();
    let snapshot = checkpoint::snapshot(&root, "session-r")
        .await
        .unwrap()
        .unwrap();

    // Drift after the snapshot: edits, deletions, new files, revivals.
    std::fs::write(root.join("tracked.txt"), "drifted\n").unwrap();
    std::fs::remove_file(root.join("fresh.txt")).unwrap();
    std::fs::write(root.join("later.txt"), "created after\n").unwrap();
    std::fs::write(root.join("doomed.txt"), "revived\n").unwrap();
    std::fs::create_dir_all(root.join("deep/nest")).unwrap();
    std::fs::write(root.join("deep/nest/leaf.txt"), "nested\n").unwrap();
    let head_before = git_stdout(&root, &["rev-parse", "HEAD"]);

    checkpoint::restore(&root, &snapshot.commit).await.unwrap();

    let read = |path: &str| std::fs::read_to_string(root.join(path)).unwrap();
    assert_eq!(read("tracked.txt"), "snapshot state\n");
    assert_eq!(read("fresh.txt"), "untracked at snapshot\n");
    assert!(!root.join("later.txt").exists());
    assert!(!root.join("doomed.txt").exists());
    assert!(!root.join("deep/nest/leaf.txt").exists());
    assert!(!root.join("deep").exists(), "emptied directories go too");
    assert_eq!(git_stdout(&root, &["rev-parse", "HEAD"]), head_before);
}

#[tokio::test]
async fn restore_never_touches_ignored_files() {
    let (_temp, root) = repository();
    std::fs::write(root.join(".gitignore"), "*.secret\n").unwrap();
    std::fs::write(root.join("keys.secret"), "v1\n").unwrap();
    let snapshot = checkpoint::snapshot(&root, "session-s")
        .await
        .unwrap()
        .unwrap();

    std::fs::write(root.join("keys.secret"), "v2\n").unwrap();
    std::fs::write(root.join("new.secret"), "born after\n").unwrap();

    checkpoint::restore(&root, &snapshot.commit).await.unwrap();

    let read = |path: &str| std::fs::read_to_string(root.join(path)).unwrap();
    assert_eq!(read("keys.secret"), "v2\n");
    assert_eq!(read("new.secret"), "born after\n");
}

#[tokio::test]
async fn restore_from_a_subdirectory_covers_the_whole_repo() {
    let (_temp, root) = repository();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/inner.txt"), "inner\n").unwrap();
    let snapshot = checkpoint::snapshot(&root, "session-t")
        .await
        .unwrap()
        .unwrap();

    std::fs::write(root.join("tracked.txt"), "drifted\n").unwrap();
    std::fs::write(root.join("sub/inner.txt"), "drifted too\n").unwrap();

    checkpoint::restore(&root.join("sub"), &snapshot.commit)
        .await
        .unwrap();

    let read = |path: &str| std::fs::read_to_string(root.join(path)).unwrap();
    assert_eq!(read("tracked.txt"), "original\n");
    assert_eq!(read("sub/inner.txt"), "inner\n");
}

#[tokio::test]
async fn restore_handles_file_and_directory_swapping_places() {
    let (_temp, root) = repository();
    // Snapshot state: `swap` is a directory, `flat` is a file.
    std::fs::create_dir(root.join("swap")).unwrap();
    std::fs::write(root.join("swap/inner.txt"), "dir content\n").unwrap();
    std::fs::write(root.join("flat"), "file content\n").unwrap();
    let snapshot = checkpoint::snapshot(&root, "session-u")
        .await
        .unwrap()
        .unwrap();

    // Drift: both flip type.
    std::fs::remove_dir_all(root.join("swap")).unwrap();
    std::fs::write(root.join("swap"), "now a file\n").unwrap();
    std::fs::remove_file(root.join("flat")).unwrap();
    std::fs::create_dir(root.join("flat")).unwrap();
    std::fs::write(root.join("flat/nested.txt"), "now a dir\n").unwrap();

    checkpoint::restore(&root, &snapshot.commit).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(root.join("swap/inner.txt")).unwrap(),
        "dir content\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("flat")).unwrap(),
        "file content\n"
    );
    assert!(!root.join("flat/nested.txt").exists());
}
