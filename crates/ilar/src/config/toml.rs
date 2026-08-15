//! TOML config loading with project > user > defaults precedence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::AgentDefinition;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GeneralConfig {
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub flavor: Option<String>,
    /// "chatgpt" -> OAuth mode (run `ilar login`).
    pub auth: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfig {
    #[serde(default = "default_threshold")]
    pub threshold: f64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            threshold: default_threshold(),
        }
    }
}

fn default_threshold() -> f64 {
    0.85
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentConfig {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    #[serde(default = "default_background_tool_timeout_ms")]
    pub background_tool_timeout_ms: u64,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_max_concurrent(),
            max_depth: default_max_depth(),
            background_tool_timeout_ms: default_background_tool_timeout_ms(),
        }
    }
}

fn default_max_concurrent() -> usize {
    10
}

fn default_max_depth() -> usize {
    3
}

fn default_background_tool_timeout_ms() -> u64 {
    600_000
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    general: Option<GeneralConfig>,
    providers: Option<HashMap<String, ProviderConfig>>,
    compaction: Option<CompactionConfig>,
    subagents: Option<SubagentConfig>,
}

/// Fully-resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub general: GeneralConfigResolved,
    pub providers: HashMap<String, ProviderConfigResolved>,
    pub compaction: CompactionConfig,
    pub subagents: SubagentConfig,
    user_dir: PathBuf,
    project_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GeneralConfigResolved {
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct ProviderConfigResolved {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub flavor: Option<String>,
    pub auth: Option<String>,
}

/// Loader with overridable directories and environment (tests pass env
/// explicitly instead of mutating process env, which is unsafe in
/// edition 2024).
pub struct Loader {
    config_dir: Option<PathBuf>,
    project_dir: Option<PathBuf>,
    env: Vec<(String, String)>,
    ignore_process_env: bool,
}

pub fn load() -> Loader {
    Loader::new()
}

impl Loader {
    pub fn new() -> Self {
        Self {
            config_dir: None,
            project_dir: None,
            env: Vec::new(),
            ignore_process_env: false,
        }
    }

    /// Loader that never reads process env (hermetic tests).
    pub fn no_env() -> Self {
        Self {
            ignore_process_env: true,
            ..Self::new()
        }
    }

    /// Loader with an explicit environment (hermetic tests): looked up
    /// before process env.
    pub fn with_env(env: Vec<(&str, String)>, _unused: Vec<()>) -> Self {
        Self {
            env: env.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            ..Self::new()
        }
    }

    pub fn config_dir(mut self, dir: PathBuf) -> Self {
        self.config_dir = Some(dir);
        self
    }

    pub fn project_dir(mut self, dir: PathBuf) -> Self {
        self.project_dir = Some(dir);
        self
    }

    fn env_lookup(&self, key: &str) -> Option<String> {
        if let Some(v) = self
            .env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
        {
            return Some(v);
        }
        if self.ignore_process_env {
            return None;
        }
        std::env::var(key).ok()
    }

    pub fn resolve(self) -> anyhow::Result<Config> {
        let user_dir = self.config_dir.clone().unwrap_or_else(default_config_dir);
        let project_dir = self
            .project_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        Config::load(user_dir, project_dir, &self)
    }
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

fn default_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ILAR_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/ilar")
}

impl Config {
    fn load(user_dir: PathBuf, project_dir: PathBuf, env: &Loader) -> anyhow::Result<Self> {
        // User file first, then project file layered on top.
        let mut merged = FileConfig::default();
        if let Some(text) = read_config_file(&user_dir.join("ilar.toml")) {
            merged = merge_file(merged, &text, &user_dir)?;
        }
        if let Some(text) = read_config_file(&project_dir.join("ilar.toml")) {
            merged = merge_file(merged, &text, &project_dir)?;
        }
        if let Some(text) = read_config_file(&project_dir.join(".ilar/ilar.toml")) {
            merged = merge_file(merged, &text, &project_dir)?;
        }

        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfigResolved {
                base_url: merged
                    .providers
                    .as_ref()
                    .and_then(|p| p.get("openai"))
                    .and_then(|c| c.base_url.clone()),
                api_key: merged
                    .providers
                    .as_ref()
                    .and_then(|p| p.get("openai"))
                    .and_then(|c| c.api_key.clone())
                    .or_else(|| env.env_lookup("ILAR_OPENAI_API_KEY")),
                flavor: None,
                auth: merged
                    .providers
                    .as_ref()
                    .and_then(|p| p.get("openai"))
                    .and_then(|c| c.auth.clone()),
            },
        );
        providers.insert(
            "zai".to_string(),
            ProviderConfigResolved {
                base_url: merged
                    .providers
                    .as_ref()
                    .and_then(|p| p.get("zai"))
                    .and_then(|c| c.base_url.clone()),
                api_key: merged
                    .providers
                    .as_ref()
                    .and_then(|p| p.get("zai"))
                    .and_then(|c| c.api_key.clone())
                    .or_else(|| env.env_lookup("ILAR_ZAI_API_KEY")),
                flavor: merged
                    .providers
                    .as_ref()
                    .and_then(|p| p.get("zai"))
                    .and_then(|c| c.flavor.clone()),
                auth: None,
            },
        );

        Ok(Config {
            general: GeneralConfigResolved {
                model: merged
                    .general
                    .and_then(|g| g.model)
                    .unwrap_or_else(|| "zai/glm-4.7".into()),
            },
            providers,
            compaction: merged.compaction.unwrap_or_default(),
            subagents: merged.subagents.unwrap_or_default(),
            user_dir,
            project_dir,
        })
    }

    /// Markdown agents from the config dir merged over built-ins.
    pub fn agents(&self) -> Vec<AgentDefinition> {
        let mut agents = AgentDefinition::builtins();
        let dir = self.user_dir.join("agents");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return agents;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(agent) = parse_agent_md(name, &text) {
                // Markdown agents may override built-ins by name.
                agents.retain(|a| a.name != agent.name);
                agents.push(agent);
            }
        }
        agents
    }

    /// Build a concrete provider for "provider/model-id", or None if the
    /// provider name is unknown.
    pub fn provider_for(&self, model: &str) -> Option<Box<dyn crate::provider::Provider>> {
        let (provider_name, _model_id) = crate::provider::resolve_model(model).ok()?;
        let settings = self.providers.get(provider_name)?;
        let provider: Box<dyn crate::provider::Provider> = match provider_name {
            "openai" if settings.auth.as_deref() == Some("chatgpt") => {
                // OAuth mode needs no api_key — tokens come from the store.
                Box::new(crate::provider::openai::OpenAIProvider::with_chatgpt_auth(
                    crate::auth::AuthStore::open(default_state_dir()),
                    settings.base_url.clone(),
                ))
            }
            "openai" => {
                let api_key = settings.api_key.clone()?;
                Box::new(crate::provider::openai::OpenAIProvider::new(
                    api_key,
                    settings.base_url.clone(),
                ))
            }
            "zai" => {
                let api_key = settings.api_key.clone()?;
                use crate::provider::zai::Flavor;
                let flavor = match settings.flavor.as_deref() {
                    Some("openai") => Flavor::OpenAI,
                    _ => Flavor::Anthropic,
                };
                Box::new(crate::provider::zai::ZaiProvider::new(
                    api_key,
                    settings.base_url.clone(),
                    flavor,
                ))
            }
            _ => return None,
        };
        Some(provider)
    }

    /// User + project config dirs (agents searched in both).
    pub fn dirs(&self) -> (&Path, &Path) {
        (&self.user_dir, &self.project_dir)
    }

    /// Deterministic config for tests: both providers keyed, no env.
    pub fn default_for_tests() -> Self {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfigResolved {
                base_url: None,
                api_key: Some("test-openai-key".into()),
                flavor: None,
                auth: None,
            },
        );
        providers.insert(
            "zai".to_string(),
            ProviderConfigResolved {
                base_url: None,
                api_key: Some("test-zai-key".into()),
                flavor: None,
                auth: None,
            },
        );
        Self {
            general: GeneralConfigResolved {
                model: "zai/glm-4.7".into(),
            },
            providers,
            compaction: CompactionConfig::default(),
            subagents: SubagentConfig::default(),
            user_dir: PathBuf::from("/nonexistent"),
            project_dir: PathBuf::from("/nonexistent"),
        }
    }
}

