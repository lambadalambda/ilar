//! TOML config loading with project > user > defaults precedence.
//! TUI theme is a user preference and is not overridden per project.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use super::AgentDefinition;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GeneralConfig {
    pub model: Option<String>,
    pub theme: Option<String>,
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
    compaction: Option<CompactionLayer>,
    subagents: Option<SubagentLayer>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CompactionLayer {
    threshold: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SubagentLayer {
    max_concurrent: Option<usize>,
    max_depth: Option<usize>,
    background_tool_timeout_ms: Option<u64>,
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
    state_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GeneralConfigResolved {
    pub model: String,
    pub theme: String,
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
    state_dir: Option<PathBuf>,
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
            state_dir: None,
            env: Vec::new(),
            ignore_process_env: false,
        }
    }

    /// Loader that never reads process env (hermetic tests).
    pub fn no_env() -> Self {
        Self {
            project_dir: Some(PathBuf::from("/nonexistent")),
            ignore_process_env: true,
            ..Self::new()
        }
    }

    /// Loader with an explicit environment for hermetic tests.
    pub fn with_env(env: Vec<(&str, String)>, _unused: Vec<()>) -> Self {
        Self {
            env: env.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            project_dir: Some(PathBuf::from("/nonexistent")),
            ignore_process_env: true,
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

    pub fn state_dir(mut self, dir: PathBuf) -> Self {
        self.state_dir = Some(dir);
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
        let home = || PathBuf::from(self.env_lookup("HOME").unwrap_or_else(|| ".".into()));
        let user_dir = self
            .config_dir
            .clone()
            .or_else(|| self.env_lookup("ILAR_CONFIG_DIR").map(PathBuf::from))
            .unwrap_or_else(|| home().join(".config/ilar"));
        let state_dir = self
            .state_dir
            .clone()
            .or_else(|| self.env_lookup("ILAR_STATE_DIR").map(PathBuf::from))
            .unwrap_or_else(|| home().join(".local/state/ilar"));
        let project_dir = match self.project_dir.clone() {
            Some(project_dir) => project_dir,
            None => std::env::current_dir().context("resolving current project directory")?,
        };
        Config::load(user_dir, project_dir, state_dir, &self)
    }
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    fn load(
        user_dir: PathBuf,
        project_dir: PathBuf,
        state_dir: PathBuf,
        env: &Loader,
    ) -> anyhow::Result<Self> {
        // User file first, then project files layered on top. Theme stays user-scoped
        // so an in-app selection has the same effective value after restart.
        let mut merged = FileConfig::default();
        let user_path = user_dir.join("ilar.toml");
        if let Some(text) = read_config_file(&user_path)? {
            merged = merge_file(merged, &text, &user_path)?;
        }
        let user_theme = merged
            .general
            .as_ref()
            .and_then(|general| general.theme.clone());
        for path in [
            project_dir.join("ilar.toml"),
            project_dir.join(".ilar/ilar.toml"),
        ] {
            if let Some(text) = read_config_file(&path)? {
                merged = merge_file(merged, &text, &path)?;
            }
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
                    .as_ref()
                    .and_then(|general| general.model.clone())
                    .unwrap_or_else(|| "zai/glm-4.7".into()),
                theme: user_theme.unwrap_or_else(|| "terminal".into()),
            },
            providers,
            compaction: CompactionConfig {
                threshold: merged
                    .compaction
                    .and_then(|config| config.threshold)
                    .unwrap_or_else(default_threshold),
            },
            subagents: SubagentConfig {
                max_concurrent: merged
                    .subagents
                    .as_ref()
                    .and_then(|config| config.max_concurrent)
                    .unwrap_or_else(default_max_concurrent),
                max_depth: merged
                    .subagents
                    .as_ref()
                    .and_then(|config| config.max_depth)
                    .unwrap_or_else(default_max_depth),
                background_tool_timeout_ms: merged
                    .subagents
                    .and_then(|config| config.background_tool_timeout_ms)
                    .unwrap_or_else(default_background_tool_timeout_ms),
            },
            user_dir,
            project_dir,
            state_dir,
        })
    }

    /// Markdown agents from the config dir merged over built-ins.
    pub fn agents(&self) -> anyhow::Result<Vec<AgentDefinition>> {
        let mut agents = AgentDefinition::builtins();
        for dir in [
            self.user_dir.join("agents"),
            self.project_dir.join(".ilar/agents"),
        ] {
            for path in markdown_files(&dir)? {
                let name = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .with_context(|| format!("agent filename is not UTF-8: {}", path.display()))?;
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading agent definition {}", path.display()))?;
                let agent = parse_agent_md(name, &text)
                    .with_context(|| format!("parsing agent definition {}", path.display()))?;
                if let Some(agent) = agent {
                    agents.retain(|existing| existing.name != name);
                    agents.push(agent);
                }
            }
        }
        Ok(agents)
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
                    crate::auth::AuthStore::open(self.state_dir.clone()),
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

    /// Chat-capable catalog models exposed by currently configured providers.
    pub fn available_models(&self) -> Vec<&'static crate::model::ModelInfo> {
        use crate::model::ModelAccess;

        crate::model::catalog()
            .iter()
            .filter(|model| {
                let Some(provider) = self.providers.get(model.provider) else {
                    return false;
                };
                let chatgpt = provider.auth.as_deref() == Some("chatgpt");
                match model.access {
                    ModelAccess::OpenAi => provider.api_key.is_some() && !chatgpt,
                    ModelAccess::OpenAiBoth => chatgpt || provider.api_key.is_some(),
                    ModelAccess::Zai => {
                        provider.api_key.is_some() && provider.flavor.as_deref() != Some("openai")
                    }
                    ModelAccess::ZaiCodingPlan => {
                        provider.api_key.is_some() && provider.flavor.as_deref() == Some("openai")
                    }
                    ModelAccess::ZaiBoth => provider.api_key.is_some(),
                }
            })
            .collect()
    }

    /// User + project config dirs (agents searched in both).
    pub fn dirs(&self) -> (&Path, &Path) {
        (&self.user_dir, &self.project_dir)
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
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
                theme: "terminal".into(),
            },
            providers,
            compaction: CompactionConfig::default(),
            subagents: SubagentConfig::default(),
            user_dir: PathBuf::from("/nonexistent"),
            project_dir: PathBuf::from("/nonexistent"),
            state_dir: PathBuf::from("/nonexistent"),
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
        crate::model::find(model)
            .map(|model| model.context_limit)
            .or_else(|| fallback_context_limit(model))
    }

    fn input_limit(&self, model: &str) -> Option<u64> {
        crate::model::find(model)
            .map(|model| {
                let zai_anthropic = model.provider == "zai"
                    && self
                        .providers
                        .get("zai")
                        .is_some_and(|provider| provider.flavor.as_deref() != Some("openai"));
                if zai_anthropic {
                    model.context_limit.saturating_sub(16_384)
                } else {
                    model.input_limit
                }
            })
            .or_else(|| fallback_context_limit(model))
    }
}

