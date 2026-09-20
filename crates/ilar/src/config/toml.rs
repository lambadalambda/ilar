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
    /// Whether the working directory's own AGENTS.md/CLAUDE.md is used.
    /// A project file is unauthenticated third-party input, so this can
    /// be turned off wholesale and opted into per launch instead.
    pub project_instructions: Option<bool>,
    /// Whether a bare launch offers this directory's last session,
    /// ghosted, for one key. On by default — continuing where you left
    /// off is what nearly every launch wants.
    pub resume_offer: Option<bool>,
    /// How much of a chat-wire model's thinking goes back to it: `all`
    /// (the default), `turn`, or `off`. A `[models.*]` or
    /// `[endpoints.*]` entry can override it for its own server.
    pub replay_thinking: Option<crate::provider::chat::ThinkingReplay>,
    /// Whether a terminal session remembers across sessions: the
    /// memory tools and the core block, per launch directory. On
    /// unless said otherwise; `false` leaves the store on disk alone
    /// and the model without the tools.
    pub memory: Option<bool>,
    /// Whether each prompt surfaces the notes it matches, after the
    /// message. On unless said otherwise; nothing without `memory`.
    pub memory_recall: Option<bool>,
    /// Whether a session opens with the newest notes' index lines
    /// beside the core block. On unless said otherwise.
    pub memory_index: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// "chatgpt" -> OAuth mode (run `ilar login`).
    pub auth: Option<String>,
    /// openai only: `false` leaves the `image_gen` tool out even
    /// though the credential would enable it.
    pub image_gen: Option<bool>,
}

/// A `[models.<name>]` entry: one OpenAI-compatible endpoint, reachable
/// as `custom/<name>`. The escape hatch for llamacpp, ollama and any
/// third-party service that speaks chat-completions.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomModel {
    /// Everything up to `/chat/completions`, e.g. `http://127.0.0.1:8080/v1`.
    pub base_url: String,
    /// Id the endpoint knows the model by, when that is not the section
    /// name (ollama's `llama3.3:70b`, for one).
    pub model: Option<String>,
    /// Local servers need none, and without one the request carries no
    /// Authorization header at all.
    pub api_key: Option<String>,
    /// The window this endpoint serves. Nothing else knows it — there is
    /// no catalog row to fall back on — so it is what input budgeting and
    /// compaction measure against.
    pub context: u64,
    /// Reply budget carved out of `context`; [`DEFAULT_OUTPUT_FRACTION`]
    /// of it when unstated.
    pub output: Option<u64>,
    /// Whether the model accepts image input.
    #[serde(default)]
    pub vision: bool,
    /// How much of the model's thinking goes back to it — `all`,
    /// `turn` or `off`; `[general]`'s setting when unstated. `off` is
    /// for a server that streams reasoning and refuses it as input.
    pub replay_thinking: Option<crate::provider::chat::ThinkingReplay>,
    /// Name shown in the picker and the models tool; the section name
    /// when unstated.
    pub display_name: Option<String>,
    /// Body fields merged into every request to this endpoint —
    /// `temperature`, `top_p`, whatever the server takes. Keys the wire
    /// owns are refused when the config is read.
    pub options: Option<serde_json::Value>,
}

/// Share of a declared window left for the reply when an entry states no
/// `output`. A local server's `context` is one budget shared by prompt
/// and reply, so some of it has to be held back; a quarter is the
/// conservative reading, in the same spirit as the catalog's windows.
pub(super) const DEFAULT_OUTPUT_FRACTION: u64 = 4;

impl CustomModel {
    /// The catalog row this entry publishes under `custom/<name>`.
    fn runtime(&self, name: &str) -> crate::model::RuntimeModel {
        crate::model::RuntimeModel {
            provider: crate::model::CUSTOM_PROVIDER.to_string(),
            id: name.to_string(),
            name: self
                .display_name
                .clone()
                .unwrap_or_else(|| name.to_string()),
            context_limit: self.context,
            output_limit: self
                .output
                .unwrap_or(self.context / DEFAULT_OUTPUT_FRACTION),
            vision: self.vision,
            origin: endpoint_origin(&self.base_url),
        }
    }

    /// The wire dialect this entry describes. Its own `replay_thinking`
    /// when it has one; `[general]`'s is applied by whoever builds the
    /// provider.
    fn dialect(&self, name: &str) -> crate::provider::chat::ChatDialect {
        let dialect = crate::provider::chat::ChatDialect::custom(
            self.base_url.clone(),
            self.model.clone().unwrap_or_else(|| name.to_string()),
            self.api_key.clone(),
            self.vision,
        )
        .with_replay_thinking(self.replay_thinking);
        match &self.options {
            Some(options) => dialect.with_options(options.clone()),
            None => dialect,
        }
    }
}

/// `host:port` of a base URL, for showing where a model is served from.
/// Never the URL itself: validation has already refused the ones without
/// a host, and echoing a raw URL would put any `user:pass@` in it on
/// screen for the sake of a provenance label.
pub(super) fn endpoint_origin(base_url: &str) -> String {
    let host = url::Url::parse(base_url)
        .ok()
        .and_then(|url| match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => Some(format!("{host}:{port}")),
            (Some(host), None) => Some(host.to_string()),
            (None, _) => None,
        });
    host.unwrap_or_else(|| "unknown host".to_string())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Max provider calls per user turn. A runaway-loop backstop, not a
    /// working limit: long-thinking models routinely need hundreds.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// The most tokens one response may produce; `0` sends no cap. A
    /// looping model otherwise generates until its context fills.
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u64,
    /// Install the sudo tool: one command as root, after the person
    /// has read it and said yes. Off unless asked for.
    #[serde(default)]
    pub sudo: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            max_output_tokens: default_max_output_tokens(),
            sudo: false,
        }
    }
}

fn default_max_iterations() -> usize {
    1_000
}

fn default_max_output_tokens() -> u64 {
    32_768
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfig {
    #[serde(default = "default_threshold")]
    pub threshold: f64,
}

/// `[cache_compact]`: compact an idle session once, just before its
/// last request leaves the provider's prompt cache, so the inevitable
/// cold re-read happens on a small context instead of a huge one. Off
/// by default — it fires an unattended provider request — and
/// user-scoped, since a cloned repository must not spend the user's
/// money on its own initiative.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheCompactConfig {
    pub enabled: bool,
    /// Seconds before the cache window closes at which to fire.
    pub margin_secs: u64,
    /// Below this many context tokens a cold read is pennies and a
    /// summary's fidelity is not worth spending.
    pub context_floor: u64,
    /// Cache window per provider prefix, in seconds, overriding the
    /// built-in guesses.
    pub ttl_secs: HashMap<String, u64>,
}

