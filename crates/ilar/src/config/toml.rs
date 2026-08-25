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
    pub reasoning: Option<String>,
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// "chatgpt" -> OAuth mode (run `ilar login`).
    pub auth: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Max provider calls per user turn. A runaway-loop backstop, not a
    /// working limit: long-thinking models routinely need hundreds.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
        }
    }
}

fn default_max_iterations() -> usize {
    1_000
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
    agent: Option<AgentLayer>,
    compaction: Option<CompactionLayer>,
    subagents: Option<SubagentLayer>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AgentLayer {
    max_iterations: Option<usize>,
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

/// One row per supported provider. Resolution, model listing, semantic
/// validation and fallback windows all read this table, so adding a
/// provider is this entry plus the two functions it names.
#[derive(Clone, Copy)]
struct ProviderKind {
    name: &'static str,
    /// Consulted when the configuration carries no `api_key`.
    api_key_env: &'static str,
    /// Accepted `auth` values; empty when the key is unsupported.
    auth_values: &'static [&'static str],
    /// Window assumed for a model the catalog does not list.
    fallback_context_limit: u64,
    /// Whether this configuration can reach a catalog row.
    reaches: fn(&ProviderConfigResolved, crate::model::ModelAccess) -> bool,
    /// The concrete client, or None when the configuration is incomplete.
    build: fn(&Config, &ProviderConfigResolved) -> Option<Box<dyn crate::provider::Provider>>,
}

static PROVIDERS: &[ProviderKind] = &[
    ProviderKind {
        name: "openai",
        api_key_env: "ILAR_OPENAI_API_KEY",
        auth_values: &["api_key", "chatgpt"],
        fallback_context_limit: 128_000,
        reaches: openai_reaches,
        build: openai_provider,
    },
    ProviderKind {
        name: "zai",
        api_key_env: "ILAR_ZAI_API_KEY",
        auth_values: &[],
        fallback_context_limit: 200_000,
        reaches: zai_reaches,
        build: zai_provider,
    },
];

fn provider_kind<'a>(name: &str, kinds: &'a [ProviderKind]) -> Option<&'a ProviderKind> {
    kinds.iter().find(|kind| kind.name == name)
}

fn chatgpt_auth(settings: &ProviderConfigResolved) -> bool {
    settings.auth.as_deref() == Some("chatgpt")
}

fn openai_reaches(settings: &ProviderConfigResolved, access: crate::model::ModelAccess) -> bool {
    use crate::model::ModelAccess;
    match access {
        ModelAccess::OpenAi => settings.api_key.is_some() && !chatgpt_auth(settings),
        ModelAccess::OpenAiBoth => chatgpt_auth(settings) || settings.api_key.is_some(),
        _ => false,
    }
}

fn openai_provider(
    config: &Config,
    settings: &ProviderConfigResolved,
) -> Option<Box<dyn crate::provider::Provider>> {
    if chatgpt_auth(settings) {
        // OAuth mode needs no api_key — tokens come from the store.
        return Some(Box::new(
            crate::provider::openai::OpenAIProvider::with_chatgpt_auth(
                crate::auth::AuthStore::open(config.state_dir.clone()),
                settings.base_url.clone(),
            ),
        ));
    }
    Some(Box::new(crate::provider::openai::OpenAIProvider::new(
        settings.api_key.clone()?,
        settings.base_url.clone(),
    )))
}

/// The only z.ai route is the coding-plan endpoint, so a model the plan
/// does not carry is not reachable however the key is configured.
fn zai_reaches(settings: &ProviderConfigResolved, access: crate::model::ModelAccess) -> bool {
    use crate::model::ModelAccess;
    settings.api_key.is_some()
        && matches!(access, ModelAccess::ZaiCodingPlan | ModelAccess::ZaiBoth)
}

fn zai_provider(
    _config: &Config,
    settings: &ProviderConfigResolved,
) -> Option<Box<dyn crate::provider::Provider>> {
    Some(Box::new(crate::provider::zai::ZaiProvider::new(
        settings.api_key.clone()?,
        settings.base_url.clone(),
    )))
}