fn fallback_context_limit(model: &str) -> Option<u64> {
    crate::provider::resolve_model(model)
        .ok()
        .and_then(|(provider, _)| match provider {
            "openai" => Some(128_000),
            "zai" => Some(200_000),
            _ => None,
        })
}

fn read_config_file(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading config {}", path.display())),
    }
}

fn merge_file(base: FileConfig, text: &str, origin: &Path) -> anyhow::Result<FileConfig> {
    let parsed: FileConfig =
        toml::from_str(text).with_context(|| format!("parsing config {}", origin.display()))?;
    validate_file(&parsed, origin)?;
    let mut merged = base;
    if let Some(g) = parsed.general {
        let current = merged.general.get_or_insert_with(GeneralConfig::default);
        if g.model.is_some() {
            current.model = g.model;
        }
        if g.theme.is_some() {
            current.theme = g.theme;
        }
    }
    if let Some(p) = parsed.providers {
        let map = merged.providers.get_or_insert_with(HashMap::new);
        for (k, v) in p {
            let current = map.entry(k).or_default();
            if v.base_url.is_some() {
                current.base_url = v.base_url;
            }
            if v.api_key.is_some() {
                current.api_key = v.api_key;
            }
            if v.flavor.is_some() {
                current.flavor = v.flavor;
            }
            if v.auth.is_some() {
                current.auth = v.auth;
            }
        }
    }
    if let Some(c) = parsed.compaction {
        let current = merged
            .compaction
            .get_or_insert_with(CompactionLayer::default);
        if c.threshold.is_some() {
            current.threshold = c.threshold;
        }
    }
    if let Some(s) = parsed.subagents {
        let current = merged.subagents.get_or_insert_with(SubagentLayer::default);
        if s.max_concurrent.is_some() {
            current.max_concurrent = s.max_concurrent;
        }
        if s.max_depth.is_some() {
            current.max_depth = s.max_depth;
        }
        if s.background_tool_timeout_ms.is_some() {
            current.background_tool_timeout_ms = s.background_tool_timeout_ms;
        }
    }
    Ok(merged)
}

/// Result of publishing a selected TUI theme to user configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemePersistOutcome {
    Saved,
    DurabilityUncertain(String),
}

/// Persist a user-selected TUI theme while preserving unrelated config text.
pub fn persist_general_theme(path: &Path, theme: &str) -> anyhow::Result<ThemePersistOutcome> {
    anyhow::ensure!(
        !theme.is_empty()
            && theme
                .chars()
                .all(|character| character.is_ascii_lowercase() || character == '-'),
        "invalid theme id {theme:?}"
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating config directory {}", parent.display()))?;

    for _ in 0..3 {
        let source = read_config_file(path)?.unwrap_or_default();
        if !source.is_empty() {
            merge_file(FileConfig::default(), &source, path)?;
        }
        let updated = set_general_theme(&source, theme)?;
        let parsed = merge_file(FileConfig::default(), &updated, path)?;
        anyhow::ensure!(
            parsed.general.and_then(|general| general.theme).as_deref() == Some(theme),
            "theme update did not produce the requested value"
        );

        if read_config_file(path)?.unwrap_or_default() != source {
            continue;
        }
        match crate::atomic_file::replace(
            path,
            updated.as_bytes(),
            crate::atomic_file::Mode::Preserve,
        ) {
            Ok(()) => return Ok(ThemePersistOutcome::Saved),
            Err(error) if persisted_general_theme(path).as_deref() == Some(theme) => {
                return Ok(ThemePersistOutcome::DurabilityUncertain(error.to_string()));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("persisting theme in {}", path.display()));
            }
        }
    }

    anyhow::bail!("config changed repeatedly while saving theme")
}