impl Default for CacheCompactConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            margin_secs: 60,
            context_floor: 150_000,
            ttl_secs: HashMap::new(),
        }
    }
}

impl CacheCompactConfig {
    /// The cache window assumed for a provider: what the providers
    /// document (OpenAI's `prompt_cache_options.ttl` is 30 minutes on
    /// GPT-5.6 and later) or, where nothing is documented, the five
    /// minutes implicit caches have been observed to hold.
    pub fn ttl_for(&self, provider: &str) -> std::time::Duration {
        let seconds = self
            .ttl_secs
            .get(provider)
            .copied()
            .unwrap_or(match provider {
                "openai" => 30 * 60,
                _ => 5 * 60,
            });
        std::time::Duration::from_secs(seconds)
    }

    pub fn margin(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.margin_secs)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CacheCompactLayer {
    enabled: Option<bool>,
    margin_secs: Option<u64>,
    context_floor: Option<u64>,
    ttl_secs: Option<HashMap<String, u64>>,
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
    models: Option<HashMap<String, CustomModel>>,
    /// `[endpoints.<name>]`: servers whose models are discovered.
    endpoints: Option<HashMap<String, super::endpoints::Endpoint>>,
    agent: Option<AgentLayer>,
    compaction: Option<CompactionLayer>,
    cache_compact: Option<CacheCompactLayer>,
    subagents: Option<SubagentLayer>,
    /// The assistant gateway's own settings, parsed by its crate: the
    /// core carries the table and decides only whose it is.
    gateway: Option<toml::Table>,
    /// `[channels.<name>]`, likewise.
    channels: Option<toml::Table>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AgentLayer {
    max_iterations: Option<usize>,
    max_output_tokens: Option<u64>,
    sudo: Option<bool>,
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
    // One OpenCode key serves both gateways, so both rows read the same
    // variable; a `[providers.*]` api_key still tells them apart.
    ProviderKind {
        name: crate::provider::opencode::ZEN_PREFIX,
        api_key_env: "ILAR_OPENCODE_API_KEY",
        auth_values: &[],
        fallback_context_limit: 128_000,
        reaches: opencode_reaches,
        build: opencode_zen_provider,
    },
    ProviderKind {
        name: crate::provider::opencode::GO_PREFIX,
        api_key_env: "ILAR_OPENCODE_API_KEY",
        auth_values: &[],
        fallback_context_limit: 128_000,
        reaches: opencode_reaches,
        build: opencode_go_provider,
    },
];

/// A base URL in its one canonical form, or why it is not one: an
/// `http://` or `https://` URL with a host, no query, no fragment,
/// and no trailing slash — so `{base}/chat/completions` joins with
/// exactly one separator wherever a wire does it. Checked when the
/// configuration is read and stored this way, so a mistake is the
/// file's to report at startup rather than a turn's to discover as a
/// 404 four calls in.
fn canonical_base_url(value: &str) -> Result<String, &'static str> {
    let url = url::Url::parse(value).map_err(|_| "must be an http:// or https:// URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("must be an http:// or https:// URL");
    }
    if !url.has_host() {
        return Err("must have a host");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not carry a query or a fragment");
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// Every `base_url` a layer carries, in its canonical form. Called after
/// validation, which is what makes the fallback unreachable.
fn canonical_base_urls(mut parsed: FileConfig) -> FileConfig {
    let canonical = |url: &str| canonical_base_url(url).unwrap_or_else(|_| url.to_string());
    if let Some(providers) = parsed.providers.as_mut() {
        for provider in providers.values_mut() {
            provider.base_url = provider.base_url.as_deref().map(canonical);
        }
    }
    if let Some(models) = parsed.models.as_mut() {
        for entry in models.values_mut() {
            entry.base_url = canonical(&entry.base_url);
        }
    }
    if let Some(endpoints) = parsed.endpoints.as_mut() {
        for entry in endpoints.values_mut() {
            entry.base_url = canonical(&entry.base_url);
        }
    }
    parsed
}

fn provider_kind<'a>(name: &str, kinds: &'a [ProviderKind]) -> Option<&'a ProviderKind> {
    kinds.iter().find(|kind| kind.name == name)
}

fn chatgpt_auth(settings: &ProviderConfigResolved) -> bool {
    settings.auth.as_deref() == Some("chatgpt")
}

/// Whether a provider has a credential at all: a key, or the OAuth mode
/// whose tokens live in the store instead.
fn configured(settings: &ProviderConfigResolved) -> bool {
    settings.api_key.is_some() || chatgpt_auth(settings)
}

/// Which of the three routes serves a model id, with what it needs.
enum Route<'a> {
    /// A `[models.<name>]` entry, addressed as `custom/<name>`.
    Configured(&'a CustomModel, &'a str),
    /// A row discovered from an `[endpoints.<name>]` listing.
    Discovered(
        &'a super::endpoints::Endpoint,
        &'static crate::model::ModelInfo,
        &'a str,
    ),
    /// A catalog row served by one of the built-in providers.
    Builtin(
        &'static ProviderKind,
        &'static crate::model::ModelInfo,
        &'a str,
    ),
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
    config: &Config,
    settings: &ProviderConfigResolved,
) -> Option<Box<dyn crate::provider::Provider>> {
    Some(Box::new(
        crate::provider::zai::ZaiProvider::new(
            settings.api_key.clone()?,
            settings.base_url.clone(),
        )
        .with_thinking_replay(config.general.replay_thinking),
    ))
}

/// Either OpenCode wire is reachable with a key; the row's provider name
/// has already picked the gateway by the time this is asked.
fn opencode_reaches(settings: &ProviderConfigResolved, access: crate::model::ModelAccess) -> bool {
    use crate::model::ModelAccess;
    settings.api_key.is_some()
        && matches!(
            access,
            ModelAccess::OpenCodeChat | ModelAccess::OpenCodeResponses
        )
}

fn opencode_zen_provider(
    config: &Config,
    settings: &ProviderConfigResolved,
) -> Option<Box<dyn crate::provider::Provider>> {
    Some(Box::new(
        crate::provider::opencode::OpenCodeProvider::zen(
            settings.api_key.clone()?,
            settings.base_url.clone(),
        )
        .with_thinking_replay(config.general.replay_thinking),
    ))
}

fn opencode_go_provider(
    config: &Config,
    settings: &ProviderConfigResolved,
) -> Option<Box<dyn crate::provider::Provider>> {
    Some(Box::new(
        crate::provider::opencode::OpenCodeProvider::go(
            settings.api_key.clone()?,
            settings.base_url.clone(),
        )
        .with_thinking_replay(config.general.replay_thinking),
    ))
}

