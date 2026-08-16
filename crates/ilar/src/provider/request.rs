//! Neutral provider request type.

use serde::{Deserialize, Serialize};

use crate::session::ChatMessage;

/// A tool as advertised to the provider: name, description, JSON schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// One API call: everything the provider needs.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// Full model id, e.g. "zai/glm-4.7" (impls parse what they need).
    pub model: String,
    pub system_prompt: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    /// Opaque assistant content from provider-paused responses. Providers
    /// that emit continuations must replay them without persistence.
    pub continuations: Vec<serde_json::Value>,
    /// Provider passthrough options (temperature, etc.).
    pub options: serde_json::Value,
}

impl Request {
    pub fn with_model(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Default::default()
        }
    }
}

pub(super) fn merge_options(
    body: &mut serde_json::Map<String, serde_json::Value>,
    options: &serde_json::Value,
    reserved: &[&str],
) -> anyhow::Result<()> {
    let Some(options) = options.as_object() else {
        if options.is_null() {
            return Ok(());
        }
        anyhow::bail!("provider options must be an object or null");
    };
    let mut conflicts = options
        .keys()
        .filter(|key| reserved.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    conflicts.sort();
    if !conflicts.is_empty() {
        anyhow::bail!("provider options cannot override: {}", conflicts.join(", "));
    }
    body.extend(options.clone());
    Ok(())
}

/// Split "provider/model-id" into its parts.
pub fn resolve_model(model: &str) -> anyhow::Result<(&str, &str)> {
    let (provider, model_id) = model.split_once('/').ok_or_else(|| {
        anyhow::anyhow!("invalid model id {model:?}: expected \"provider/model-id\"")
    })?;
    if provider.is_empty() || model_id.is_empty() {
        anyhow::bail!("invalid model id {model:?}: empty provider or model part");
    }
    Ok((provider, model_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_provider_model_pairs() {
        assert_eq!(resolve_model("zai/glm-4.7").unwrap(), ("zai", "glm-4.7"));
        assert_eq!(
            resolve_model("openai/gpt-5.2").unwrap(),
            ("openai", "gpt-5.2")
        );
    }

    #[test]
    fn rejects_bare_model_ids() {
        assert!(resolve_model("glm-4.7").is_err());
        assert!(resolve_model("/glm").is_err());
        assert!(resolve_model("zai/").is_err());
        assert!(resolve_model("").is_err());
    }
}
