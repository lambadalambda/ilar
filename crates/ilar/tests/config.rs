use std::fs;

use ilar::config::{Config, Loader, system_prompt_for};

fn tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn write(path: &std::path::Path, content: &str) {
    fs::write(path, content).unwrap();
}

#[test]
fn defaults_when_no_config_exists() {
    let (_g, empty) = tempdir();
    let config = Loader::with_env(vec![("ILAR_ZAI_API_KEY", "zk".to_string())], vec![])
        .config_dir(empty)
        .resolve()
        .unwrap();
    assert_eq!(config.general.model, "zai/glm-4.7");
    assert_eq!(config.providers.len(), 2); // openai + zai defaults
    assert!(config.providers.contains_key("zai"));
    assert_eq!(
        config.providers["zai"].api_key.as_deref(),
        Some("zk"),
        "env var key resolved"
    );
    assert_eq!(config.compaction.threshold, 0.85);
    assert_eq!(config.subagents.max_concurrent, 10);
    assert_eq!(config.subagents.max_depth, 3);
    assert_eq!(config.subagents.background_tool_timeout_ms, 600_000);
}

#[test]
fn background_tool_timeout_is_configurable() {
    let (_g, dir) = tempdir();
    write(
        &dir.join("ilar.toml"),
        "[subagents]\nbackground_tool_timeout_ms = 42000\n",
    );
    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    assert_eq!(config.subagents.background_tool_timeout_ms, 42_000);
}

#[test]
fn project_config_overrides_user_config() {
    let (_gu, user) = tempdir();
    write(
        &user.join("ilar.toml"),
        "[general]\nmodel = \"zai/glm-4.7-air\"\n",
    );
    let (_gp, project) = tempdir();
    write(
        &project.join("ilar.toml"),
        "[general]\nmodel = \"openai/gpt-5.2\"\n",
    );

    let config = Loader::no_env()
        .config_dir(user.clone())
        .project_dir(project.clone())
        .resolve()
        .unwrap();
    assert_eq!(config.general.model, "openai/gpt-5.2", "project wins");

    // Without a project file, user config applies.
    fs::remove_file(project.join("ilar.toml")).unwrap();
    let config = Loader::no_env()
        .config_dir(user)
        .project_dir(project)
        .resolve()
        .unwrap();
    assert_eq!(config.general.model, "zai/glm-4.7-air");
}

#[test]
fn provider_settings_parsed() {
    let (_g, dir) = tempdir();
    write(
        &dir.join("ilar.toml"),
        r#"
[providers.zai]
base_url = "https://proxy.example/api/anthropic"

[providers.openai]
api_key = "inline-key"
"#,
    );
    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    assert_eq!(
        config.providers["zai"].base_url.as_deref(),
        Some("https://proxy.example/api/anthropic")
    );
    assert_eq!(
        config.providers["openai"].api_key.as_deref(),
        Some("inline-key")
    );
}

#[test]
fn markdown_agents_parsed_and_merged() {
    let (_g, dir) = tempdir();
    fs::create_dir_all(dir.join("agents")).unwrap();
    write(
        &dir.join("agents/reviewer.md"),
        "---\ndescription = \"Reviews code for bugs\"\nmodel = \"zai/glm-4.7-air\"\n---\nYou are a code reviewer. Be harsh.\n",
    );
    write(
        &dir.join("agents/disabled.md"),
        "---\ndescription = \"nope\"\ndisabled = true\n---\nunused\n",
    );

    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    let agents = config.agents();
    let reviewer = agents
        .iter()
        .find(|a| a.name == "reviewer")
        .expect("reviewer agent present");
    assert_eq!(reviewer.description, "Reviews code for bugs");
    assert_eq!(reviewer.model.as_deref(), Some("zai/glm-4.7-air"));
    assert!(reviewer.prompt.contains("Be harsh."));
    assert!(
        !agents.iter().any(|a| a.name == "disabled"),
        "disabled agents excluded"
    );
    // Built-in build agent still present.
    assert!(agents.iter().any(|a| a.name == "build"));
}

#[test]
fn agents_md_discovered_up_the_tree() {
    let (_g, root) = tempdir();
    fs::create_dir_all(root.join("a/b/c")).unwrap();
    write(&root.join("a/AGENTS.md"), "# Project rules\nUse tabs.\n");

    let prompt = system_prompt_for(&root.join("a/b/c"));
    assert!(
        prompt.to_lowercase().contains("use tabs"),
        "AGENTS.md not injected: {prompt}"
    );

    // CLAUDE.md fallback.
    fs::remove_file(root.join("a/AGENTS.md")).unwrap();
    write(&root.join("a/CLAUDE.md"), "# Legacy rules\nUse spaces.\n");
    let prompt = system_prompt_for(&root.join("a/b/c"));
    assert!(
        prompt.to_lowercase().contains("use spaces"),
        "CLAUDE.md fallback broken: {prompt}"
    );
}

#[test]
fn no_agents_md_yields_base_prompt() {
    let (_g, root) = tempdir();
    let prompt = system_prompt_for(&root);
    assert!(!prompt.to_lowercase().contains("agents.md"));
}

#[test]
fn closest_agents_md_wins() {
    let (_g, root) = tempdir();
    fs::create_dir_all(root.join("a/b")).unwrap();
    write(&root.join("AGENTS.md"), "root rules\n");
    write(&root.join("a/AGENTS.md"), "middle rules\n");

    let prompt = system_prompt_for(&root.join("a/b"));
    assert!(
        prompt.contains("middle rules"),
        "nearest should win: {prompt}"
    );
    assert!(!prompt.contains("root rules"), "parent leaked: {prompt}");
}

#[test]
fn provider_for_builds_concrete_providers() {
    let config = Config::default_for_tests();
    assert!(config.provider_for("zai/glm-4.7").is_some());
    assert!(config.provider_for("openai/gpt-5.2").is_some());
    assert!(config.provider_for("unknown/model").is_none());
}

#[test]
fn chatgpt_auth_needs_no_api_key() {
    // Regression: provider_for bailed on the missing api_key before
    // reaching the chatgpt branch.
    let (_g, dir) = tempdir();
    write(
        &dir.join("ilar.toml"),
        "[providers.openai]\nauth = \"chatgpt\"\n",
    );
    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    assert!(
        config.provider_for("openai/gpt-5.6-sol").is_some(),
        "chatgpt-auth provider without api_key must resolve"
    );
}
