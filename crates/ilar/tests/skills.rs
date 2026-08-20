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

    let store = store(user.path(), project.path());
    let skill = store
        .load("deploy")
        .unwrap()
        .expect("skill with documented triggers loads");
    assert_eq!(skill.description, "Deploy safely");
    assert_eq!(skill.triggers, vec!["deploy", "release"]);

    let listing = store.listing_prompt().unwrap();
    assert!(
        listing.contains("deploy: Deploy safely (use when: deploy; release)"),
        "{listing}"
    );
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

#[test]
fn builtin_mcp_via_cli_skill_present() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let store = store(user.path(), project.path());
    let skill = store
        .load("mcp-via-cli")
        .unwrap()
        .expect("builtin mcp-via-cli skill");
    assert!(skill.body.contains("mcp tools"), "{}", skill.body);
    assert!(skill.body.contains("mcp call"), "{}", skill.body);
    assert!(skill.body.contains("--params"), "{}", skill.body);
    assert!(
        skill.triggers.iter().any(|t| t.contains("MCP")),
        "{:?}",
        skill.triggers
    );
    let listing = store.listing_prompt().unwrap();
    assert!(listing.contains("mcp-via-cli"), "{listing}");
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

// ---- foreign formats ----

/// Claude Code and opencode ship skills as `<name>/SKILL.md` with YAML
/// frontmatter. Loading them unchanged is the whole point.
#[test]
fn yaml_skill_directories_load() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/repo-issues/SKILL.md"),
        "---\nname: repo-issues\ndescription: Manage repository-local issues.\ncompatibility: opencode\nmetadata:\n  category: workflow\n  scope: repository\n---\n# Repo issues\n\nBody text.\n",
    );

    let skills = store(user.path(), project.path()).list().unwrap();
    let skill = skills
        .iter()
        .find(|s| s.name == "repo-issues")
        .expect("yaml skill directory loads");
    assert_eq!(skill.description, "Manage repository-local issues.");
    assert!(skill.body.contains("Body text."), "{}", skill.body);
    // Unknown keys must not fail the load, and must not leak into the
    // body — the frontmatter delimiter is where the body starts.
    assert!(!skill.body.contains("compatibility"), "{}", skill.body);
    assert!(skill.body.starts_with("# Repo issues"), "{}", skill.body);
}

/// Values legitimately contain colons, so splitting on the wrong one
/// truncates them.
#[test]
fn yaml_values_may_contain_colons_and_quotes() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/agent-browser/SKILL.md"),
        "---\nname: agent-browser\ndescription: Use when the user says \"open a website\": drive the browser.\nallowed-tools: Bash(agent-browser:*), Bash(npx agent-browser:*)\nhidden: true\n---\nBody\n",
    );

    let skills = store(user.path(), project.path()).list().unwrap();
    let skill = skills.iter().find(|s| s.name == "agent-browser").unwrap();
    assert_eq!(
        skill.description,
        "Use when the user says \"open a website\": drive the browser."
    );
}

/// A `name:` that disagrees with the directory wins, so a renamed
/// directory does not silently change the invocation name.
#[test]
fn yaml_name_field_overrides_the_directory() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/some-folder/SKILL.md"),
        "---\nname: real-name\ndescription: Renamed.\n---\nBody\n",
    );

    let skills = store(user.path(), project.path()).list().unwrap();
    assert!(skills.iter().any(|s| s.name == "real-name"), "{skills:?}");
    assert!(!skills.iter().any(|s| s.name == "some-folder"));
}

/// YAML list form for triggers, alongside TOML's inline array.
#[test]
fn yaml_triggers_list_loads() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/tavily/SKILL.md"),
        "---\ndescription: Search the web.\ntriggers:\n  - search the web\n  - find articles about\n---\nBody\n",
    );

    let skills = store(user.path(), project.path()).list().unwrap();
    let skill = skills.iter().find(|s| s.name == "tavily").unwrap();
    assert_eq!(
        skill.triggers,
        vec!["search the web", "find articles about"]
    );
}

/// Existing TOML flat files keep working.
#[test]
fn toml_flat_skills_still_load_beside_yaml_directories() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/old.md"),
        "---\ndescription = \"A TOML skill\"\ntriggers = [\"cue\"]\n---\nOld body\n",
    );
    write(
        &user.path().join("skills/new/SKILL.md"),
        "---\ndescription: A YAML skill\n---\nNew body\n",
    );

    let skills = store(user.path(), project.path()).list().unwrap();
    let old = skills.iter().find(|s| s.name == "old").unwrap();
    assert_eq!(old.description, "A TOML skill");
    assert_eq!(old.triggers, vec!["cue"]);
    let new = skills.iter().find(|s| s.name == "new").unwrap();
    assert_eq!(new.description, "A YAML skill");
}

/// `description: |` puts the text on following indented lines. Real
/// skills use it for long descriptions, and treating `|` as the value
/// silently loses the whole thing.
#[test]
fn yaml_block_scalars_load_as_the_value() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("skills/tavily/SKILL.md"),
        "---\nname: tavily\ndescription: |\n  Web search via the Tavily CLI.\n  Use it for research.\ncompatibility: Requires tavily-cli\n---\nBody\n",
    );

    let skills = store(user.path(), project.path()).list().unwrap();
    let skill = skills.iter().find(|s| s.name == "tavily").unwrap();
    assert_eq!(
        skill.description,
        "Web search via the Tavily CLI. Use it for research."
    );
}

/// Real files, copied verbatim from an opencode install. The point of
/// the feature is that these load untouched, so assert on them rather
/// than on hand-written approximations.
#[test]
fn verbatim_opencode_skill_files_load() {
    let project = tempfile::tempdir().unwrap();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let skills = store(&fixtures, project.path()).list().unwrap();

    let tavily = skills
        .iter()
        .find(|s| s.name == "search-and-research-with-tavily")
        .expect("block-scalar skill loads");
    assert!(
        tavily
            .description
            .starts_with("Web search, content extraction"),
        "{}",
        tavily.description
    );
    assert!(
        tavily
            .description
            .contains("Do NOT trigger for local file operations"),
        "the block scalar was truncated: {} chars",
        tavily.description.len()
    );

    let browser = skills
        .iter()
        .find(|s| s.name == "agent-browser")
        .expect("long single-line skill loads");
    assert!(browser.description.contains("Prefer agent-browser over"));
    assert!(browser.body.contains("agent-browser"), "body preserved");
}
