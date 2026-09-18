//! `[endpoints.<name>]`: an OpenAI-compatible server whose models are
//! discovered from its `/models` listing rather than declared one by
//! one. Each discovered model is addressed as `<name>/<id>`.
//!
//! The listing is fetched when configuration loads, with a short
//! timeout, and kept under the state directory: a server that is down
//! at start still yields the models it served last time, with a
//! warning, rather than none.

use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use crate::model::RuntimeModel;

/// The window assumed for a discovered model whose listing says nothing
/// about it: llama.cpp's and vLLM's listings do not.
pub const DEFAULT_CONTEXT: u64 = 32_768;

/// How long the listing may take before the cache is used instead.
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Everything up to `/models` and `/chat/completions`, e.g.
    /// `http://127.0.0.1:13305/api/v1`.
    pub base_url: String,
    /// Sent as `Authorization: Bearer …` on the listing and every
    /// request; none for a local server.
    pub api_key: Option<String>,
    /// The window for a model whose listing does not say. A listing
    /// may say twice — `context_length` and `max_context_window` — and
    /// the two disagree in both directions: Lemonade configures a
    /// window below the model's maximum, llama.cpp reports the total
    /// across its parallel slots above one request's share. The
    /// smaller of the two is the one a request can use.
    pub context: Option<u64>,
    /// Reply budget carved out of the window; a quarter when unstated.
    pub output: Option<u64>,
    /// Vision for models whose listing does not say (Lemonade's `vision`
    /// label does).
    #[serde(default)]
    pub vision: bool,
    /// How much of a model's thinking goes back to it — `all`, `turn`
    /// or `off`; `[general]`'s setting when unstated. `off` is for a
    /// server that streams reasoning and refuses it as input.
    pub replay_thinking: Option<crate::provider::chat::ThinkingReplay>,
    /// Only these ids, when set. Otherwise every chat model listed.
    pub models: Option<Vec<String>>,
    /// Body fields merged into every request to this endpoint.
    pub options: Option<serde_json::Value>,
}

impl Endpoint {
    /// The wire for one discovered model, under the endpoint's own
    /// prefix — the registered row's, which is the leaked static name.
    /// Its own `replay_thinking` when it has one; `[general]`'s is
    /// applied by whoever builds the provider.
    pub fn dialect(
        &self,
        model_id: &str,
        prefix: &'static str,
        vision: bool,
    ) -> crate::provider::chat::ChatDialect {
        let dialect = crate::provider::chat::ChatDialect::custom(
            self.base_url.clone(),
            model_id.to_string(),
            self.api_key.clone(),
            vision,
        )
        .with_prefix(prefix)
        .with_replay_thinking(self.replay_thinking);
        match &self.options {
            Some(options) => dialect.with_options(options.clone()),
            None => dialect,
        }
    }
}

