use std::fs;

use ilar::config::{
    AgentWorkspaceMode, CompactionConfig, Config, Loader, SubagentConfig, ThemePersistOutcome,
    persist_general_theme, system_prompt_for,
};
use ilar::provider::ProviderResolver;

fn tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn write(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
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
    assert_eq!(config.general.reasoning, None);
    // A tuned dark theme by default; `terminal` is opt-in for people who
    // want their own terminal colours instead.
    assert_eq!(config.general.theme, "carbon");
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
fn confirmed_theme_is_persisted_without_discarding_user_config() {
    let (_guard, dir) = tempdir();
    let path = dir.join("ilar.toml");
    write(
        &path,
        "# keep this comment\n[general]\nmodel = \"openai/gpt-5.2\"\n\n[providers.openai]\nauth = \"chatgpt\"\n",
    );

    assert_eq!(
        persist_general_theme(&path, "carbon").unwrap(),
        ThemePersistOutcome::Saved
    );
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep this comment"), "{text}");
    assert!(text.contains("model = \"openai/gpt-5.2\""), "{text}");
    assert!(text.contains("theme = \"carbon\""), "{text}");
    assert!(text.contains("[providers.openai]"), "{text}");

    persist_general_theme(&path, "frost").unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.matches("theme =").count(), 1, "{text}");
    assert!(text.contains("theme = \"frost\""), "{text}");

    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    assert_eq!(config.general.theme, "frost");
}

#[test]
fn theme_persistence_edits_toml_without_matching_multiline_string_contents() {
    let (_guard, dir) = tempdir();
    let path = dir.join("ilar.toml");
    let source = concat!(
        "[providers.openai]\r\n",
        "api_key = \"\"\"not-a-secret\r\n",
        "[general]\r\n",
        "theme = \\\"text-only\\\"\r\n",
        "\"\"\"\r\n",
        "\r\n",
        "[general] # preserve this header\r\n",
        "model = \"openai/gpt-5.2\"\r\n",
    );
    write(&path, source);

    persist_general_theme(&path, "parchment").unwrap();

    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("theme = \\\"text-only\\\""), "{text}");
    assert!(text.contains("[general] # preserve this header"), "{text}");
    assert!(text.contains("theme = \"parchment\""), "{text}");
    assert!(
        !text.replace("\r\n", "").contains('\n'),
        "line endings changed: {text:?}"
    );
}

