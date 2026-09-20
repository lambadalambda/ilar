//! Config: TOML + markdown agents + AGENTS.md — see
//! meta/issues/config-and-agents-md.md.

mod agents_md;
mod endpoints;
mod frontmatter;
mod toml;

pub use endpoints::Endpoint;

pub use agents_md::{
    CONTEXT_FILES, ProjectInstructions, SOUL_FILES, SystemPrompt, system_prompt_for,
    system_prompt_with,
};
pub(crate) use frontmatter::parse as parse_frontmatter;
pub use toml::{
    CacheCompactConfig, CompactionConfig, Config, Dirs, Loader, ProviderConfig, SubagentConfig,
    ThemePersistOutcome, load, persist_general_theme, provider_key_variables,
};
pub(crate) use toml::{credential_sources, markdown_files};

pub(crate) fn split_frontmatter(text: &str) -> anyhow::Result<(String, String)> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let mut lines = text.split('\n');
    anyhow::ensure!(
        lines.next() == Some("---"),
        "frontmatter must start with an exact `---` delimiter"
    );

    let mut frontmatter = Vec::new();
    let mut closed = false;
    for line in &mut lines {
        if line == "---" {
            closed = true;
            break;
        }
        frontmatter.push(line);
    }
    anyhow::ensure!(closed, "frontmatter must end with an exact `---` delimiter");
    Ok((frontmatter.join("\n"), lines.collect::<Vec<_>>().join("\n")))
}

/// A usable agent: built-in or markdown-defined.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub prompt: String,
    pub workspace_mode: AgentWorkspaceMode,
    /// Tool allowlist; `None` grants the default set for the workspace
    /// mode. Coordination only — not a security boundary.
    pub tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentWorkspaceMode {
    #[default]
    Mutable,
    ReadOnly,
}

/// What the built-in `review` agent may use: the mutable set with the
/// write tools removed. `secrets` so a review that runs `gh` can find
/// the token's name; no delegation, no services, no sudo.
const REVIEW_TOOLS: &[&str] = &["read", "glob", "grep", "webfetch", "bash", "secrets"];

impl AgentDefinition {
    /// Built-in agents.
    pub fn builtins() -> Vec<Self> {
        vec![
            Self {
                name: "build".into(),
                description: "General-purpose coding agent with all tools".into(),
                model: None,
                prompt: String::new(), // base prompt; TUI supplies the core text
                workspace_mode: AgentWorkspaceMode::Mutable,
                tools: None,
            },
            Self {
                name: "explore".into(),
                // What it *has*, not what it will refrain from. Called
                // read-only, this agent was read as "will not write"
                // and handed work needing a shell — one was told to
                // decode a PNG "with python3" and spent forty minutes
                // improvising with grep instead of saying it could
                // not. The list is the description now.
                description: "Repository inspection and review with read, glob, grep and \
                              webfetch only. No shell: it cannot run tests, builds, git or \
                              scripts, and cannot write files — send anything that must run a \
                              command to `build`. Several can run at once."
                    .into(),
                model: None,
                prompt: "Inspect, analyze and review. Your tools are read, glob, grep and \
                         webfetch: you have no shell, so you cannot run tests, builds, git or \
                         scripts, and you cannot write, edit or delete anything. If the work \
                         you were given needs one of those, say so in your first reply — name \
                         the tool you lack and report what you could establish without it. Do \
                         not improvise around a missing tool."
                    .into(),
                workspace_mode: AgentWorkspaceMode::ReadOnly,
                tools: None,
            },
            Self {
                name: "review".into(),
                // The reviewer that may run things. `explore` cannot,
                // because a shell under the shared read lease is four
                // parallel reviewers running four builds over one
                // target directory; so this one takes the write lease
                // like `build` and runs one at a time — with the write
                // tools taken away, since a review reports and does
                // not fix. The allowlist is the guard rail, plus the
                // prompt: `bash` can write, so this is coordination,
                // not a boundary, the same caveat `read_only` carries.
                description: "Review and verification that has to run things, with read, \
                              glob, grep, webfetch, bash and secrets: it can run tests, \
                              builds, git and scripts, and has no write or edit tool. \
                              Serialized per checkout like `build`, so one at a time; for \
                              inspection that needs no shell, `explore` runs in parallel."
                    .into(),
                model: None,
                prompt: "Review and verify. Your tools are read, glob, grep, webfetch, bash \
                         and secrets: you may run tests, builds, git and scripts, and you \
                         report what you find — you have no write or edit tool, and you do \
                         not fix what you find through the shell either. A finding names the \
                         file and line, what is wrong, and a concrete fix, so the agent that \
                         delegated to you can make it."
                    .into(),
                workspace_mode: AgentWorkspaceMode::Mutable,
                tools: Some(
                    REVIEW_TOOLS
                        .iter()
                        .map(|tool| (*tool).to_string())
                        .collect(),
                ),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A description is what a delegating model reads to choose an
    /// agent, so it has to name the toolset and not the intention:
    /// "read-only … without modifying the workspace" was read as
    /// "will not write" and answered with work that needed a shell,
    /// which the agent then spent forty minutes improvising around.
    /// Both halves say what is there and what is not — and a tool
    /// added to the read-only set has to be added to both.
    #[test]
    fn the_read_only_agent_names_every_tool_it_has_and_the_one_it_lacks() {
        let explore = AgentDefinition::builtins()
            .into_iter()
            .find(|agent| agent.name == "explore")
            .expect("explore is built in");

        for tool in crate::tools::ToolRegistry::read_only().tool_names() {
            assert!(
                explore.description.contains(tool),
                "the description does not name {tool}: {}",
                explore.description
            );
            assert!(
                explore.prompt.contains(tool),
                "the prompt does not name {tool}: {}",
                explore.prompt
            );
        }
        assert!(
            explore.description.contains("No shell") && explore.prompt.contains("no shell"),
            "the one tool it does not have is the one worth naming"
        );
    }

    /// The reviewer that may run things says so the same way: every
    /// tool on its allowlist in both halves, the two it lacks named,
    /// and the allowlist itself free of anything that writes.
    #[test]
    fn the_review_agent_names_every_tool_it_has_and_the_two_it_lacks() {
        let review = AgentDefinition::builtins()
            .into_iter()
            .find(|agent| agent.name == "review")
            .expect("review is built in");
        assert_eq!(review.workspace_mode, AgentWorkspaceMode::Mutable);
        let tools = review.tools.as_deref().expect("review is an allowlist");
        for tool in tools {
            assert!(
                review.description.contains(tool.as_str()),
                "the description does not name {tool}: {}",
                review.description
            );
            assert!(
                review.prompt.contains(tool.as_str()),
                "the prompt does not name {tool}: {}",
                review.prompt
            );
        }
        for writes in ["write", "edit", "task", "sudo"] {
            assert!(
                !tools.iter().any(|tool| tool == writes),
                "{writes} is on the review allowlist"
            );
        }
        assert!(
            review.description.contains("no write or edit tool")
                && review.prompt.contains("no write or edit tool"),
            "the tools it does not have are the ones worth naming"
        );
        assert!(
            review.description.contains("one at a time"),
            "the serialization is part of what a delegator chooses by"
        );
    }
}
