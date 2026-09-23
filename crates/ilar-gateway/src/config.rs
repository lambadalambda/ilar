//! `[gateway]` and `[channels.<name>]`, handed over by the core as raw
//! tables and given meaning here.

use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// The assistant's home: `SOUL.md`, `skills/`, `agents/`, memory,
    /// workspace, routes, jobs, the channel accounts. `<state
    /// dir>/gateway` when unset.
    pub home: Option<PathBuf>,
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
    /// What the model may run for a chat, and for the subagents it
    /// spawns.
    #[serde(default)]
    pub tools: crate::policy::ToolPolicy,
    /// A periodic turn on chats of your choosing, silent unless the
    /// model uses the message tool.
    #[serde(default)]
    pub heartbeat: Heartbeat,
    /// How often due jobs and heartbeats are looked for.
    #[serde(default = "default_scheduler_tick_secs")]
    pub scheduler_tick_secs: u64,
    #[serde(default)]
    pub memory: Memory,
    /// A status line in the chat while a turn runs — "thinking — …",
    /// "running bash: …" — edited as things move and deleted when the
    /// reply comes. Off with `status = false`.
    #[serde(default = "default_true")]
    pub status: bool,
    /// One line to the last private chat when the gateway starts and
    /// when it stops, so a restart is visible where the person looks.
    #[serde(default = "default_true")]
    pub announce: bool,
    /// Seconds between two edits of the status line: on Delta Chat
    /// every edit is a message on the wire.
    #[serde(default = "default_status_interval_secs")]
    pub status_interval_secs: u64,
    /// Seconds between two tries at a send the channel refused. Four
    /// tries, enough to outlast a channel that is reconnecting; then
    /// the chat and the model are told it never went.
    #[serde(default = "default_send_retry_secs")]
    pub send_retry_secs: u64,
    /// The review after a turn: once per idle episode, what was worth
    /// keeping.
    #[serde(default)]
    pub review: crate::review::ReviewConfig,
    /// The weekly review of memory and skills: a cron job of its own.
    #[serde(default)]
    pub weekly: crate::weekly::WeeklyConfig,
}

fn default_status_interval_secs() -> u64 {
    4
}

fn default_send_retry_secs() -> u64 {
    2
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    /// Core files in the prompt, the archive behind the tools, and a
    /// daily note at every compaction. On unless said otherwise.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Each prompt surfaces the notes it matches, after the message.
    #[serde(default = "default_true")]
    pub recall: bool,
    /// A chat opens with the newest notes' index beside the core.
    #[serde(default = "default_true")]
    pub index: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            home: None,
            agent: None,
            model: None,
            workspace: None,
            notify_interval_secs: default_notify_interval_secs(),
            tools: Default::default(),
            heartbeat: Default::default(),
            scheduler_tick_secs: default_scheduler_tick_secs(),
            memory: Default::default(),
            status: true,
            announce: true,
            status_interval_secs: default_status_interval_secs(),
            send_retry_secs: default_send_retry_secs(),
            review: Default::default(),
            weekly: Default::default(),
        }
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            enabled: true,
            recall: true,
            index: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    /// Off when zero.
    #[serde(default)]
    pub every_secs: u64,
    #[serde(default = "default_heartbeat_prompt")]
    pub prompt: String,
    /// Session keys (`channel:chat`) to beat on; only chats that have
    /// written are reachable anyway.
    #[serde(default)]
    pub chats: Vec<String>,
}

fn default_scheduler_tick_secs() -> u64 {
    30
}

fn default_heartbeat_prompt() -> String {
    concat!(
        "Heartbeat. Look at what you know is going on; if there is something the person ",
        "should hear now, send it with the message tool, otherwise stay silent."
    )
    .into()
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

    /// Where everything of the assistant's lives.
    pub fn home(&self, config: &ilar::config::Config) -> PathBuf {
        self.home.clone().unwrap_or_else(|| gateway_dir(config))
    }

    pub fn workspace(&self, config: &ilar::config::Config) -> PathBuf {
        self.workspace
            .clone()
            .unwrap_or_else(|| self.home(config).join("workspace"))
    }
}

/// The default home, beside the sessions the gateway drives.
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

/// A channel that would answer nobody, or anybody without being told
/// to: refused at start, where the process can stop on it, rather than
/// in the channel's run, where the gateway restarts it every few
/// seconds for as long as it is up.
pub fn check_allowlist(
    channel: &str,
    allow_from: &[String],
    allow_anyone: bool,
) -> anyhow::Result<()> {
    if allow_from.is_empty() && !allow_anyone {
        anyhow::bail!(
            "[channels.{channel}] has no allow_from; list who may talk, or set allow_anyone = true"
        );
    }
    Ok(())
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
    #[test]
    fn a_channel_that_answers_nobody_does_not_start() {
        let refused = super::check_allowlist("telegram", &[], false).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("[channels.telegram] has no allow_from")
        );
        assert!(super::check_allowlist("telegram", &[], true).is_ok());
        assert!(super::check_allowlist("telegram", &["@me".into()], false).is_ok());
    }

    use super::*;

    #[test]
    fn the_gateway_table_parses_and_defaults() {
        let table: toml::Table = toml::from_str("agent = \"assistant\"").unwrap();
        let parsed: GatewayConfig = table.try_into().unwrap();
        assert_eq!(parsed.agent.as_deref(), Some("assistant"));
        assert_eq!(parsed.notify_interval_secs, 60);
        assert!(parsed.workspace.is_none());
        assert!(parsed.model.is_none());
        assert!(parsed.status);
        assert_eq!(parsed.status_interval_secs, 4);
        assert_eq!(parsed.send_retry_secs, 2);
        let table: toml::Table = toml::from_str("model = \"zai/glm-4.7\"").unwrap();
        let parsed: GatewayConfig = table.try_into().unwrap();
        assert_eq!(parsed.model.as_deref(), Some("zai/glm-4.7"));
        assert!(parsed.tools.is_empty());
        let table: toml::Table =
            toml::from_str("[tools]\nsafe_mode = true\ndeny = [\"task\"]").unwrap();
        let parsed: GatewayConfig = table.try_into().unwrap();
        assert!(parsed.tools.safe_mode);
        assert_eq!(parsed.tools.deny, ["task"]);
    }

    #[test]
    fn the_default_heartbeat_prompt_is_one_clean_line() {
        assert!(!default_heartbeat_prompt().contains("  "));
        assert!(GatewayConfig::default().memory.enabled);
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let table: toml::Table = toml::from_str("agnet = \"x\"").unwrap();
        assert!(table.try_into::<GatewayConfig>().is_err());
    }
}
