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

fn repository_beneath(parent: &Path, name: &str) -> std::path::PathBuf {
    let root = parent.join(name);
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.name", "ilar tests"]);
    git(&root, &["config", "user.email", "ilar@example.invalid"]);
    std::fs::write(root.join("README.md"), "test\n").unwrap();
    git(&root, &["add", "README.md"]);
    git(&root, &["commit", "-qm", "initial"]);
    root
}

fn repository_named(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = repository_beneath(temp.path(), name);
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

/// A session rooted above its repositories (cwd `~/repos`) anchors a
/// worktree request to the repository containing the *requested* path:
/// task path `~/repos/project-task` (a worktree of `project`) — or the
/// checkout `~/repos/project` itself — validates against `project`,
/// not against the session cwd, which is in no repository at all.
#[tokio::test]
async fn anchors_to_the_repository_beneath_a_repositoryless_session_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let project = repository_beneath(temp.path(), "project");
    let worktree = temp.path().join("project-task");
    git(
        &project,
        &[
            "worktree",
            "add",
            "-qb",
            "parent-cwd-test",
            worktree.to_str().unwrap(),
        ],
    );
    let session = WorkspaceLocation::shared(temp.path().to_path_buf());

    let isolated = WorkspaceLocation::validated_git_worktree(&session, worktree.clone())
        .await
        .unwrap();
    let checkout = WorkspaceLocation::validated_git_worktree(&session, project.clone())
        .await
        .unwrap();

    assert_eq!(isolated.cwd(), std::fs::canonicalize(&worktree).unwrap());
    assert!(matches!(
        isolated.isolation(),
        WorkspaceIsolation::GitWorktree { .. }
    ));
    assert_ne!(isolated.id(), session.id());
    assert_eq!(checkout.root(), std::fs::canonicalize(&project).unwrap());
    assert_ne!(checkout.id(), isolated.id());
}

/// The validation error names the path it actually examined: a
/// requested path in no repository is blamed itself, with what was
/// expected of it — not the session cwd it never looked at.
#[tokio::test]
async fn blames_the_requested_path_when_it_is_in_no_repository() {
    let temp = tempfile::tempdir().unwrap();
    let plain = temp.path().join("not-a-repo");
    std::fs::create_dir(&plain).unwrap();
    let session = WorkspaceLocation::shared(temp.path().to_path_buf());

    let error = WorkspaceLocation::validated_git_worktree(&session, plain.clone())
        .await
        .unwrap_err()
        .to_string();

    let examined = std::fs::canonicalize(&plain).unwrap();
    assert!(error.contains(&format!("{examined:?}")), "{error}");
    assert!(error.contains("not inside a Git repository"), "{error}");
}

/// A parent probe that fails for any reason other than "no repository
/// here" — an unreadable cwd, a timeout — must not silently relax the
/// same-repository rules to containment-only. The error propagates,
/// naming the session cwd it could not judge.
#[tokio::test]
async fn a_failing_parent_probe_refuses_rather_than_relaxing_the_rules() {
    let (temp, root) = repository();
    let worktree = temp.path().join("isolated worktree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-qb",
            "probe-fail",
            worktree.to_str().unwrap(),
        ],
    );
    let vanished = temp.path().join("vanished session cwd");
    std::fs::create_dir(&vanished).unwrap();
    let session = WorkspaceLocation::shared(vanished.clone());
    std::fs::remove_dir(&vanished).unwrap();

    let error = format!(
        "{:#}",
        WorkspaceLocation::validated_git_worktree(&session, worktree)
            .await
            .unwrap_err()
    );

    assert!(
        error.contains("could not determine whether the session cwd"),
        "{error}"
    );
    assert!(
        !error.contains("outside the session cwd"),
        "containment-only judgement on a failed probe: {error}"
    );
}

/// A cwd that is not there is a refusal, not an abort. Both runtime
/// constructors take a cwd off disk — a resumed session's, a worktree
/// somebody deleted — and the panicking convenience they wrap would
/// have taken the whole process down with it.
#[test]
fn the_runtime_constructors_refuse_a_vanished_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let vanished = temp.path().join("deleted between launch and resume");
    std::fs::create_dir(&vanished).unwrap();
    std::fs::remove_dir(&vanished).unwrap();

    let error = match ilar::tools::ToolContext::try_root(vanished.clone()) {
        Ok(_) => panic!("a context was built on a directory that is not there"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("cannot be resolved"), "{error}");
    assert!(WorkspaceLocation::try_shared(vanished).is_err());
}

/// A repositoryless session cwd anchors only repositories beneath it:
/// a worktree of some unrelated repository elsewhere is refused, and
/// the refusal names both paths it compared.
#[tokio::test]
async fn rejects_a_repository_outside_the_repositoryless_session_cwd() {
    let session_temp = tempfile::tempdir().unwrap();
    let (_repo_temp, elsewhere) = repository();
    let session = WorkspaceLocation::shared(session_temp.path().to_path_buf());

    let error = WorkspaceLocation::validated_git_worktree(&session, elsewhere)
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("outside the session cwd"), "{error}");
    assert!(error.contains(&format!("{:?}", session.cwd())), "{error}");
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