/// Fully-resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub general: GeneralConfigResolved,
    pub providers: HashMap<String, ProviderConfigResolved>,
    pub agent: AgentConfig,
    pub compaction: CompactionConfig,
    pub subagents: SubagentConfig,
    /// Settings that parsed but were not honoured, one line each, for
    /// the frontend to show. A silently ignored setting reads as a bug
    /// in the program rather than a rule about the setting.
    pub warnings: Vec<String>,
    user_dir: PathBuf,
    project_dir: PathBuf,
    state_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GeneralConfigResolved {
    pub model: String,
    pub reasoning: Option<String>,
    pub theme: String,
}

#[derive(Debug, Clone)]
pub struct ProviderConfigResolved {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
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
    pub fn with_env(env: Vec<(&str, String)>) -> Self {
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
        let mut warnings = Vec::new();
        for path in [
            project_dir.join("ilar.toml"),
            project_dir.join(".ilar/ilar.toml"),
        ] {
            if let Some(text) = read_config_file(&path)? {
                if declares_theme(&text) {
                    warnings.push(format!(
                        "{}: general.theme is a user preference and is ignored in project config",
                        path.display()
                    ));
                }
                merged = merge_file(merged, &text, &path)?;
            }
        }

        let providers = resolve_providers(&merged, env, PROVIDERS);

        let model = merged
            .general
            .as_ref()
            .and_then(|general| general.model.clone())
            .unwrap_or_else(|| "zai/glm-4.7".into());
        let reasoning = merged
            .general
            .as_ref()
            .and_then(|general| general.reasoning.clone())
            .filter(|reasoning| reasoning != "default");
        crate::model::variant_options(&model, reasoning.as_deref())
            .context("validating general.reasoning")?;

        Ok(Config {
            general: GeneralConfigResolved {
                model,
                reasoning,
                // A tuned dark theme, not the adaptive one: the surfaces and
                // damped chrome it encodes are what a first run should show.
                theme: user_theme.unwrap_or_else(|| "carbon".into()),
            },
            providers,
            agent: AgentConfig {
                max_iterations: merged
                    .agent
                    .and_then(|config| config.max_iterations)
                    .unwrap_or_else(default_max_iterations),
            },
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
            warnings,
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
        let kind = provider_kind(provider_name, PROVIDERS)?;
        (kind.build)(self, settings)
    }

    /// Chat-capable catalog models exposed by currently configured providers.
    pub fn available_models(&self) -> Vec<&'static crate::model::ModelInfo> {
        available_models_in(&self.providers, PROVIDERS)
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
                auth: None,
            },
        );
        providers.insert(
            "zai".to_string(),
            ProviderConfigResolved {
                base_url: None,
                api_key: Some("test-zai-key".into()),
                auth: None,
            },
        );
        Self {
            general: GeneralConfigResolved {
                model: "zai/glm-4.7".into(),
                reasoning: None,
                theme: "carbon".into(),
            },
            agent: AgentConfig::default(),
            providers,
            compaction: CompactionConfig::default(),
            subagents: SubagentConfig::default(),
            warnings: Vec::new(),
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
            .or_else(|| fallback_context_limit(model, PROVIDERS))
    }

    fn input_limit(&self, model: &str) -> Option<u64> {
        crate::model::find(model)
            .map(|model| model.input_limit)
            .or_else(|| fallback_context_limit(model, PROVIDERS))
    }

    fn compaction_limit(&self, model: &str) -> Option<u64> {
        crate::model::find(model)
            .map(crate::model::compaction_limit)
            // Never exceed the model's own input cap.
            .zip(self.input_limit(model))
            .map(|(compaction, input)| compaction.min(input))
            .or_else(|| fallback_context_limit(model, PROVIDERS))
    }
}

/// Resolved settings for every known provider: file values, then the
/// provider's environment variable for the key. Keys a provider does not
/// support are rejected by validation, so copying them is a no-op.
fn resolve_providers(
    merged: &FileConfig,
    env: &Loader,
    kinds: &[ProviderKind],
) -> HashMap<String, ProviderConfigResolved> {
    kinds
        .iter()
        .map(|kind| {
            let configured = merged
                .providers
                .as_ref()
                .and_then(|providers| providers.get(kind.name));
            let field = |pick: fn(&ProviderConfig) -> Option<String>| configured.and_then(pick);
            (
                kind.name.to_string(),
                ProviderConfigResolved {
                    base_url: field(|config| config.base_url.clone()),
                    api_key: field(|config| config.api_key.clone())
                        .or_else(|| env.env_lookup(kind.api_key_env)),
                    auth: field(|config| config.auth.clone()),
                },
            )
        })
        .collect()
}

