//! Config: TOML + markdown agents + AGENTS.md — see
//! meta/issues/config-and-agents-md.md.

mod agents_md;
mod toml;

pub use agents_md::system_prompt_for;
pub use toml::{
    CompactionConfig, Config, Loader, ProviderConfig, SubagentConfig, default_state_dir, load,
};

/// A usable agent: built-in or markdown-defined.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub prompt: String,
}

impl AgentDefinition {
    /// Built-in agents.
    pub fn builtins() -> Vec<Self> {
        vec![Self {
            name: "build".into(),
            description: "General-purpose coding agent with all tools".into(),
            model: None,
            prompt: String::new(), // base prompt; TUI supplies the core text
        }]
    }
}
