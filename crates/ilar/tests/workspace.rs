use std::path::Path;
use std::process::Command;
use std::time::Duration;

use ilar::tools::{WorkspaceAccess, WorkspaceIsolation, WorkspaceLocation, WorkspaceScheduler};

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

fn repository_named(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(name);
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.name", "ilar tests"]);
    git(&root, &["config", "user.email", "ilar@example.invalid"]);
    std::fs::write(root.join("README.md"), "test\n").unwrap();
    git(&root, &["add", "README.md"]);
    git(&root, &["commit", "-qm", "initial"]);
    (temp, root)
}

fn repository() -> (tempfile::TempDir, std::path::PathBuf) {
    repository_named("main checkout")
}

#[tokio::test]
async fn validates_registered_sibling_worktree_identity() {
    let (temp, root) = repository();
    let worktree = temp.path().join("isolated worktree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-qb",
            "isolated-test",
            worktree.to_str().unwrap(),
        ],
    );
    let parent = WorkspaceLocation::shared(root.clone());

    let isolated = WorkspaceLocation::validated_git_worktree(&parent, worktree.clone())
        .await
        .unwrap();

    assert_eq!(isolated.cwd(), std::fs::canonicalize(&worktree).unwrap());
    assert_ne!(isolated.id(), parent.id());
    assert!(matches!(
        isolated.isolation(),
        WorkspaceIsolation::GitWorktree { .. }
    ));
    let subdirectory = worktree.join("nested");
    std::fs::create_dir(&subdirectory).unwrap();
    let nested = WorkspaceLocation::validated_git_worktree(&parent, subdirectory)
        .await
        .unwrap();
    assert_eq!(nested.id(), isolated.id());
    assert_ne!(nested.cwd(), isolated.cwd());
    assert!(
        WorkspaceLocation::validated_git_worktree(&parent, root)
            .await
            .unwrap_err()
            .to_string()
            .contains("different")
    );
}

#[tokio::test]
async fn rejects_checkout_from_another_repository() {
    let (_first_temp, first) = repository();
    let (_second_temp, second) = repository();
    let parent = WorkspaceLocation::shared(first);

    let error = WorkspaceLocation::validated_git_worktree(&parent, second)
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("parent Git repository"),
        "{error:#}"
    );
}

/// Reads are advisory: a held write permit stops a second *writer* on
/// the same checkout and nothing else — a reader never waits, and a
/// distinct worktree keys on its own lock.
#[tokio::test]
async fn keyed_scheduler_serializes_writers_per_checkout_but_never_readers() {
    let (temp, root) = repository();
    let worktree = temp.path().join("parallel-worktree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-qb",
            "parallel-test",
            worktree.to_str().unwrap(),
        ],
    );
    let parent = WorkspaceLocation::shared(root);
    let isolated = WorkspaceLocation::validated_git_worktree(&parent, worktree)
        .await
        .unwrap();
    let scheduler = WorkspaceScheduler::for_location(&parent);
    let same_checkout_scheduler = scheduler.scoped(&parent);
    let isolated_scheduler = scheduler.scoped(&isolated);
    let _parent_write = scheduler.acquire(WorkspaceAccess::Mutating).await;

    tokio::time::timeout(
        Duration::from_millis(50),
        same_checkout_scheduler.acquire(WorkspaceAccess::ReadOnly),
    )
    .await
    .expect("a reader waited behind a write permit");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            same_checkout_scheduler.acquire(WorkspaceAccess::Mutating)
        )
        .await
        .is_err(),
        "a second writer escaped the same checkout's barrier"
    );
    tokio::time::timeout(
        Duration::from_millis(50),
        isolated_scheduler.acquire(WorkspaceAccess::Mutating),
    )
    .await
    .expect("distinct worktree should not share the checkout lock");
}

#[tokio::test]
async fn validates_git_paths_containing_newlines() {
    let (temp, root) = repository_named("main\ncheckout");
    let worktree = temp.path().join("isolated\nworktree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-qb",
            "newline-test",
            worktree.to_str().unwrap(),
        ],
    );
    let parent = WorkspaceLocation::shared(root);

    let isolated = WorkspaceLocation::validated_git_worktree(&parent, worktree.clone())
        .await
        .unwrap();

    assert_eq!(isolated.root(), std::fs::canonicalize(worktree).unwrap());
}