fn persisted_general_theme(path: &Path) -> Option<String> {
    read_config_file(path)
        .ok()
        .flatten()
        .and_then(|text| toml::from_str::<FileConfig>(&text).ok())
        .and_then(|config| config.general)
        .and_then(|general| general.theme)
}

fn set_general_theme(source: &str, theme: &str) -> anyhow::Result<String> {
    use toml_edit::{DocumentMut, Item, Table, Value, value};

    let mut document = if source.is_empty() {
        DocumentMut::new()
    } else {
        source
            .parse::<DocumentMut>()
            .context("parsing editable config")?
    };
    let general = document
        .entry("general")
        .or_insert_with(|| Item::Table(Table::new()));
    match general {
        Item::Table(table) => table["theme"] = value(theme),
        Item::Value(Value::InlineTable(table)) => {
            table.insert("theme", Value::from(theme));
        }
        _ => anyhow::bail!("general config must be a table"),
    }
    let updated = document.to_string();
    if source.contains("\r\n") && !source.replace("\r\n", "").contains('\n') {
        Ok(updated.replace("\r\n", "\n").replace('\n', "\r\n"))
    } else {
        Ok(updated)
    }
}

fn validate_file(config: &FileConfig, origin: &Path) -> anyhow::Result<()> {
    if let Some(threshold) = config.compaction.as_ref().and_then(|c| c.threshold) {
        anyhow::ensure!(
            threshold.is_finite() && threshold > 0.0 && threshold < 1.0,
            "{}: compaction.threshold must be finite and between 0 and 1",
            origin.display()
        );
    }
    if let Some(subagents) = &config.subagents {
        anyhow::ensure!(
            subagents.max_concurrent != Some(0),
            "{}: subagents.max_concurrent must be at least 1",
            origin.display()
        );
        anyhow::ensure!(
            subagents.max_depth != Some(0),
            "{}: subagents.max_depth must be at least 1",
            origin.display()
        );
        anyhow::ensure!(
            subagents.background_tool_timeout_ms != Some(0),
            "{}: subagents.background_tool_timeout_ms must be at least 1",
            origin.display()
        );
    }
    if let Some(providers) = &config.providers {
        for (name, provider) in providers {
            match name.as_str() {
                "openai" => {
                    anyhow::ensure!(
                        provider
                            .auth
                            .as_deref()
                            .is_none_or(|auth| matches!(auth, "api_key" | "chatgpt")),
                        "{}: providers.openai.auth must be `api_key` or `chatgpt`",
                        origin.display()
                    );
                    anyhow::ensure!(
                        provider.flavor.is_none(),
                        "{}: providers.openai.flavor is not supported",
                        origin.display()
                    );
                }
                "zai" => {
                    anyhow::ensure!(
                        provider
                            .flavor
                            .as_deref()
                            .is_none_or(|flavor| matches!(flavor, "anthropic" | "openai")),
                        "{}: providers.zai.flavor must be `anthropic` or `openai`",
                        origin.display()
                    );
                    anyhow::ensure!(
                        provider.auth.is_none(),
                        "{}: providers.zai.auth is not supported",
                        origin.display()
                    );
                }
                _ => anyhow::bail!("{}: unsupported provider {name:?}", origin.display()),
            }
        }
    }
    Ok(())
}

pub(crate) fn markdown_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading definition directory {}", dir.display()));
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry in {}", dir.display()))?;
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "md") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// frontmatter (description, model, disabled) + body prompt.
fn parse_agent_md(name: &str, text: &str) -> anyhow::Result<Option<AgentDefinition>> {
    let (frontmatter, body) = super::split_frontmatter(text)?;
    #[derive(Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct Frontmatter {
        description: Option<String>,
        model: Option<String>,
        disabled: Option<bool>,
        read_only: Option<bool>,
    }
    let fm: Frontmatter = toml::from_str(&frontmatter).context("invalid agent frontmatter")?;
    if fm.disabled == Some(true) {
        return Ok(None);
    }
    Ok(Some(AgentDefinition {
        name: name.into(),
        description: fm.description.unwrap_or_else(|| name.into()),
        model: fm.model,
        prompt: body.trim_start_matches('\n').trim().to_string(),
        workspace_mode: if fm.read_only == Some(true) {
            super::AgentWorkspaceMode::ReadOnly
        } else {
            super::AgentWorkspaceMode::Mutable
        },
    }))
}