/// Fully-resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub general: GeneralConfigResolved,
    pub providers: HashMap<String, ProviderConfigResolved>,
    /// `[models.<name>]` entries, by name. Their catalog rows are
    /// published to [`crate::model`]; these are the endpoints behind them.
    pub models: HashMap<String, CustomModel>,
    /// `[endpoints.<name>]` entries, by name; their discovered rows are
    /// in the catalog under `<name>/<id>`.
    pub endpoints: HashMap<String, super::endpoints::Endpoint>,
    pub agent: AgentConfig,
    pub compaction: CompactionConfig,
    pub cache_compact: CacheCompactConfig,
    pub subagents: SubagentConfig,
    /// `[gateway]` as written in the user's file, for the gateway crate
    /// to parse. User-scoped: a project may not point the user's
    /// assistant anywhere.
    pub gateway: Option<toml::Table>,
    /// `[channels.<name>]`, likewise.
    pub channels: Option<toml::Table>,
    /// Settings that parsed but were not honoured, one line each, for
    /// the frontend to show. A silently ignored setting reads as a bug
    /// in the program rather than a rule about the setting.
    pub warnings: Vec<String>,
    /// Catalog rows for `models`, in name order — the listing side of
    /// what `models` describes.
    custom_models: Vec<&'static crate::model::ModelInfo>,
    user_dir: PathBuf,
    project_dir: PathBuf,
    state_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GeneralConfigResolved {
    pub model: String,
    pub reasoning: Option<String>,
    pub theme: String,
    /// Trusting the working directory's context file is the default;
    /// `--no-project-instructions` overrides it for one launch.
    pub project_instructions: bool,
    /// Whether a bare launch offers this directory's last session as a
    /// ghost — see docs/interface.md ("Starting").
    pub resume_offer: bool,
    /// How much thinking goes back on the chat wire, for every model
    /// that replays it; see docs/configuration.md ("Thinking on the
    /// wire").
    pub replay_thinking: crate::provider::chat::ThinkingReplay,
    /// Whether a terminal session has a memory; see docs/sessions.md
    /// ("Memory that outlives a session").
    pub memory: bool,
    /// Whether each prompt surfaces the notes it matches.
    pub memory_recall: bool,
    /// Whether a session opens with the newest notes' index.
    pub memory_index: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderConfigResolved {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub auth: Option<String>,
    /// Whether the credential also brings image generation; only the
    /// openai provider reads it.
    pub image_gen: bool,
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

    /// An empty variable reads as unset: `HOME=""` would resolve the
    /// directories under the working directory just as an absent one
    /// does, and an empty key would send a blank Authorization header
    /// rather than say a credential is missing.
    fn env_lookup(&self, key: &str) -> Option<String> {
        if let Some(v) = self
            .env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
        {
            return Some(v).filter(|value| !value.is_empty());
        }
        if self.ignore_process_env {
            return None;
        }
        std::env::var(key).ok().filter(|value| !value.is_empty())
    }

    /// The directories, and nothing else: no config file is read and no
    /// endpoint is probed. What a subcommand that only touches the
    /// state directory needs — a broken `ilar.toml`, or an endpoint
    /// that takes three seconds to refuse, must not stand between
    /// someone and `ilar secret set`.
    pub fn resolve_dirs(&self) -> Dirs {
        let home = self.env_lookup("HOME");
        let under_home =
            |suffix: &str| PathBuf::from(home.clone().unwrap_or_else(|| ".".into())).join(suffix);
        let config = self
            .config_dir
            .clone()
            .or_else(|| self.env_lookup("ILAR_CONFIG_DIR").map(PathBuf::from));
        let state = self
            .state_dir
            .clone()
            .or_else(|| self.env_lookup("ILAR_STATE_DIR").map(PathBuf::from));
        Dirs {
            // A default that needed `HOME` and did not get it resolves
            // under the working directory; the flag is how a frontend
            // gets to refuse that instead of scattering state into
            // whatever project happens to be current.
            homeless: home.is_none() && (config.is_none() || state.is_none()),
            config: config.unwrap_or_else(|| under_home(".config/ilar")),
            state: state.unwrap_or_else(|| under_home(".local/state/ilar")),
        }
    }

    pub fn resolve(self) -> anyhow::Result<Config> {
        let Dirs {
            config: user_dir,
            state: state_dir,
            homeless,
        } = self.resolve_dirs();
        let project_dir = match self.project_dir.clone() {
            Some(project_dir) => project_dir,
            None => std::env::current_dir().context("resolving current project directory")?,
        };
        Config::load(user_dir, project_dir, state_dir, homeless, &self)
    }
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

/// Where configuration and state live, resolved from the environment
/// alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// `${ILAR_CONFIG_DIR:-~/.config/ilar}`.
    pub config: PathBuf,
    /// `${ILAR_STATE_DIR:-~/.local/state/ilar}`.
    pub state: PathBuf,
    /// A default fell back to the working directory because `HOME` was
    /// unset. See [`Self::require_home`].
    pub homeless: bool,
}

impl Dirs {
    /// Refuse the homeless case. With `HOME` unset and neither variable
    /// set, `~/.local/state/ilar` resolves to `./.local/state/ilar`:
    /// sessions, prompt history and the secret store land in whatever
    /// project happens to be current, silently, a different set per
    /// directory. A frontend says so instead.
    pub fn require_home(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.homeless,
            "HOME is not set, so config and state would land in the working directory: set HOME, or set ILAR_CONFIG_DIR and ILAR_STATE_DIR"
        );
        Ok(())
    }
}