fn available_models_in(
    providers: &HashMap<String, ProviderConfigResolved>,
    kinds: &[ProviderKind],
) -> Vec<&'static crate::model::ModelInfo> {
    crate::model::catalog()
        .iter()
        .filter(|model| {
            providers
                .get(model.provider)
                .zip(provider_kind(model.provider, kinds))
                .is_some_and(|(settings, kind)| (kind.reaches)(settings, model.access))
        })
        .collect()
}

fn fallback_context_limit(model: &str, kinds: &[ProviderKind]) -> Option<u64> {
    crate::provider::resolve_model(model)
        .ok()
        .and_then(|(provider, _)| provider_kind(provider, kinds))
        .map(|kind| kind.fallback_context_limit)
}

/// Whether a config layer sets `general.theme`. Checked on the text
/// rather than on the merge result, so a project file that repeats the
/// user's own theme is still reported.
fn declares_theme(text: &str) -> bool {
    toml::from_str::<FileConfig>(text)
        .ok()
        .and_then(|parsed| parsed.general)
        .is_some_and(|general| general.theme.is_some())
}

fn read_config_file(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading config {}", path.display())),
    }
}

/// Lay a parsed layer's set fields over the inherited ones; an omitted
/// field keeps whatever the layer below it resolved to.
macro_rules! overlay {
    ($current:expr, $incoming:expr, $($field:ident),+ $(,)?) => {{
        let current = $current;
        let incoming = $incoming;
        $(
            if incoming.$field.is_some() {
                current.$field = incoming.$field;
            }
        )+
    }};
}

fn merge_file(base: FileConfig, text: &str, origin: &Path) -> anyhow::Result<FileConfig> {
    let parsed: FileConfig =
        toml::from_str(text).with_context(|| format!("parsing config {}", origin.display()))?;
    validate_file(&parsed, origin)?;
    let mut merged = base;
    if let Some(general) = parsed.general {
        overlay!(
            merged.general.get_or_insert_with(GeneralConfig::default),
            general,
            model,
            reasoning,
            theme,
        );
    }
    if let Some(providers) = parsed.providers {
        let map = merged.providers.get_or_insert_with(HashMap::new);
        for (name, provider) in providers {
            overlay!(
                map.entry(name).or_default(),
                provider,
                base_url,
                api_key,
                auth,
            );
        }
    }
    if let Some(agent) = parsed.agent {
        overlay!(
            merged.agent.get_or_insert_with(AgentLayer::default),
            agent,
            max_iterations,
        );
    }
    if let Some(compaction) = parsed.compaction {
        overlay!(
            merged
                .compaction
                .get_or_insert_with(CompactionLayer::default),
            compaction,
            threshold,
        );
    }
    if let Some(subagents) = parsed.subagents {
        overlay!(
            merged.subagents.get_or_insert_with(SubagentLayer::default),
            subagents,
            max_concurrent,
            max_depth,
            background_tool_timeout_ms,
        );
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
    if let Some(agent) = &config.agent {
        anyhow::ensure!(
            agent.max_iterations != Some(0),
            "{}: agent.max_iterations must be at least 1",
            origin.display()
        );
    }
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
        validate_providers(providers, origin, PROVIDERS)?;
    }
    Ok(())
}

fn validate_providers(
    providers: &HashMap<String, ProviderConfig>,
    origin: &Path,
    kinds: &[ProviderKind],
) -> anyhow::Result<()> {
    for (name, provider) in providers {
        let Some(kind) = provider_kind(name, kinds) else {
            anyhow::bail!("{}: unsupported provider {name:?}", origin.display());
        };
        validate_provider_value(origin, kind.name, "auth", &provider.auth, kind.auth_values)?;
    }
    Ok(())
}

