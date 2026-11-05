//! models: read-only listing of the models available to this session,
//! with pricing and reasoning variants — lets agents pick a cheap model
//! when delegating tasks.

use super::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};

pub struct ModelsTool {
    models: Vec<&'static crate::model::ModelInfo>,
}

impl ModelsTool {
    pub fn new(models: Vec<&'static crate::model::ModelInfo>) -> Self {
        Self { models }
    }
}

fn context_label(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else {
        format!("{}k", tokens / 1_000)
    }
}

fn pricing_label(model: &crate::model::ModelInfo) -> String {
    let full_id = model.full_id();
    // A configured endpoint has no price to quote, so it says where it is
    // instead — which is the thing worth knowing about a model whose id
    // was invented by whoever wrote the config.
    if let Some(origin) = model.origin() {
        origin.to_string()
    } else if crate::model::plan_billed(&full_id) {
        "subscription plan".to_string()
    } else {
        match crate::model::pricing_for(&full_id) {
            Some(pricing) if pricing.input == 0.0 && pricing.output == 0.0 => "free".to_string(),
            Some(pricing) => format!("${}/{} per Mtok in/out", pricing.input, pricing.output),
            None => "pricing unknown".to_string(),
        }
    }
}

fn variant_ids(model: &crate::model::ModelInfo) -> Vec<&'static str> {
    model.variants().iter().map(|variant| variant.id).collect()
}

fn describe(model: &crate::model::ModelInfo) -> String {
    let variants = variant_ids(model);
    let reasoning = if variants.is_empty() {
        String::new()
    } else {
        format!(" · reasoning: {}", variants.join("/"))
    };
    format!(
        "{} · ctx {} · {}{reasoning}",
        model.full_id(),
        context_label(model.context_limit),
        pricing_label(model)
    )
}

/// One line per model: what the tool answers and `ilar models` prints.
pub fn listing(models: &[&crate::model::ModelInfo]) -> String {
    if models.is_empty() {
        return "no models available".to_string();
    }
    models
        .iter()
        .map(|model| describe(model))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The same listing for a script: `ilar models --json`.
pub fn listing_json(models: &[&crate::model::ModelInfo]) -> serde_json::Value {
    models
        .iter()
        .map(|model| {
            serde_json::json!({
                "id": model.full_id(),
                "name": model.name,
                "context": model.context_limit,
                "pricing": pricing_label(model),
                "reasoning": variant_ids(model),
            })
        })
        .collect()
}

impl Tool for ModelsTool {
    fn name(&self) -> &'static str {
        "models"
    }

    fn description(&self) -> &'static str {
        "List the models available in this session (context window, \
         pricing, reasoning variants). Useful for choosing a cheaper or \
         faster model when delegating with the task tool."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn run(&self, _input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let listing = listing(&self.models);
        Box::pin(async move { ToolOutput::text(listing) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_shows_pricing_plan_and_variants() {
        let models: Vec<&'static crate::model::ModelInfo> = ["zai/glm-5.3", "zai/glm-4.7"]
            .iter()
            .map(|id| crate::model::find(id).expect("catalog model"))
            .collect();
        let lines: Vec<String> = models.iter().map(|model| describe(model)).collect();
        assert!(
            lines[0].contains("zai/glm-5.3")
                && lines[0].contains("subscription plan")
                && lines[0].contains("reasoning: low/high/max"),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("zai/glm-4.7") && lines[1].contains("$0.6/2.2 per Mtok"),
            "{lines:?}"
        );
    }

    /// The CLI's `--json`: one object per model, the id first among
    /// equals, for a script that only wants to pick one.
    #[test]
    fn json_listing_names_id_context_pricing_and_variants() {
        let models = vec![crate::model::find("zai/glm-5.3").expect("catalog model")];
        let json = listing_json(&models);
        assert_eq!(json[0]["id"], "zai/glm-5.3");
        assert_eq!(json[0]["context"], models[0].context_limit);
        assert_eq!(json[0]["pricing"], "subscription plan");
        assert_eq!(
            json[0]["reasoning"],
            serde_json::json!(["low", "high", "max"])
        );
        assert!(json[0]["name"].is_string(), "{json}");
    }

    #[test]
    fn text_listing_is_one_described_line_per_model() {
        let models: Vec<&'static crate::model::ModelInfo> = ["zai/glm-5.3", "zai/glm-4.7"]
            .iter()
            .map(|id| crate::model::find(id).expect("catalog model"))
            .collect();
        assert_eq!(
            listing(&models),
            format!("{}\n{}", describe(models[0]), describe(models[1]))
        );
        assert_eq!(listing(&[]), "no models available");
    }

    /// A configured endpoint is listed like any other model, with where
    /// it lives standing in for the price it has none of.
    #[test]
    fn listing_shows_a_configured_endpoints_host_instead_of_pricing() {
        let rows = crate::model::register_runtime(&[crate::model::RuntimeModel {
            provider: crate::model::CUSTOM_PROVIDER.to_string(),
            id: "qwen-listed-by-the-tool".into(),
            name: "Qwen".into(),
            context_limit: 32_768,
            output_limit: 8_192,
            vision: false,
            origin: "localhost:8080".into(),
        }]);
        assert_eq!(
            describe(rows[0]),
            "custom/qwen-listed-by-the-tool · ctx 32k · localhost:8080"
        );
    }
}