impl Config {
    /// `guessed_state` is true when the state directory came from the
    /// `HOME`-less fallback: it is the working directory, and nothing
    /// is cached into it. See [`Dirs::require_home`].
    fn load(
        user_dir: PathBuf,
        project_dir: PathBuf,
        state_dir: PathBuf,
        guessed_state: bool,
        env: &Loader,
    ) -> anyhow::Result<Self> {
        // User file first, then project files layered on top. Theme stays user-scoped
        // so an in-app selection has the same effective value after restart.
        let mut merged = FileConfig::default();
        let user_path = user_dir.join("ilar.toml");
        if let Some(text) = read_config_file(&user_path)? {
            merged = merge_file(merged, &text, &user_path, Layer::User)?;
        }
        let user_theme = merged
            .general
            .as_ref()
            .and_then(|general| general.theme.clone());
        let user_project_instructions = merged
            .general
            .as_ref()
            .and_then(|general| general.project_instructions);
        let mut warnings = Vec::new();
        // Model endpoints and provider settings are user configuration,
        // never a project's: a cloned repository must not be able to
        // route the conversation — prompts, code, tool output — to an
        // endpoint it chose. For `[providers.*]` every field is such a
        // lever: base_url re-routes requests carrying the user's key,
        // api_key substitutes the repository's, auth flips OAuth mode.
        // The project layer sheds those tables before it is even
        // validated (`Layer::Project`): an ignored table may not refuse
        // startup either, or a cloned repository could still keep ilar
        // from opening with one bad line it was told is ignored.
        for path in [
            project_dir.join("ilar.toml"),
            project_dir.join(".ilar/ilar.toml"),
        ] {
            if let Some(text) = read_config_file(&path)? {
                for key in user_scoped_general_keys(&text) {
                    warnings.push(format!(
                        "{}: {key} is a user preference and is ignored in project config",
                        path.display()
                    ));
                }
                for table in declared_user_scoped_tables(&text) {
                    warnings.push(format!(
                        "{}: [{table}] is user configuration and is ignored in project config",
                        path.display()
                    ));
                }
                merged = merge_file(merged, &text, &path, Layer::Project)?;
            }
        }

        let secrets = crate::secrets::SecretStore::open(&state_dir);
        let providers = resolve_providers(&merged, env, Some(&secrets), PROVIDERS);

        // Configured models join the catalog before anything reads it:
        // `general.model` may name one, and so may its reasoning check.
        let models = merged.models.take().unwrap_or_default();
        let mut names = models.keys().cloned().collect::<Vec<_>>();
        names.sort();
        let rows = names
            .iter()
            .map(|name| models[name].runtime(name))
            .collect::<Vec<_>>();
        let mut custom_models = crate::model::register_runtime(&rows);

        // Discovered models join too, endpoint by endpoint in name order,
        // so `general.model` may name one of them as well.
        let endpoints = merged.endpoints.take().unwrap_or_default();
        let mut endpoint_names = endpoints.keys().cloned().collect::<Vec<_>>();
        endpoint_names.sort();
        let cache_dir = (!guessed_state).then_some(state_dir.as_path());
        for name in &endpoint_names {
            let (discovered, notes) = super::endpoints::discover(name, &endpoints[name], cache_dir);
            warnings.extend(notes);
            custom_models.extend(crate::model::register_runtime(&discovered));
        }

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
                // User-scoped like the theme: a project may not vote on
                // whether its own instructions are trusted.
                project_instructions: user_project_instructions.unwrap_or(true),
                resume_offer: merged
                    .general
                    .as_ref()
                    .and_then(|general| general.resume_offer)
                    .unwrap_or(true),
                replay_thinking: merged
                    .general
                    .as_ref()
                    .and_then(|general| general.replay_thinking)
                    .unwrap_or_default(),
                memory: merged
                    .general
                    .as_ref()
                    .and_then(|general| general.memory)
                    .unwrap_or(true),
                memory_recall: merged
                    .general
                    .as_ref()
                    .and_then(|general| general.memory_recall)
                    .unwrap_or(true),
                memory_index: merged
                    .general
                    .as_ref()
                    .and_then(|general| general.memory_index)
                    .unwrap_or(true),
            },
            providers,
            models,
            endpoints,
            custom_models,
            agent: AgentConfig {
                max_iterations: merged
                    .agent
                    .as_ref()
                    .and_then(|config| config.max_iterations)
                    .unwrap_or_else(default_max_iterations),
                max_output_tokens: merged
                    .agent
                    .as_ref()
                    .and_then(|config| config.max_output_tokens)
                    .unwrap_or_else(default_max_output_tokens),
                sudo: merged
                    .agent
                    .as_ref()
                    .and_then(|config| config.sudo)
                    .unwrap_or(false),
            },
            compaction: CompactionConfig {
                threshold: merged
                    .compaction
                    .and_then(|config| config.threshold)
                    .unwrap_or_else(default_threshold),
            },
            cache_compact: {
                let defaults = CacheCompactConfig::default();
                let layer = merged.cache_compact.unwrap_or_default();
                CacheCompactConfig {
                    enabled: layer.enabled.unwrap_or(defaults.enabled),
                    margin_secs: layer.margin_secs.unwrap_or(defaults.margin_secs),
                    context_floor: layer.context_floor.unwrap_or(defaults.context_floor),
                    ttl_secs: layer.ttl_secs.unwrap_or(defaults.ttl_secs),
                }
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
            gateway: merged.gateway,
            channels: merged.channels,
            user_dir,
            project_dir,
            state_dir,
            warnings,
        })
    }

    /// Markdown agents from the config dir merged over built-ins.
    pub fn agents(&self) -> anyhow::Result<Vec<AgentDefinition>> {
        self.agents_from(&self.user_dir)
    }

    /// The same, with another directory standing in for the user's —
    /// an assistant's home, whose `agents/` are its own.
    pub fn agents_from(&self, user_dir: &Path) -> anyhow::Result<Vec<AgentDefinition>> {
        let mut agents = AgentDefinition::builtins();
        for dir in [
            user_dir.join("agents"),
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

    /// The provider for "provider/model-id", or what to do about it.
    /// A malformed id, a provider nobody knows, an id that provider does
    /// not serve, a provider with no credential and a row that
    /// credential cannot reach are five different next steps, so they
    /// are five different messages — "no provider configured" answered
    /// all of them and diagnosed none.
    pub fn provider_result(
        &self,
        model: &str,
    ) -> anyhow::Result<Box<dyn crate::provider::Provider>> {
        match self.route(model)? {
            // A configured entry carries its own endpoint, so it needs
            // no row in the provider table to be reachable.
            Route::Configured(entry, model_id) => Ok(Box::new(
                crate::provider::chat::ChatProvider::new(entry.dialect(model_id))
                    .with_thinking_replay(self.general.replay_thinking),
            )),
            // A discovered model: the endpoint is the provider, and the
            // row registered at load is what says whether it sees
            // images.
            Route::Discovered(endpoint, row, model_id) => Ok(Box::new(
                crate::provider::chat::ChatProvider::new(endpoint.dialect(
                    model_id,
                    row.provider,
                    row.supports_vision(),
                ))
                .with_thinking_replay(self.general.replay_thinking),
            )),
            Route::Builtin(kind, row, model_id) => {
                let settings = self
                    .providers
                    .get(kind.name)
                    .filter(|settings| configured(settings))
                    .ok_or_else(|| {
                        anyhow::anyhow!(missing_credential_message(model, kind, &self.keyed()))
                    })?;
                // A credential the model's access does not accept — an
                // API-key row under `auth = "chatgpt"`, a model outside
                // the plan this key buys. The model picker never offers
                // these; a `--model` or a `general.model` can still name
                // one, and the request died on the wire.
                anyhow::ensure!(
                    (kind.reaches)(settings, row.access),
                    "{} cannot reach {model_id:?} with the credential it is configured with{}",
                    kind.name,
                    offered(&self.reachable_ids(kind.name))
                );
                (kind.build)(self, settings).ok_or_else(|| {
                    anyhow::anyhow!(missing_credential_message(model, kind, &self.keyed()))
                })
            }
        }
    }

    /// Refuse a model id this configuration cannot name: not
    /// `provider/model-id`, a provider nobody knows, or an id that is
    /// in no catalog row and no entry of its own. Asked where a session
    /// is decided as well as where its client is built — unknown ids
    /// used to be fatal only when a reasoning variant happened to be
    /// set, and otherwise got a client, the provider's fallback window
    /// on the meter, and a raw HTTP 400 on the first turn.
    pub fn ensure_model_known(&self, model: &str) -> anyhow::Result<()> {
        self.route(model).map(|_| ())
    }

    /// Which of the three routes serves a model id, or why none does.
    /// One decision: what says the id is resolvable is the same thing
    /// that hands back what resolves it, so the two cannot drift.
    fn route<'a>(&'a self, model: &'a str) -> anyhow::Result<Route<'a>> {
        let (provider_name, model_id) = crate::provider::resolve_model(model)?;
        if provider_name == crate::model::CUSTOM_PROVIDER {
            let entry = self.models.get(model_id).with_context(|| {
                format!("no [models.{model_id}] entry in your ilar.toml for model {model:?}")
            })?;
            return Ok(Route::Configured(entry, model_id));
        }
        if let Some(endpoint) = self.endpoints.get(provider_name) {
            let row = crate::model::find(model).with_context(|| {
                format!(
                    "endpoint {provider_name:?} does not serve a model {model_id:?}{}",
                    offered(&self.reachable_ids(provider_name))
                )
            })?;
            return Ok(Route::Discovered(endpoint, row, model_id));
        }
        let kind = provider_kind(provider_name, PROVIDERS)
            .ok_or_else(|| anyhow::anyhow!(self.unknown_provider_message(model, provider_name)))?;
        let row = crate::model::find(model).with_context(|| {
            format!(
                "{provider_name} has no model {model_id:?}: it is in no catalog row{}",
                offered(&self.reachable_ids(provider_name))
            )
        })?;
        Ok(Route::Builtin(kind, row, model_id))
    }

    /// Provider names a model id may carry: the built-in table, the
    /// prefix `[models.*]` publishes under, and every declared endpoint.
    fn unknown_provider_message(&self, model: &str, provider: &str) -> String {
        let mut known = PROVIDERS
            .iter()
            .map(|kind| kind.name.to_string())
            .collect::<Vec<_>>();
        if !self.models.is_empty() {
            known.push(crate::model::CUSTOM_PROVIDER.to_string());
        }
        known.extend(self.endpoints.keys().cloned());
        known.sort();
        format!(
            "no provider named {provider:?} (from model {model:?}); known providers: {}",
            known.join(", ")
        )
    }

    /// Providers this configuration has a credential for, in table order.
    fn keyed(&self) -> Vec<&'static str> {
        PROVIDERS
            .iter()
            .filter(|kind| self.providers.get(kind.name).is_some_and(configured))
            .map(|kind| kind.name)
            .collect()
    }

    /// Model ids this configuration can reach under one provider — what
    /// to offer when the id it was handed turns out not to exist.
    fn reachable_ids(&self, provider: &str) -> Vec<&'static str> {
        self.available_models()
            .into_iter()
            .filter(|model| model.provider == provider)
            .map(|model| model.id)
            .collect()
    }

    /// Chat-capable models this configuration can reach: the catalog rows
    /// its providers serve, then the endpoints it declared itself.
    pub fn available_models(&self) -> Vec<&'static crate::model::ModelInfo> {
        let mut models = available_models_in(&self.providers, PROVIDERS);
        models.extend(self.custom_models.iter().copied());
        models
    }

    /// User + project config dirs (agents searched in both).
    pub fn dirs(&self) -> (&Path, &Path) {
        (&self.user_dir, &self.project_dir)
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Deterministic config for tests: openai and zai keyed, no env.
    pub fn default_for_tests() -> Self {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfigResolved {
                base_url: None,
                api_key: Some("test-openai-key".into()),
                auth: None,
                image_gen: true,
            },
        );
        providers.insert(
            "zai".to_string(),
            ProviderConfigResolved {
                base_url: None,
                api_key: Some("test-zai-key".into()),
                auth: None,
                image_gen: true,
            },
        );
        Self {
            general: GeneralConfigResolved {
                model: "zai/glm-4.7".into(),
                reasoning: None,
                theme: "carbon".into(),
                project_instructions: true,
                resume_offer: true,
                replay_thinking: Default::default(),
                memory: true,
                memory_recall: true,
                memory_index: true,
            },
            agent: AgentConfig::default(),
            providers,
            models: HashMap::new(),
            custom_models: Vec::new(),
            compaction: CompactionConfig::default(),
            cache_compact: CacheCompactConfig::default(),
            subagents: SubagentConfig::default(),
            endpoints: HashMap::new(),
            gateway: None,
            channels: None,
            warnings: Vec::new(),
            user_dir: PathBuf::from("/nonexistent"),
            project_dir: PathBuf::from("/nonexistent"),
            state_dir: PathBuf::from("/nonexistent"),
        }
    }
}