impl crate::provider::ProviderResolver for Config {
    fn resolve_provider(&self, model: &str) -> anyhow::Result<crate::provider::ProviderHandle<'_>> {
        self.provider_for(model)
            .map(crate::provider::ProviderHandle::Owned)
            .ok_or_else(|| anyhow::anyhow!("no configured provider for model {model:?}"))
    }

    fn context_limit(&self, model: &str) -> Option<u64> {
        crate::provider::resolve_model(model)
            .ok()
            .map(|(provider, _)| if provider == "zai" { 200_000 } else { 128_000 })
    }
}

fn read_config_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn merge_file(base: FileConfig, text: &str, origin: &Path) -> anyhow::Result<FileConfig> {
    let parsed: FileConfig =
        toml::from_str(text).map_err(|e| anyhow::anyhow!("{origin:?}: {e}"))?;
    let mut merged = base;
    if let Some(g) = parsed.general {
        merged.general = Some(g);
    }
    if let Some(p) = parsed.providers {
        let map = merged.providers.get_or_insert_with(HashMap::new);
        for (k, v) in p {
            map.insert(k, v);
        }
    }
    if let Some(c) = parsed.compaction {
        merged.compaction = Some(c);
    }
    if let Some(s) = parsed.subagents {
        merged.subagents = Some(s);
    }
    Ok(merged)
}

/// frontmatter (description, model, disabled) + body prompt.
fn parse_agent_md(name: &str, text: &str) -> Option<AgentDefinition> {
    let text = text.trim_start_matches('\u{feff}');
    let rest = text.strip_prefix("---\n")?;
    let (frontmatter, body) = rest.split_once("\n---")?;

    #[derive(Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct Frontmatter {
        description: Option<String>,
        model: Option<String>,
        disabled: Option<bool>,
    }
    let fm: Frontmatter = toml::from_str(frontmatter).ok()?;
    if fm.disabled == Some(true) {
        return None;
    }
    Some(AgentDefinition {
        name: name.into(),
        description: fm.description.unwrap_or_else(|| name.into()),
        model: fm.model,
        prompt: body.trim_start_matches('\n').trim().to_string(),
    })
}

/// State directory: ILAR_STATE_DIR override, else ~/.local/state/ilar.
pub fn default_state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ILAR_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/state/ilar")
}