#[test]
fn theme_is_a_user_preference_not_a_project_override() {
    let (_user_guard, user) = tempdir();
    write(&user.join("ilar.toml"), "[general]\ntheme = \"frost\"\n");
    let (_project_guard, project) = tempdir();
    write(
        &project.join("ilar.toml"),
        "[general]\nmodel = \"openai/gpt-5.2\"\ntheme = \"carbon\"\n",
    );

    let config = Loader::no_env()
        .config_dir(user)
        .project_dir(project)
        .resolve()
        .unwrap();

    assert_eq!(config.general.model, "openai/gpt-5.2");
    assert_eq!(config.general.theme, "frost");
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
fn public_section_types_retain_deserialization_defaults() {
    let compaction: CompactionConfig = toml::from_str("").unwrap();
    assert_eq!(compaction.threshold, 0.85);

    let subagents: SubagentConfig = toml::from_str("max_depth = 7").unwrap();
    assert_eq!(subagents.max_concurrent, 10);
    assert_eq!(subagents.max_depth, 7);
    assert_eq!(subagents.background_tool_timeout_ms, 600_000);
}

#[test]
fn project_config_overrides_user_config() {
    let (_gu, user) = tempdir();
    write(
        &user.join("ilar.toml"),
        "[general]\nmodel = \"openai/gpt-5.2\"\nreasoning = \"low\"\n",
    );
    let (_gp, project) = tempdir();
    write(
        &project.join("ilar.toml"),
        "[general]\nreasoning = \"high\"\n",
    );

    let config = Loader::no_env()
        .config_dir(user.clone())
        .project_dir(project.clone())
        .resolve()
        .unwrap();
    assert_eq!(config.general.model, "openai/gpt-5.2");
    assert_eq!(config.general.reasoning.as_deref(), Some("high"));

    // Without a project file, user config applies.
    fs::remove_file(project.join("ilar.toml")).unwrap();
    let config = Loader::no_env()
        .config_dir(user)
        .project_dir(project)
        .resolve()
        .unwrap();
    assert_eq!(config.general.model, "openai/gpt-5.2");
    assert_eq!(config.general.reasoning.as_deref(), Some("low"));
}

#[test]
fn higher_config_layer_can_reset_reasoning_to_provider_default() {
    let (_user_guard, user) = tempdir();
    write(
        &user.join("ilar.toml"),
        "[general]\nmodel = \"openai/gpt-5.2\"\nreasoning = \"high\"\n",
    );
    let (_project_guard, project) = tempdir();
    write(
        &project.join("ilar.toml"),
        "[general]\nmodel = \"zai/glm-4.7\"\nreasoning = \"default\"\n",
    );

    let config = Loader::no_env()
        .config_dir(user)
        .project_dir(project)
        .resolve()
        .unwrap();

    assert_eq!(config.general.model, "zai/glm-4.7");
    assert_eq!(config.general.reasoning, None);
}

#[test]
fn configured_reasoning_must_match_the_configured_model() {
    let (_guard, dir) = tempdir();
    write(
        &dir.join("ilar.toml"),
        "[general]\nmodel = \"zai/glm-4.7\"\nreasoning = \"high\"\n",
    );

    let error = Loader::no_env().config_dir(dir).resolve().unwrap_err();
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("unsupported variant \"high\" for zai/glm-4.7"),
        "{rendered}"
    );
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
        "---\ndescription = \"Reviews code for bugs\"\nmodel = \"zai/glm-4.7-air\"\nread_only = true\n---\nYou are a code reviewer. Be harsh.\n",
    );
    write(
        &dir.join("agents/disabled.md"),
        "---\ndescription = \"nope\"\ndisabled = true\n---\nunused\n",
    );

    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    let agents = config.agents().unwrap();
    let reviewer = agents
        .iter()
        .find(|a| a.name == "reviewer")
        .expect("reviewer agent present");
    assert_eq!(reviewer.description, "Reviews code for bugs");
    assert_eq!(reviewer.model.as_deref(), Some("zai/glm-4.7-air"));
    assert_eq!(reviewer.workspace_mode, AgentWorkspaceMode::ReadOnly);
    assert!(reviewer.prompt.contains("Be harsh."));
    assert!(
        !agents.iter().any(|a| a.name == "disabled"),
        "disabled agents excluded"
    );
    // Built-in build agent still present.
    assert!(agents.iter().any(|a| a.name == "build"));
    let explore = agents
        .iter()
        .find(|agent| agent.name == "explore")
        .expect("built-in read-only explorer present");
    assert_eq!(explore.workspace_mode, AgentWorkspaceMode::ReadOnly);
    assert!(explore.description.contains("review"));
}

#[test]
fn agent_tool_allowlists_parse_and_reject_unknown_names() {
    let (_g, dir) = tempdir();
    fs::create_dir_all(dir.join("agents")).unwrap();
    write(
        &dir.join("agents/searcher.md"),
        "---\ndescription = \"Search only\"\ntools = [\"grep\", \"glob\", \"read\"]\n---\nFind things.\n",
    );
    let config = Loader::no_env().config_dir(dir.clone()).resolve().unwrap();
    let agents = config.agents().unwrap();
    let searcher = agents
        .iter()
        .find(|a| a.name == "searcher")
        .expect("searcher agent present");
    assert_eq!(
        searcher.tools.as_deref(),
        Some(&["grep".to_string(), "glob".into(), "read".into()][..])
    );

    write(
        &dir.join("agents/broken.md"),
        "---\ndescription = \"bad\"\ntools = [\"grep\", \"teleport\"]\n---\nx\n",
    );
    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    let error = config.agents().unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("teleport"), "{message}");
    assert!(message.contains("known:"), "{message}");
}

#[test]
fn user_and_working_directory_context_are_combined_without_parent_search() {
    let (_g, root) = tempdir();
    let user = root.join("config");
    let parent = root.join("project");
    let cwd = parent.join("nested");
    fs::create_dir_all(&cwd).unwrap();
    write(&user.join("AGENTS.md"), "global rules\n");
    write(&parent.join("AGENTS.md"), "parent rules\n");
    write(&cwd.join("CLAUDE.md"), "working rules\n");

    let prompt = system_prompt_for(&user, &cwd).unwrap();

    assert!(prompt.contains("global rules"), "{prompt}");
    assert!(prompt.contains("working rules"), "{prompt}");
    assert!(!prompt.contains("parent rules"), "{prompt}");
    assert!(
        prompt.find("global rules") < prompt.find("working rules"),
        "working-directory rules should be last: {prompt}"
    );
}