impl crate::provider::ProviderResolver for Config {
    fn resolve_provider(&self, model: &str) -> anyhow::Result<crate::provider::ProviderHandle<'_>> {
        self.provider_result(model)
            .map(crate::provider::ProviderHandle::Owned)
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
/// provider's environment variable for the key, then the secret store
/// under that same name. Keys a provider does not support are rejected
/// by validation, so copying them is a no-op.
fn resolve_providers(
    merged: &FileConfig,
    env: &Loader,
    secrets: Option<&crate::secrets::SecretStore>,
    kinds: &[ProviderKind],
) -> HashMap<String, ProviderConfigResolved> {
    let stored = |name: &str| secrets.and_then(|store| store.value(name).ok().flatten());
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
                        .or_else(|| env.env_lookup(kind.api_key_env))
                        .or_else(|| stored(kind.api_key_env)),
                    auth: field(|config| config.auth.clone()),
                    image_gen: configured
                        .and_then(|config| config.image_gen)
                        .unwrap_or(true),
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

/// Where a provider's credential can come from, for an error that has
/// to tell someone which one a server refused. The candidates, not the
/// one that won: a resolved key is a secret, and its provenance is not
/// carried down to the wire alongside it.
/// Every provider whose key can come from the environment, as
/// `(name, variable)` in the order they are declared. The CLI's help
/// listed these by hand next to a function that already knew them,
/// so adding a provider meant remembering two places.
pub fn provider_key_variables() -> Vec<(&'static str, &'static str)> {
    PROVIDERS
        .iter()
        .map(|kind| (kind.name, kind.api_key_env))
        .collect()
}

pub(crate) fn credential_sources(provider: &str) -> String {
    if provider == crate::model::CUSTOM_PROVIDER {
        return "the api_key of the [models.*] entry that serves it".to_string();
    }
    match provider_kind(provider, PROVIDERS) {
        Some(kind) => format!(
            "{} (environment or secret store) or providers.{}.api_key in your ilar.toml",
            kind.api_key_env, kind.name
        ),
        None => format!("the api_key of the [endpoints.{provider}] entry that serves it"),
    }
}

/// Ids to try instead, as the tail of a sentence: at most a handful,
/// and nothing at all when the configuration can reach none of them —
/// "available: " followed by silence reads as a second failure.
const MAX_OFFERED_IDS: usize = 6;

fn offered(ids: &[&str]) -> String {
    if ids.is_empty() {
        return String::new();
    }
    let listed = ids
        .iter()
        .take(MAX_OFFERED_IDS)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let more = if ids.len() > MAX_OFFERED_IDS {
        ", …"
    } else {
        ""
    };
    format!(". Available now: {listed}{more}")
}

/// A provider the program knows, configured without a credential: the
/// variable to set, the file key that overrides it, and — because the
/// default model is not always the provider a fresh box has a key for —
/// which providers *are* configured.
fn missing_credential_message(model: &str, kind: &ProviderKind, keyed: &[&str]) -> String {
    let provider = kind.name;
    // The same candidates a refused credential names, spelled once.
    let mut message = format!(
        "model {model:?} needs the {provider} provider, which has no credential: set {}",
        credential_sources(provider)
    );
    if kind.auth_values.contains(&"chatgpt") {
        message.push_str(&format!(
            ", or run `ilar login` and set providers.{provider}.auth = \"chatgpt\""
        ));
    }
    match keyed {
        [] => message.push_str(". No provider is configured yet"),
        keyed => message.push_str(&format!(
            ". Configured now: {} — point general.model or --model at one of those",
            keyed.join(", ")
        )),
    }
    message
}

fn fallback_context_limit(model: &str, kinds: &[ProviderKind]) -> Option<u64> {
    crate::provider::resolve_model(model)
        .ok()
        .and_then(|(provider, _)| provider_kind(provider, kinds))
        .map(|kind| kind.fallback_context_limit)
}

/// The `[general]` keys a layer sets that resolve from user
/// configuration only. The theme is a preference an in-app selection
/// must survive; `project_instructions` decides whether the project
/// directory is trusted at all, and letting that directory answer its
/// own question would defeat the setting. Checked on the text rather
/// than on the merge result, so a project file that merely repeats the
/// user's own value is still reported.
fn user_scoped_general_keys(text: &str) -> Vec<&'static str> {
    let Some(general) = toml::from_str::<FileConfig>(text)
        .ok()
        .and_then(|parsed| parsed.general)
    else {
        return Vec::new();
    };
    [
        ("general.theme", general.theme.is_some()),
        (
            "general.project_instructions",
            general.project_instructions.is_some(),
        ),
    ]
    .into_iter()
    .filter(|(_, declared)| *declared)
    .map(|(key, _)| key)
    .collect()
}

/// The tables only the user's own file may set, in the order the
/// warnings name them. `[models]`, `[endpoints]` and `[providers]` route
/// the conversation; `[cache_compact]` fires unattended provider
/// requests; `[gateway]` and `[channels]` decide who may message the
/// assistant.
const USER_SCOPED_TABLES: &[&str] = &[
    "models",
    "endpoints",
    "providers",
    "cache_compact",
    "gateway",
    "channels",
];

/// Which user-scoped tables a project layer declares — read off the raw
/// TOML, so a table whose *contents* would not even parse as ours is
/// still reported as the ignored table it is. An empty `[models]` says
/// nothing and is not reported; the others are reported when present.
fn declared_user_scoped_tables(text: &str) -> Vec<&'static str> {
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return Vec::new();
    };
    USER_SCOPED_TABLES
        .iter()
        .copied()
        .filter(|name| match table.get(*name) {
            Some(toml::Value::Table(entries))
                if matches!(*name, "models" | "endpoints" | "providers") =>
            {
                !entries.is_empty()
            }
            Some(_) => true,
            None => false,
        })
        .collect()
}

