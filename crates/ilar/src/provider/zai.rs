//! z.ai GLM provider: the shared chat-completions wire ([`super::chat`])
//! with z.ai's own endpoint, body field and always-present credential.

use super::chat::{ChatDialect, ChatProvider};
use super::request::Request;
use super::{EventStream, Provider};

/// Coding-plan billing lives under /api/coding/paas/v4; the plain
/// /api/paas/v4 endpoint requires a separate balance.
const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

/// The z.ai dialect of the chat-completions provider. A named type
/// rather than a constructor so the provider a caller holds says which
/// service it talks to.
#[derive(Clone)]
pub struct ZaiProvider(ChatProvider);

impl ZaiProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self(ChatProvider::new(ChatDialect::zai(
            api_key,
            base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into()),
        )))
    }

    /// Test accessor for the wire body (prefix-stability checks).
    pub fn wire_body_for_test(&self, req: &Request) -> serde_json::Value {
        self.0.wire_body_for_test(req)
    }
}

impl Provider for ZaiProvider {
    fn provider_prefix(&self) -> Option<&'static str> {
        self.0.provider_prefix()
    }

    fn stream(&self, req: Request) -> anyhow::Result<EventStream> {
        self.0.stream(req)
    }
}
