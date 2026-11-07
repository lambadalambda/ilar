//! Claude Code's memories, imported into a project's ilar store.

use ilar::claude_memory::{import, memory_dirs, slug};
use ilar::memory::MemoryStore;

fn write(dir: &std::path::Path, name: &str, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(name), text).unwrap();
}

#[test]
fn claude_code_names_a_project_by_its_path() {
    assert_eq!(
        slug(std::path::Path::new("/Users/me/repos/my.app_2")),
        "-Users-me-repos-my-app-2"
    );
}

/// Claude Code shares one memory across a repository's worktrees, kept
/// under the main checkout's path.
#[test]
fn a_worktree_looks_under_its_main_checkout_first() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("app");
    std::fs::create_dir_all(main.join(".git/worktrees/side")).unwrap();
    std::fs::write(main.join(".git/worktrees/side/commondir"), "../..\n").unwrap();
    let side = root.path().join("side");
    std::fs::create_dir_all(&side).unwrap();
    let gitdir = main.join(".git/worktrees/side");
    std::fs::write(side.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();

    let home = std::path::Path::new("/claude");
    let dirs = memory_dirs(home, &side);
    let main = main.canonicalize().unwrap();
    let side = side.canonicalize().unwrap();
    assert_eq!(
        dirs,
        [
            home.join("projects").join(slug(&main)).join("memory"),
            home.join("projects").join(slug(&side)).join("memory"),
        ]
    );
    // Outside a repository: the launch directory alone.
    assert_eq!(memory_dirs(home, root.path()).len(), 1);
}

#[test]
fn an_import_adds_each_memory_once() {
    let root = tempfile::tempdir().unwrap();
    let from = root.path().join("claude");
    write(
        &from,
        "MEMORY.md",
        "# Memory index\n\n- [Tenco install](tenco-install.md) — push then install.sh\n",
    );
    write(
        &from,
        "tenco-install.md",
        "---\nname: tenco-install\ndescription: \"Installing on tenco: push, then scripts/install.sh\"\n\
         metadata:\n  type: project\n  modified: 2026-08-31T07:57:49.450Z\n---\n\n\
         Run it on the box. See [[run-tests-on-tenco]].\n",
    );
    write(
        &from,
        "use-the-edit-tool.md",
        &format!(
            "---\nname: use-the-edit-tool\ndescription: {}\nmetadata:\n  type: feedback\n---\n\nEdit, not sed.\n",
            "x".repeat(250)
        ),
    );
    write(&from, "loose.md", "Fixtures live in the worktree.\n");
    // A copy under another file name, with the same Claude name.
    write(
        &from,
        "z-copy.md",
        "---\nname: tenco-install\ndescription: again\n---\n\nSame fact.\n",
    );
    let store = MemoryStore::new(root.path().join("memory"));

    let dry = import(&store, &from, true).unwrap();
    assert_eq!(dry.iter().filter(|done| done.added).count(), 3);
    assert!(
        store.notes().unwrap().is_empty(),
        "a dry run writes nothing"
    );

    let first = import(&store, &from, false).unwrap();
    assert_eq!(first.len(), 4, "MEMORY.md is not a note");
    assert!(!first[3].added, "one name, one note");
    let notes = store.notes().unwrap();
    assert_eq!(notes.len(), 3);

    let tenco = notes.iter().find(|n| n.title == "Tenco install").unwrap();
    assert_eq!(tenco.kind, "decision");
    assert_eq!(
        tenco.summary,
        "Installing on tenco: push, then scripts/install.sh"
    );
    assert_eq!(tenco.when.to_rfc3339(), "2026-08-31T07:57:49.450+00:00");
    assert!(tenco.id.starts_with("20260831-"));
    assert!(
        tenco
            .body
            .starts_with("Run it on the box. See [[run-tests-on-tenco]].")
    );
    assert!(
        tenco.body.contains("`tenco-install`"),
        "the [[link]] name stays findable"
    );

    let edit = notes
        .iter()
        .find(|n| n.title == "Use the edit tool")
        .unwrap();
    assert_eq!(edit.kind, "preference");
    assert_eq!(edit.summary.chars().count(), ilar::memory::SUMMARY_MAX);
    assert!(
        edit.body.contains(&"x".repeat(250)),
        "what was clipped is kept"
    );

    let loose = notes.iter().find(|n| n.title == "Loose").unwrap();
    assert_eq!(loose.summary, "Fixtures live in the worktree.");

    // Again, and after a forget: nothing comes back.
    store.forget(&tenco.id).unwrap();
    let second = import(&store, &from, false).unwrap();
    assert!(second.iter().all(|done| !done.added));
    assert_eq!(store.notes().unwrap().len(), 2);
}