/// Whose file a layer is. The project layer sheds the user-scoped
/// tables before validation: they are ignored, and an ignored table
/// must not be able to refuse startup.
enum Layer {
    User,
    Project,
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

fn merge_file(
    base: FileConfig,
    text: &str,
    origin: &Path,
    layer: Layer,
) -> anyhow::Result<FileConfig> {
    let parsed: FileConfig = match layer {
        Layer::User => {
            toml::from_str(text).with_context(|| format!("parsing config {}", origin.display()))?
        }
        Layer::Project => {
            let mut table: toml::Table = toml::from_str(text)
                .with_context(|| format!("parsing config {}", origin.display()))?;
            for name in USER_SCOPED_TABLES {
                table.remove(*name);
            }
            table
                .try_into()
                .with_context(|| format!("parsing config {}", origin.display()))?
        }
    };
    validate_file(&parsed, origin)?;
    // Validated, so every base URL canonicalises; stored that way, so
    // every wire joins its path onto one shape. Only the user layer
    // carries these tables by now.
    let parsed = canonical_base_urls(parsed);
    let mut merged = base;
    if let Some(general) = parsed.general {
        overlay!(
            merged.general.get_or_insert_with(GeneralConfig::default),
            general,
            model,
            reasoning,
            theme,
            project_instructions,
            resume_offer,
            replay_thinking,
            memory,
            memory_recall,
            memory_index,
        );
    }
    if parsed.gateway.is_some() {
        merged.gateway = parsed.gateway;
    }
    if parsed.channels.is_some() {
        merged.channels = parsed.channels;
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
                image_gen,
            );
        }
    }
    // Unlike `[providers.*]`, a model entry is replaced whole rather than
    // merged field by field: it describes one endpoint, and half an
    // endpoint description — a project's base_url still carrying the
    // user's api_key — is not something to hand a server. This is the
    // rule agent definitions follow, by name.
    if let Some(models) = parsed.models {
        merged
            .models
            .get_or_insert_with(HashMap::new)
            .extend(models);
    }
    if let Some(endpoints) = parsed.endpoints {
        merged
            .endpoints
            .get_or_insert_with(HashMap::new)
            .extend(endpoints);
    }
    if let Some(agent) = parsed.agent {
        overlay!(
            merged.agent.get_or_insert_with(AgentLayer::default),
            agent,
            max_iterations,
            max_output_tokens,
            sudo,
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
    if let Some(cache_compact) = parsed.cache_compact {
        overlay!(
            merged
                .cache_compact
                .get_or_insert_with(CacheCompactLayer::default),
            cache_compact,
            enabled,
            margin_secs,
            context_floor,
            ttl_secs,
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
            merge_file(FileConfig::default(), &source, path, Layer::User)?;
        }
        let updated = set_general_theme(&source, theme)?;
        let parsed = merge_file(FileConfig::default(), &updated, path, Layer::User)?;
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
    if let Some(models) = &config.models {
        validate_models(models, origin, PROVIDERS)?;
    }
    if let Some(endpoints) = &config.endpoints {
        validate_endpoints(endpoints, origin, PROVIDERS)?;
    }
    Ok(())
}

/// `[endpoints.<name>]`: the name becomes a model-id prefix, so it has
/// to be usable as one and must not shadow a provider; the URL has to
/// be one requests can go to.
fn validate_endpoints(
    endpoints: &HashMap<String, super::endpoints::Endpoint>,
    origin: &Path,
    kinds: &[ProviderKind],
) -> anyhow::Result<()> {
    let mut names = endpoints.keys().collect::<Vec<_>>();
    names.sort();
    for name in names {
        let entry = &endpoints[name];
        anyhow::ensure!(
            !name.is_empty() && !name.contains('/'),
            "{}: endpoint name {name:?} must be non-empty and contain no slash",
            origin.display()
        );
        anyhow::ensure!(
            provider_kind(name, kinds).is_none() && name != crate::model::CUSTOM_PROVIDER,
            "{}: endpoint name {name:?} must not be a provider name",
            origin.display()
        );
        canonical_base_url(&entry.base_url).map_err(|why| {
            anyhow::anyhow!("{}: endpoints.{name}.base_url {why}", origin.display())
        })?;
        anyhow::ensure!(
            entry.context != Some(0) && entry.output != Some(0),
            "{}: endpoints.{name}: context and output must be at least 1",
            origin.display()
        );
    }
    Ok(())
}

/// `[models.<name>]` entries: the name has to be usable as the second
/// half of a model id, and the endpoint has to be described well enough
/// to send a request to and to budget a context against. Checked per
/// file, in name order, so the same config always reports the same first
/// offender.
fn validate_models(
    models: &HashMap<String, CustomModel>,
    origin: &Path,
    kinds: &[ProviderKind],
) -> anyhow::Result<()> {
    let mut names = models.keys().collect::<Vec<_>>();
    names.sort();
    for name in names {
        let entry = &models[name];
        anyhow::ensure!(
            !name.is_empty(),
            "{}: a model name must not be empty",
            origin.display()
        );
        anyhow::ensure!(
            !name.contains('/'),
            "{}: model name {name:?} must not contain a slash",
            origin.display()
        );
        // `custom/openai` would read as the OpenAI provider's, and
        // `custom/custom` as nothing at all.
        anyhow::ensure!(
            provider_kind(name, kinds).is_none() && name != crate::model::CUSTOM_PROVIDER,
            "{}: model name {name:?} must not be a provider name",
            origin.display()
        );
        // The scheme is checked too: a URL reqwest cannot post to is the
        // same kind of mistake as a malformed one, and finding out
        // mid-turn is the thing this function exists to prevent.
        canonical_base_url(&entry.base_url)
            .map_err(|why| anyhow::anyhow!("{}: models.{name}.base_url {why}", origin.display()))?;
        anyhow::ensure!(
            entry.context > 0,
            "{}: models.{name}.context must be at least 1",
            origin.display()
        );
        if let Some(output) = entry.output {
            // Zero is not "no reservation": it would hand the whole
            // window to the prompt and compact only once the reply had
            // nowhere to go.
            anyhow::ensure!(
                output > 0,
                "{}: models.{name}.output must be at least 1",
                origin.display()
            );
            anyhow::ensure!(
                output < entry.context,
                "{}: models.{name}.output must be below its context",
                origin.display()
            );
        }
        if let Some(options) = &entry.options {
            let options = options.as_object().with_context(|| {
                format!(
                    "{}: models.{name}.options must be a table",
                    origin.display()
                )
            })?;
            // Refused here rather than at request time: a body field the
            // wire owns is a mistake to learn about at startup, not four
            // turns into a session. The list comes from the dialect that
            // will send them, so the two cannot drift apart.
            let conflicts = crate::provider::chat::reserved_conflicts(options);
            anyhow::ensure!(
                conflicts.is_empty(),
                "{}: models.{name}.options cannot override: {}",
                origin.display(),
                conflicts.join(", ")
            );
        }
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
        if let Some(url) = &provider.base_url {
            canonical_base_url(url).map_err(|why| {
                anyhow::anyhow!(
                    "{}: providers.{}.base_url {why}",
                    origin.display(),
                    kind.name
                )
            })?;
        }
        validate_provider_value(origin, kind.name, "auth", &provider.auth, kind.auth_values)?;
        if provider.image_gen.is_some() && kind.name != "openai" {
            anyhow::bail!(
                "{}: providers.{}.image_gen: only the openai provider generates images",
                origin.display(),
                kind.name
            );
        }
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
        let resolved = resolve_providers(&FileConfig::default(), &env, None, &kinds);
        assert_eq!(resolved.len(), PROVIDERS.len() + 1);
        assert_eq!(resolved["acme"].api_key.as_deref(), Some("acme-key"));
        assert_eq!(resolved["openai"].api_key, None);

        // The secret store answers under the same name, after the
        // environment.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::secrets::SecretStore::open(dir.path());
        store
            .set("ILAR_OPENAI_API_KEY", "", "stored-openai-key")
            .unwrap();
        store
            .set("ILAR_ACME_API_KEY", "", "stored-acme-key")
            .unwrap();
        let resolved = resolve_providers(&FileConfig::default(), &env, Some(&store), &kinds);
        assert_eq!(resolved["acme"].api_key.as_deref(), Some("acme-key"));
        assert_eq!(
            resolved["openai"].api_key.as_deref(),
            Some("stored-openai-key")
        );

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
        // Only openai generates images, so only openai takes the switch.
        let no_images = provider_section([(
            "acme",
            ProviderConfig {
                image_gen: Some(false),
                ..ProviderConfig::default()
            },
        )]);
        assert_eq!(
            validate_providers(&no_images, origin, &kinds)
                .unwrap_err()
                .to_string(),
            "ilar.toml: providers.acme.image_gen: only the openai provider generates images"
        );
    }

    /// Five ways a model id fails to reach a provider, five next steps.
    /// One message for all of them is what made `ilar --model glm-4.7`
    /// report a missing API key.
    #[test]
    fn an_unreachable_model_says_which_of_the_five_things_is_wrong() {
        // `Box<dyn Provider>` is not Debug, so the refusal is read as
        // its message rather than through unwrap_err.
        let refusal = |config: &Config, model: &str| {
            config
                .provider_result(model)
                .err()
                .unwrap_or_else(|| panic!("{model} should not resolve"))
                .to_string()
        };
        let config = Config::default_for_tests();

        // Not "provider/model-id" at all.
        let error = refusal(&config, "glm-4.7");
        assert!(error.contains("expected \"provider/model-id\""), "{error}");

        // A provider nobody knows, with the ones that are known named.
        let error = refusal(&config, "anthropic/claude");
        assert!(error.contains("no provider named \"anthropic\""), "{error}");
        assert!(error.contains("zai"), "{error}");

        // A known provider, no credential: the variable, the file key,
        // and the provider that *is* configured.
        let mut unkeyed = Config::default_for_tests();
        unkeyed.providers.remove("zai");
        let error = refusal(&unkeyed, "zai/glm-4.7");
        assert!(error.contains("ILAR_ZAI_API_KEY"), "{error}");
        assert!(error.contains("providers.zai.api_key"), "{error}");
        assert!(error.contains("Configured now: openai"), "{error}");

        // A credential with nothing configured at all says so instead.
        let bare = Config {
            providers: HashMap::new(),
            ..Config::default_for_tests()
        };
        let error = refusal(&bare, "zai/glm-4.7");
        assert!(error.contains("No provider is configured yet"), "{error}");

        // A keyed provider that does not serve that id, with ids it does.
        let error = refusal(&config, "zai/glm-9.9");
        assert!(error.contains("no model \"glm-9.9\""), "{error}");
        assert!(error.contains("Available now: glm-"), "{error}");

        // A credential the row's access does not accept: the ChatGPT
        // backend serves the Codex catalog, not the API-key one. The
        // picker never offers these, but a `--model` can still name one,
        // and the request used to die on the wire.
        let mut oauth = Config::default_for_tests();
        oauth.providers.insert(
            "openai".to_string(),
            ProviderConfigResolved {
                base_url: None,
                api_key: None,
                auth: Some("chatgpt".into()),
                image_gen: true,
            },
        );
        let error = refusal(&oauth, "openai/gpt-5.2");
        assert!(
            error.contains("cannot reach \"gpt-5.2\" with the credential"),
            "{error}"
        );
        // The same account reaches the Codex rows.
        assert!(
            oauth
                .provider_result(crate::model::CHATGPT_SUGGESTED_MODEL)
                .is_ok()
        );

        // And the reachable model still resolves.
        assert!(config.provider_result("zai/glm-4.7").is_ok());
    }

    /// A subcommand that only writes to the state directory resolves
    /// the directories and stops: no `ilar.toml` is parsed, so a broken
    /// one cannot stand between someone and `ilar secret set`.
    #[test]
    fn the_directories_resolve_without_reading_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ilar.toml"), "this is not toml {{{").unwrap();
        let loader = Loader::with_env(vec![("HOME", dir.path().display().to_string())])
            .config_dir(dir.path().to_path_buf());
        let dirs = loader.resolve_dirs();
        assert_eq!(dirs.config, dir.path());
        assert_eq!(dirs.state, dir.path().join(".local/state/ilar"));
        assert!(!dirs.homeless);
        dirs.require_home().unwrap();
        // The same loader reading the file is the thing that fails.
        assert!(loader.resolve().is_err());

        // Explicit variables need no home at all.
        let told = Loader::with_env(vec![
            ("ILAR_CONFIG_DIR", "/etc/ilar".into()),
            ("ILAR_STATE_DIR", "/var/ilar".into()),
        ])
        .resolve_dirs();
        assert_eq!(told.config, PathBuf::from("/etc/ilar"));
        assert!(!told.homeless);
        told.require_home().unwrap();

        // Without either, the defaults would land in the working
        // directory: refused rather than scattered per project.
        let homeless = Loader::with_env(Vec::new()).resolve_dirs();
        assert!(homeless.homeless);
        let error = homeless.require_home().unwrap_err().to_string();
        assert!(error.contains("HOME is not set"), "{error}");
        assert!(error.contains("ILAR_STATE_DIR"), "{error}");
    }

    /// `ilar login` is a next step, not a wall of variables: the openai
    /// row offers it, the others cannot.
    #[test]
    fn only_a_provider_with_oauth_offers_the_login_flow() {
        let openai = provider_kind("openai", PROVIDERS).expect("openai is a known provider");
        let zai = provider_kind("zai", PROVIDERS).expect("z.ai is a known provider");
        assert!(
            missing_credential_message("openai/gpt-5.2", openai, &["zai"]).contains("ilar login")
        );
        assert!(!missing_credential_message("zai/glm-4.7", zai, &[]).contains("ilar login"));

        // The same table answers "which key did the server refuse".
        assert_eq!(
            credential_sources("zai"),
            "ILAR_ZAI_API_KEY (environment or secret store) or providers.zai.api_key in your ilar.toml"
        );
        assert!(credential_sources("lemon").contains("[endpoints.lemon]"));
    }

    /// An empty list of alternatives says nothing rather than trailing
    /// off, and a long one is cut.
    #[test]
    fn offered_ids_are_a_handful_or_nothing() {
        assert_eq!(offered(&[]), "");
        assert_eq!(offered(&["a", "b"]), ". Available now: a, b");
        let many = ["a", "b", "c", "d", "e", "f", "g"];
        assert_eq!(offered(&many), ". Available now: a, b, c, d, e, f, …");
    }

    #[test]
    fn model_listing_asks_the_table_which_rows_are_reachable() {
        let keyed = |name: &str| ProviderConfigResolved {
            base_url: None,
            api_key: Some(format!("{name}-key")),
            auth: None,
            image_gen: true,
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