#[test]
fn no_agents_md_yields_base_prompt() {
    let (_user_guard, user) = tempdir();
    let (_cwd_guard, cwd) = tempdir();
    let prompt = system_prompt_for(&user, &cwd).unwrap();
    assert!(!prompt.to_lowercase().contains("agents.md"));
    assert!(!prompt.to_lowercase().contains("claude.md"));
}

#[test]
fn agents_md_wins_over_claude_md_in_each_context_location() {
    let (_g, root) = tempdir();
    let user = root.join("config");
    let cwd = root.join("project");
    write(&user.join("AGENTS.md"), "user agents\n");
    write(&user.join("CLAUDE.md"), "user claude\n");
    write(&cwd.join("AGENTS.md"), "project agents\n");
    write(&cwd.join("CLAUDE.md"), "project claude\n");

    let prompt = system_prompt_for(&user, &cwd).unwrap();

    assert!(prompt.contains("user agents"), "{prompt}");
    assert!(prompt.contains("project agents"), "{prompt}");
    assert!(!prompt.contains("user claude"), "{prompt}");
    assert!(!prompt.contains("project claude"), "{prompt}");
}

#[test]
fn invalid_agents_md_is_reported_instead_of_falling_back() {
    let (_guard, root) = tempdir();
    let user = root.join("config");
    let cwd = root.join("project");
    fs::create_dir_all(&user).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    fs::write(user.join("AGENTS.md"), [0xff]).unwrap();
    write(&user.join("CLAUDE.md"), "must not be used\n");

    let error = system_prompt_for(&user, &cwd).unwrap_err().to_string();

    assert!(error.contains("AGENTS.md"), "{error}");
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

#[test]
fn model_catalog_drives_context_limits() {
    let config = Config::default_for_tests();

    for id in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
        let full_id = format!("openai/{id}");
        let model = ilar::model::find(&full_id).unwrap();
        assert_eq!(model.context_limit, 272_000);
        assert_eq!(model.max_context_limit, 1_050_000);
        assert_eq!(config.context_limit(&full_id), Some(272_000));
        assert_eq!(config.input_limit(&full_id), Some(272_000));
    }
    assert_eq!(config.context_limit("zai/glm-4.7"), Some(204_800));
    assert_eq!(config.input_limit("zai/glm-4.7"), Some(188_416));
    assert_eq!(config.context_limit("openai/not-in-catalog"), Some(128_000));
}

#[test]
fn configured_providers_expose_their_supported_models() {
    let config = Config::default_for_tests();
    let models = config.available_models();

    assert!(
        models
            .iter()
            .any(|model| model.full_id() == "openai/gpt-5.6-sol")
    );
    assert!(models.iter().any(|model| model.full_id() == "zai/glm-4.7"));
    assert!(!models.iter().any(|model| model.full_id() == "zai/glm-5.3"));
}

#[test]
fn zai_openai_flavor_uses_coding_plan_catalog() {
    let (_g, dir) = tempdir();
    write(
        &dir.join("ilar.toml"),
        "[providers.zai]\napi_key = \"zk\"\nflavor = \"openai\"\n",
    );
    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    let models = config.available_models();

    assert!(models.iter().any(|model| model.full_id() == "zai/glm-5.3"));
    assert!(!models.iter().any(|model| model.full_id() == "zai/glm-4.6"));
}

#[test]
fn chatgpt_auth_only_exposes_backend_supported_models() {
    let (_g, dir) = tempdir();
    write(
        &dir.join("ilar.toml"),
        "[providers.openai]\nauth = \"chatgpt\"\n",
    );
    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    let models = config.available_models();

    assert!(
        models
            .iter()
            .any(|model| model.full_id() == "openai/gpt-5.6-sol")
    );
    assert!(
        models
            .iter()
            .any(|model| model.full_id() == "openai/gpt-5.5")
    );
    assert!(
        !models
            .iter()
            .any(|model| model.full_id() == "openai/gpt-5.2")
    );
}

#[test]
fn chatgpt_auth_takes_catalog_precedence_over_an_api_key() {
    let (_g, dir) = tempdir();
    write(
        &dir.join("ilar.toml"),
        "[providers.openai]\nauth = \"chatgpt\"\napi_key = \"also-present\"\n",
    );
    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    let models = config.available_models();

    assert!(
        !models
            .iter()
            .any(|model| model.full_id() == "openai/gpt-5.2")
    );
}

