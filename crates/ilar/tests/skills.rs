use ilar::skill::SkillStore;

fn write(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn store(user: &std::path::Path, project: &std::path::Path) -> SkillStore {
    SkillStore::new(user.into(), project.into())
}

#[test]
fn discovers_skills_in_user_and_project_dirs() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/deploy.md"),
        "---\ndescription = \"How we deploy\"\n---\nRun deploy.sh with --canary first.\n",
    );
    write(
        &project.path().join(".ilar/skills/local-style.md"),
        "---\ndescription = \"Repo-specific style\"\n---\nAlways use tabs here.\n",
    );
    let store = store(user.path(), project.path());
    let skills = store.list().unwrap();
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"deploy"), "{names:?}");
    assert!(names.contains(&"local-style"), "{names:?}");
}

#[test]
fn project_skill_overrides_user_skill_by_name() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/deploy.md"),
        "---\ndescription = \"user version\"\n---\nUSER BODY\n",
    );
    write(
        &project.path().join(".ilar/skills/deploy.md"),
        "---\ndescription = \"project version\"\n---\nPROJECT BODY\n",
    );
    let store = store(user.path(), project.path());
    let skills = store.list().unwrap();
    let deploys: Vec<_> = skills.iter().filter(|s| s.name == "deploy").collect();
    assert_eq!(deploys.len(), 1, "duplicate: {skills:?}");
    assert_eq!(deploys[0].description, "project version");
}

#[test]
fn crlf_skill_frontmatter_loads() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &project.path().join(".ilar/skills/windows.md"),
        "---\r\ndescription = \"CRLF skill\"\r\n---\r\nUse Windows lines.\r\n",
    );

    let skill = store(user.path(), project.path())
        .load("windows")
        .unwrap()
        .expect("CRLF skill loads");
    assert_eq!(skill.description, "CRLF skill");
    assert_eq!(skill.body, "Use Windows lines.");
}

#[test]
fn malformed_skill_frontmatter_reports_its_path() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join(".ilar/skills/broken.md");
    write(
        &path,
        "---\ndescription = \"not exactly closed\"\n--- trailing\nbody\n",
    );

    let error = store(user.path(), project.path())
        .list()
        .expect_err("malformed skill must be diagnosed");
    let message = format!("{error:#}");
    assert!(
        message.contains(path.to_string_lossy().as_ref()),
        "{message}"
    );
    assert!(message.contains("exact `---`"), "{message}");
}

#[test]
fn documented_skill_triggers_are_accepted() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/deploy.md"),
        "---\ndescription = \"Deploy safely\"\ntriggers = [\"deploy\", \"release\"]\n---\nCanary first.\n",
    );

    let skill = store(user.path(), project.path())
        .load("deploy")
        .unwrap()
        .expect("skill with documented triggers loads");
    assert_eq!(skill.description, "Deploy safely");
}

#[test]
fn listing_renders_for_system_prompt() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/deploy.md"),
        "---\ndescription = \"How we deploy\"\n---\nbody\n",
    );
    write(
        &user.path().join("skills/review.md"),
        "---\ndescription = \"Review checklist\"\n---\nbody\n",
    );
    let store = store(user.path(), project.path());
    let listing = store.listing_prompt().unwrap();
    assert!(listing.contains("deploy"), "{listing}");
    assert!(listing.contains("How we deploy"), "{listing}");
    assert!(listing.contains("review"), "{listing}");
    assert!(
        !listing.contains("body"),
        "listing must not leak skill bodies"
    );
}

#[test]
fn load_returns_body() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/deploy.md"),
        "---\ndescription = \"How we deploy\"\n---\nCanary first, then full rollout.\n",
    );
    let store = store(user.path(), project.path());
    let skill = store.load("deploy").unwrap().expect("skill loads");
    assert_eq!(skill.name, "deploy");
    assert!(skill.body.contains("Canary first"));
    assert!(store.load("nonexistent").unwrap().is_none());
}

#[test]
fn builtin_worktree_isolation_skill_present() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let store = store(user.path(), project.path());
    let skills = store.list().unwrap();
    let wt = skills
        .iter()
        .find(|s| s.name == "worktree-isolation")
        .expect("builtin worktree-isolation skill");
    assert!(wt.body.contains("git worktree"), "{}", wt.body);
    assert!(wt.body.contains("task"), "should reference the task tool");
    assert!(wt.body.contains("\"workspace\""), "{}", wt.body);
    assert!(wt.body.contains("git_worktree"), "{}", wt.body);
}

#[tokio::test]
async fn skill_tool_loads_body_on_invocation() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/deploy.md"),
        "---\ndescription = \"How we deploy\"\n---\nCanary steps here.\n",
    );
    let store = std::sync::Arc::new(store(user.path(), project.path()));
    let reg = ilar::tools::ToolRegistry::builtin()
        .with_skills(store)
        .unwrap();
    let tool = reg.get("skill").expect("skill tool registered");
    let out = tool
        .run(
            serde_json::json!({"name": "deploy"}),
            ilar::tools::ToolContext::root(std::env::temp_dir()),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("Canary steps here."),
        "{}",
        out.content
    );
    // Unknown skill: error listing available.
    let out = tool
        .run(
            serde_json::json!({"name": "zzz"}),
            ilar::tools::ToolContext::root(std::env::temp_dir()),
        )
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("deploy"), "{}", out.content);
}
