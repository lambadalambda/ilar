//! `[gateway]` and `[channels.<name>]`, handed over by the core as raw
//! tables and given meaning here.

use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// The agent every chat runs as; the core's default when unset.
    pub agent: Option<String>,
    /// `provider/model` for every chat; `general.model` when unset.
    /// An assistant on a channel rarely wants the terminal's model.
    pub model: Option<String>,
    /// Where the assistant's sessions work. Not a project checkout:
    /// `<state dir>/gateway/workspace` when unset.
    pub workspace: Option<PathBuf>,
    /// Seconds a source of `ilar-gateway notify` must wait between two
    /// messages, so a script in a loop cannot flood a chat.
    #[serde(default = "default_notify_interval_secs")]
    pub notify_interval_secs: u64,
}

fn default_notify_interval_secs() -> u64 {
    60
}

impl GatewayConfig {
    pub fn from_core(config: &ilar::config::Config) -> anyhow::Result<Self> {
        match &config.gateway {
            Some(table) => table
                .clone()
                .try_into()
                .context("parsing [gateway] in ilar.toml"),
            None => Ok(Self::default()),
        }
    }

    pub fn workspace(&self, config: &ilar::config::Config) -> PathBuf {
        self.workspace
            .clone()
            .unwrap_or_else(|| gateway_dir(config).join("workspace"))
    }
}

/// The gateway's own state, beside the sessions it drives.
pub fn gateway_dir(config: &ilar::config::Config) -> PathBuf {
    config.state_dir().join("gateway")
}

/// One channel's table, for its adapter to parse.
pub fn channel_table(config: &ilar::config::Config, name: &str) -> Option<toml::Table> {
    config
        .channels
        .as_ref()
        .and_then(|channels| channels.get(name))
        .and_then(|value| value.as_table().cloned())
}

/// Every configured channel by name.
pub fn channel_names(config: &ilar::config::Config) -> Vec<String> {
    config
        .channels
        .as_ref()
        .map(|channels| channels.keys().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gateway_table_parses_and_defaults() {
        let table: toml::Table = toml::from_str("agent = \"assistant\"").unwrap();
        let parsed: GatewayConfig = table.try_into().unwrap();
        assert_eq!(parsed.agent.as_deref(), Some("assistant"));
        assert_eq!(parsed.notify_interval_secs, 60);
        assert!(parsed.workspace.is_none());
        assert!(parsed.model.is_none());
        let table: toml::Table = toml::from_str("model = \"zai/glm-4.7\"").unwrap();
        let parsed: GatewayConfig = table.try_into().unwrap();
        assert_eq!(parsed.model.as_deref(), Some("zai/glm-4.7"));
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let table: toml::Table = toml::from_str("agnet = \"x\"").unwrap();
        assert!(table.try_into::<GatewayConfig>().is_err());
    }
}