fn validate_provider_value(
    origin: &Path,
    provider: &str,
    field: &str,
    value: &Option<String>,
    allowed: &[&str],
) -> anyhow::Result<()> {
    let Some(value) = value.as_deref() else {
        return Ok(());
    };
    anyhow::ensure!(
        !allowed.is_empty(),
        "{}: providers.{provider}.{field} is not supported",
        origin.display()
    );
    anyhow::ensure!(
        allowed.contains(&value),
        "{}: providers.{provider}.{field} must be {}",
        origin.display(),
        allowed
            .iter()
            .map(|allowed| format!("`{allowed}`"))
            .collect::<Vec<_>>()
            .join(" or ")
    );
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
        tools: Option<Vec<String>>,
    }
    let fm: Frontmatter = toml::from_str(&frontmatter).context("invalid agent frontmatter")?;
    if fm.disabled == Some(true) {
        return Ok(None);
    }
    if let Some(tools) = &fm.tools {
        let known = crate::tools::child_tool_names();
        for tool in tools {
            anyhow::ensure!(
                known.contains(&tool.as_str()),
                "unknown tool {tool:?} in agent allowlist (known: {})",
                known.join(", ")
            );
        }
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
        tools: fm.tools,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hypothetical provider, written the way a real one is: one row.
    const ACME: ProviderKind = ProviderKind {
        name: "acme",
        api_key_env: "ILAR_ACME_API_KEY",
        auth_values: &["api_key"],
        fallback_context_limit: 64_000,
        reaches: |settings, _| settings.api_key.is_some(),
        build: |_, _| None,
    };

    fn with_acme() -> Vec<ProviderKind> {
        PROVIDERS.iter().copied().chain([ACME]).collect()
    }

    fn provider_section(entries: [(&str, ProviderConfig); 1]) -> HashMap<String, ProviderConfig> {
        entries
            .into_iter()
            .map(|(name, config)| (name.to_string(), config))
            .collect()
    }

    #[test]
    fn one_table_row_is_all_a_new_provider_needs() {
        let kinds = with_acme();

        // Resolution: the row's environment variable supplies the key,
        // and the provider appears alongside the built-in ones.
        let env = Loader::with_env(vec![("ILAR_ACME_API_KEY", "acme-key".into())]);
        let resolved = resolve_providers(&FileConfig::default(), &env, &kinds);
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved["acme"].api_key.as_deref(), Some("acme-key"));
        assert_eq!(resolved["openai"].api_key, None);

        // Fallback windows: the row's own number, not a match arm.
        assert_eq!(fallback_context_limit("acme/q-1", &kinds), Some(64_000));
        assert_eq!(fallback_context_limit("nope/q-1", &kinds), None);

        // Validation: accepted values pass, everything else is refused
        // in the wording every provider shares.
        let origin = Path::new("ilar.toml");
        let good = provider_section([(
            "acme",
            ProviderConfig {
                auth: Some("api_key".into()),
                ..ProviderConfig::default()
            },
        )]);
        validate_providers(&good, origin, &kinds).unwrap();
        let bad_auth = provider_section([(
            "acme",
            ProviderConfig {
                auth: Some("mystery".into()),
                ..ProviderConfig::default()
            },
        )]);
        assert_eq!(
            validate_providers(&bad_auth, origin, &kinds)
                .unwrap_err()
                .to_string(),
            "ilar.toml: providers.acme.auth must be `api_key`"
        );
        // Still unknown while the row is absent from the table.
        assert_eq!(
            validate_providers(&good, origin, PROVIDERS)
                .unwrap_err()
                .to_string(),
            "ilar.toml: unsupported provider \"acme\""
        );
    }

    #[test]
    fn model_listing_asks_the_table_which_rows_are_reachable() {
        let keyed = |name: &str| ProviderConfigResolved {
            base_url: None,
            api_key: Some(format!("{name}-key")),
            auth: None,
        };
        let providers: HashMap<String, ProviderConfigResolved> = ["openai", "zai"]
            .into_iter()
            .map(|name| (name.to_string(), keyed(name)))
            .collect();

        assert!(
            available_models_in(&providers, PROVIDERS)
                .iter()
                .any(|model| model.provider == "zai")
        );

        // Silence one row and only that provider's models disappear.
        let mut muted = PROVIDERS.to_vec();
        muted
            .iter_mut()
            .find(|kind| kind.name == "zai")
            .expect("z.ai is a known provider")
            .reaches = |_, _| false;
        let listed = available_models_in(&providers, &muted);
        assert!(listed.iter().all(|model| model.provider == "openai"));
        assert!(!listed.is_empty());
    }
}
