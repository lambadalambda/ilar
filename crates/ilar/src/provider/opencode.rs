//! OpenCode Zen and Go: one gateway, two wires.
//!
//! The gateway fronts every model on exactly one of the OpenAI wires —
//! the GPT, Grok and Muse Spark families on `/responses`, everything
//! else on `/chat/completions` — and answers a bare 500 on the other
//! (probed live on 2026-09-03 across both catalogs). The catalog row
//! records which ([`ModelAccess::OpenCodeResponses`] or
//! [`ModelAccess::OpenCodeChat`]), and this provider holds one client
//! per wire and routes by it. An id the catalog does not know goes to
//! chat-completions, the wire most of the catalog is on.
//!
//! Zen (`opencode/<id>`) is pay-as-you-go; Go (`opencode-go/<id>`) is a
//! subscription with usage caps counted in dollars at the listed prices.
//! One key serves both. Every request carries `x-opencode-session` and
//! friends ([`Affinity::OpenCode`]): the gateway keys on the session
//! and, from 2026-09-06, refuses requests that do not name one.

use super::chat::{ChatDialect, ChatProvider};
use super::openai::OpenAIProvider;
use super::request::Request;
use super::transport::Affinity;
use super::{EventStream, Provider};
use crate::model::ModelAccess;

/// Provider prefix for OpenCode Zen, as opencode itself spells it.
pub const ZEN_PREFIX: &str = "opencode";
/// Provider prefix for OpenCode Go.
pub const GO_PREFIX: &str = "opencode-go";

const ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";
const GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

#[derive(Clone)]
pub struct OpenCodeProvider {
    prefix: &'static str,
    chat: ChatProvider,
    responses: OpenAIProvider,
}

impl OpenCodeProvider {
    /// The Zen gateway; `base_url` overrides the published one.
    pub fn zen(api_key: String, base_url: Option<String>) -> Self {
        Self::new(
            ZEN_PREFIX,
            api_key,
            base_url.unwrap_or_else(|| ZEN_BASE_URL.into()),
        )
    }

    /// The Go gateway; `base_url` overrides the published one.
    pub fn go(api_key: String, base_url: Option<String>) -> Self {
        Self::new(
            GO_PREFIX,
            api_key,
            base_url.unwrap_or_else(|| GO_BASE_URL.into()),
        )
    }

    fn new(prefix: &'static str, api_key: String, base_url: String) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            prefix,
            chat: ChatProvider::new(ChatDialect::opencode(
                prefix,
                api_key.clone(),
                base_url.clone(),
            )),
            responses: OpenAIProvider::new(api_key, Some(base_url))
                .with_prefix(prefix)
                .with_affinity(Affinity::OpenCode),
        }
    }

    /// The client the catalog row sends this request to.
    fn wire(&self, model: &str) -> &dyn Provider {
        match crate::model::find(model).map(|row| row.access) {
            Some(ModelAccess::OpenCodeResponses) => &self.responses,
            _ => &self.chat,
        }
    }
}

impl Provider for OpenCodeProvider {
    fn provider_prefix(&self) -> Option<&'static str> {
        Some(self.prefix)
    }

    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        self.wire(&req.model).stream(req)
    }
}
