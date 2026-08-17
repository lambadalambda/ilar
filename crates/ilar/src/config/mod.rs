//! Config: TOML + markdown agents + AGENTS.md — see
//! meta/issues/config-and-agents-md.md.

mod agents_md;
mod toml;

pub use agents_md::system_prompt_for;
pub(crate) use toml::markdown_files;
pub use toml::{CompactionConfig, Config, Loader, ProviderConfig, SubagentConfig, load};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentWorkspaceMode {
    #[default]
    Mutable,
    ReadOnly,
}

impl AgentDefinition {
    /// Built-in agents.
    pub fn builtins() -> Vec<Self> {
        vec![Self {
            name: "build".into(),
            description: "General-purpose coding agent with all tools".into(),
            model: None,
            prompt: String::new(), // base prompt; TUI supplies the core text
            workspace_mode: AgentWorkspaceMode::Mutable,
        }]
    }
}