#[test]
fn project_layers_preserve_omitted_nested_fields() {
    let (_gu, user) = tempdir();
    write(
        &user.join("ilar.toml"),
        r#"
[providers.zai]
api_key = "user-key"
base_url = "https://user.example"

[compaction]
threshold = 0.7

[subagents]
max_concurrent = 4
max_depth = 2
background_tool_timeout_ms = 42000
"#,
    );
    let (_gp, project) = tempdir();
    write(
        &project.join(".ilar/ilar.toml"),
        r#"
[providers.zai]
base_url = "https://project.example"

[compaction]

[subagents]
max_depth = 5
"#,
    );

    let config = Loader::no_env()
        .config_dir(user)
        .project_dir(project)
        .resolve()
        .unwrap();
    assert_eq!(config.providers["zai"].api_key.as_deref(), Some("user-key"));
    assert_eq!(
        config.providers["zai"].base_url.as_deref(),
        Some("https://project.example")
    );
    assert_eq!(config.compaction.threshold, 0.7);
    assert_eq!(config.subagents.max_concurrent, 4);
    assert_eq!(config.subagents.max_depth, 5);
    assert_eq!(config.subagents.background_tool_timeout_ms, 42_000);
}

#[test]
fn project_can_reset_inherited_chatgpt_auth_to_api_key() {
    let (_gu, user) = tempdir();
    write(
        &user.join("ilar.toml"),
        "[providers.openai]\nauth = \"chatgpt\"\n",
    );
    let (_gp, project) = tempdir();
    write(
        &project.join("ilar.toml"),
        "[providers.openai]\nauth = \"api_key\"\napi_key = \"project-key\"\n",
    );

    let config = Loader::no_env()
        .config_dir(user)
        .project_dir(project)
        .resolve()
        .unwrap();
    assert_eq!(config.providers["openai"].auth.as_deref(), Some("api_key"));
    assert_eq!(
        config.providers["openai"].api_key.as_deref(),
        Some("project-key")
    );
    assert!(config.provider_for("openai/gpt-5.2").is_some());
}

#[test]
fn config_read_errors_include_the_file_path() {
    let (_g, dir) = tempdir();
    let path = dir.join("ilar.toml");
    fs::write(&path, [0xff, 0xfe]).unwrap();

    let error = Loader::no_env()
        .config_dir(dir)
        .resolve()
        .expect_err("invalid UTF-8 must not look like a missing config");
    let message = format!("{error:#}");
    assert!(
        message.contains(path.to_string_lossy().as_ref()),
        "{message}"
    );
    assert!(message.to_lowercase().contains("utf"), "{message}");
}

#[test]
fn injected_environment_resolves_the_config_directory() {
    let (_g, dir) = tempdir();
    let (_gp, project) = tempdir();
    let (_gs, state) = tempdir();
    write(
        &dir.join("ilar.toml"),
        "[general]\nmodel = \"openai/gpt-5.6-sol\"\n",
    );

    let config = Loader::with_env(
        vec![
            ("ILAR_CONFIG_DIR", dir.display().to_string()),
            ("ILAR_STATE_DIR", state.display().to_string()),
        ],
        vec![],
    )
    .project_dir(project)
    .resolve()
    .unwrap();
    assert_eq!(config.general.model, "openai/gpt-5.6-sol");
    assert_eq!(config.dirs().0, dir);
    assert_eq!(config.state_dir(), state);
}

#[test]
fn project_agents_override_user_agents_and_accept_crlf() {
    let (_gu, user) = tempdir();
    write(
        &user.join("agents/reviewer.md"),
        "---\ndescription = \"user reviewer\"\n---\nuser prompt\n",
    );
    let (_gp, project) = tempdir();
    write(
        &project.join(".ilar/agents/reviewer.md"),
        "---\r\ndescription = \"project reviewer\"\r\nread_only = true\r\n---\r\nproject prompt\r\n",
    );

    let config = Loader::no_env()
        .config_dir(user)
        .project_dir(project)
        .resolve()
        .unwrap();
    let agents = config.agents().unwrap();
    let reviewers = agents
        .iter()
        .filter(|agent| agent.name == "reviewer")
        .collect::<Vec<_>>();
    assert_eq!(reviewers.len(), 1, "{agents:?}");
    assert_eq!(reviewers[0].description, "project reviewer");
    assert_eq!(reviewers[0].workspace_mode, AgentWorkspaceMode::ReadOnly);
    assert_eq!(reviewers[0].prompt, "project prompt");
}