/// One entry of a `/models` listing, as much of it as matters.
#[derive(Debug, Deserialize)]
struct Listed {
    id: String,
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    downloaded: Option<bool>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_context_window: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Listing {
    data: Vec<Listed>,
}

/// The rows a listing yields for endpoint `name`. A listing with labels
/// (Lemonade's) contributes its `chat` models, with their own window
/// and vision; one without (llama.cpp, vLLM) contributes everything,
/// on the endpoint's defaults. Ids not in `models` are left out when
/// that list is set.
pub(crate) fn discovered_rows(
    name: &str,
    endpoint: &Endpoint,
    listing: &str,
) -> anyhow::Result<Vec<RuntimeModel>> {
    let listing: Listing = serde_json::from_str(listing).context("parsing the model listing")?;
    let origin = super::toml::endpoint_origin(&endpoint.base_url);
    let mut rows = Vec::new();
    for model in listing.data {
        if model.downloaded == Some(false) {
            continue;
        }
        if let Some(labels) = &model.labels
            && !labels.iter().any(|label| label == "chat")
        {
            continue;
        }
        if let Some(wanted) = &endpoint.models
            && !wanted.iter().any(|id| id == &model.id)
        {
            continue;
        }
        let stated = [model.context_length, model.max_context_window]
            .into_iter()
            .flatten()
            .filter(|context| *context > 0)
            .min();
        let context = stated.or(endpoint.context).unwrap_or(DEFAULT_CONTEXT);
        let vision = model
            .labels
            .as_ref()
            .map(|labels| labels.iter().any(|label| label == "vision"))
            .unwrap_or(endpoint.vision);
        rows.push(RuntimeModel {
            provider: name.to_string(),
            id: model.id.clone(),
            name: model.id,
            context_limit: context,
            output_limit: endpoint
                .output
                .unwrap_or(context / super::toml::DEFAULT_OUTPUT_FRACTION)
                .min(context),
            vision,
            origin: origin.clone(),
        });
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rows)
}

/// Discover an endpoint's models: the live listing when the server
/// answers in time, the cached one otherwise. Warnings say which.
pub(crate) fn discover(
    name: &str,
    endpoint: &Endpoint,
    state_dir: &Path,
) -> (Vec<RuntimeModel>, Vec<String>) {
    let mut warnings = Vec::new();
    let cache = state_dir.join("endpoints").join(format!("{name}.json"));
    let listing = match fetch_listing(&endpoint.base_url, endpoint.api_key.as_deref()) {
        Ok(listing) => {
            if let Err(error) = std::fs::create_dir_all(cache.parent().unwrap_or(state_dir))
                .and_then(|()| std::fs::write(&cache, &listing))
            {
                warnings.push(format!("endpoints.{name}: listing not cached: {error}"));
            }
            listing
        }
        Err(error) => match std::fs::read_to_string(&cache) {
            Ok(cached) => {
                warnings.push(format!(
                    "endpoints.{name}: {error:#}; using the models it listed last time"
                ));
                cached
            }
            Err(_) => {
                warnings.push(format!(
                    "endpoints.{name}: {error:#}; no models known from it yet"
                ));
                return (Vec::new(), warnings);
            }
        },
    };
    match discovered_rows(name, endpoint, &listing) {
        Ok(rows) => {
            if rows.is_empty() {
                warnings.push(format!("endpoints.{name}: the listing has no chat models"));
            }
            (rows, warnings)
        }
        Err(error) => {
            warnings.push(format!("endpoints.{name}: {error:#}"));
            (Vec::new(), warnings)
        }
    }
}

/// `GET {base_url}/models`, from a thread of its own: configuration
/// loads synchronously, sometimes inside an async runtime, and a request
/// needs one of its own either way.
fn fetch_listing(base_url: &str, api_key: Option<&str>) -> anyhow::Result<String> {
    let url = format!("{base_url}/models");
    let api_key = api_key.map(str::to_string);
    let worker = std::thread::spawn(move || -> anyhow::Result<String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let client = reqwest::Client::builder()
                .timeout(DISCOVERY_TIMEOUT)
                .build()?;
            let mut request = client.get(&url);
            if let Some(key) = api_key {
                request = request.bearer_auth(key);
            }
            let response = request.send().await.context("listing models")?;
            anyhow::ensure!(
                response.status().is_success(),
                "listing models: HTTP {}",
                response.status()
            );
            Ok(response.text().await?)
        })
    });
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("the discovery thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> Endpoint {
        Endpoint {
            base_url: "http://127.0.0.1:13305/api/v1".into(),
            api_key: None,
            context: Some(65_536),
            output: None,
            vision: false,
            replay_thinking: None,
            models: None,
            options: None,
        }
    }

    const LEMONADE: &str = r#"{"object":"list","data":[
        {"id":"Qwen3.8-27B-GGUF","labels":["chat","reasoning","vision"],"downloaded":true,"context_length":131072,"max_context_window":262144},
        {"id":"Two-Slots","labels":["chat"],"downloaded":true,"context_length":524288,"max_context_window":262144},
        {"id":"Z-Image-Turbo","labels":["image"],"downloaded":true},
        {"id":"RPG-HaloTales-V2","labels":["chat"],"downloaded":true},
        {"id":"Not-Here","labels":["chat"],"downloaded":false}
    ]}"#;

    #[test]
    fn a_labelled_listing_keeps_the_downloaded_chat_models_with_their_own_window() {
        let rows = discovered_rows("lemon", &endpoint(), LEMONADE).unwrap();
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["Qwen3.8-27B-GGUF", "RPG-HaloTales-V2", "Two-Slots"]);
        let qwen = &rows[0];
        assert_eq!(qwen.provider, "lemon");
        assert_eq!(qwen.context_limit, 131_072);
        assert_eq!(qwen.output_limit, 131_072 / 4);
        assert!(qwen.vision);
        assert_eq!(qwen.origin, "127.0.0.1:13305");
        // llama.cpp's total across two slots is not one request's window.
        assert_eq!(rows[2].context_limit, 262_144);
        let rpg = &rows[1];
        assert_eq!(rpg.context_limit, 65_536);
        assert!(!rpg.vision);
    }

    #[test]
    fn a_plain_listing_takes_everything_on_the_endpoint_defaults_and_the_allowlist_narrows() {
        let plain =
            r#"{"object":"list","data":[{"id":"b","object":"model"},{"id":"a","object":"model"}]}"#;
        let rows = discovered_rows(
            "local",
            &Endpoint {
                context: None,
                vision: true,
                ..endpoint()
            },
            plain,
        )
        .unwrap();
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(rows[0].context_limit, DEFAULT_CONTEXT);
        assert!(rows[0].vision);
        let only_b = Endpoint {
            models: Some(vec!["b".into()]),
            ..endpoint()
        };
        let rows = discovered_rows("local", &only_b, plain).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "b");
        assert!(discovered_rows("local", &endpoint(), "not json").is_err());
    }

    #[test]
    fn a_dead_server_falls_back_to_the_cache_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let dead = Endpoint {
            base_url: "http://127.0.0.1:9/api/v1".into(),
            ..endpoint()
        };
        let (rows, warnings) = discover("lemon", &dead, dir.path());
        assert!(rows.is_empty());
        assert!(warnings[0].contains("no models known"), "{warnings:?}");
        std::fs::create_dir_all(dir.path().join("endpoints")).unwrap();
        std::fs::write(dir.path().join("endpoints/lemon.json"), LEMONADE).unwrap();
        let (rows, warnings) = discover("lemon", &dead, dir.path());
        assert_eq!(rows.len(), 3);
        assert!(warnings[0].contains("listed last time"), "{warnings:?}");
    }
}