#[test]
fn semantic_ranges_and_provider_modes_are_validated() {
    for (name, content) in [
        ("threshold", "[compaction]\nthreshold = 1.0\n"),
        ("concurrency", "[subagents]\nmax_concurrent = 0\n"),
        ("depth", "[subagents]\nmax_depth = 0\n"),
        (
            "background timeout",
            "[subagents]\nbackground_tool_timeout_ms = 0\n",
        ),
        ("OpenAI auth", "[providers.openai]\nauth = \"mystery\"\n"),
        ("z.ai flavor", "[providers.zai]\nflavor = \"mystery\"\n"),
    ] {
        let (_g, dir) = tempdir();
        write(&dir.join("ilar.toml"), content);
        let error = Loader::no_env().config_dir(dir).resolve().expect_err(name);
        assert!(
            format!("{error:#}").contains("ilar.toml"),
            "{name}: {error:#}"
        );
    }
}

#[test]
fn checked_in_config_example_parses() {
    let (_g, dir) = tempdir();
    write(
        &dir.join("ilar.toml"),
        include_str!("../../../ilar.toml.example"),
    );
    Loader::no_env().config_dir(dir).resolve().unwrap();
}

#[test]
fn checked_in_agent_example_parses() {
    let (_g, dir) = tempdir();
    write(
        &dir.join("agents/explorer.md"),
        include_str!("../../../examples/agents/explorer.md"),
    );

    let config = Loader::no_env().config_dir(dir).resolve().unwrap();
    let agent = config
        .agents()
        .unwrap()
        .into_iter()
        .find(|agent| agent.name == "explorer")
        .expect("checked-in agent example loads");
    assert_eq!(agent.workspace_mode, AgentWorkspaceMode::ReadOnly);
}

#[test]
fn malformed_agent_frontmatter_reports_its_path() {
    let (_g, dir) = tempdir();
    let path = dir.join("agents/broken.md");
    write(
        &path,
        "---\ndescription = \"not exactly closed\"\n----\nbody\n",
    );
    let config = Loader::no_env().config_dir(dir).resolve().unwrap();

    let error = config
        .agents()
        .expect_err("an inexact delimiter must be diagnosed");
    let message = format!("{error:#}");
    assert!(
        message.contains(path.to_string_lossy().as_ref()),
        "{message}"
    );
    assert!(message.contains("exact `---`"), "{message}");
}

#[test]
fn disabled_override_does_not_remove_an_existing_agent() {
    let (_g, dir) = tempdir();
    write(
        &dir.join("agents/build.md"),
        "---\ndisabled = true\n---\nunused\n",
    );
    let config = Loader::no_env().config_dir(dir).resolve().unwrap();

    let agents = config.agents().unwrap();
    assert!(
        agents.iter().any(|agent| agent.name == "build"),
        "{agents:?}"
    );
}

#[cfg(unix)]
#[test]
fn config_permission_errors_are_not_treated_as_missing() {
    use std::os::unix::fs::PermissionsExt;

    let (_g, dir) = tempdir();
    let path = dir.join("ilar.toml");
    write(&path, "[general]\nmodel = \"zai/glm-4.7\"\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_to_string(&path).is_ok() {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        return;
    }

    let result = Loader::no_env().config_dir(dir).resolve();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let error = result.expect_err("permission denial must be reported");
    assert!(
        format!("{error:#}").contains(path.to_string_lossy().as_ref()),
        "{error:#}"
    );
}

#[test]
fn agent_max_iterations_parses_layers_and_rejects_zero() {
    let (_g, dir) = tempdir();
    write(&dir.join("ilar.toml"), "[agent]\nmax_iterations = 400\n");
    let config = Loader::no_env().config_dir(dir.clone()).resolve().unwrap();
    assert_eq!(config.agent.max_iterations, 400);

    write(
        &dir.join("ilar.toml"),
        "[general]\nmodel = \"zai/glm-4.7\"\n",
    );
    let config = Loader::no_env().config_dir(dir.clone()).resolve().unwrap();
    assert_eq!(config.agent.max_iterations, 1_000, "default");

    write(&dir.join("ilar.toml"), "[agent]\nmax_iterations = 0\n");
    let error = Loader::no_env().config_dir(dir).resolve().unwrap_err();
    assert!(format!("{error:#}").contains("max_iterations"), "{error:#}");
}
