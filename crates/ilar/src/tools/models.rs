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

fn describe(model: &crate::model::ModelInfo) -> String {
    let full_id = model.full_id();
    let pricing = if crate::model::plan_billed(&full_id) {
        "subscription plan".to_string()
    } else {
        match crate::model::pricing_for(&full_id) {
            Some(pricing) if pricing.input == 0.0 && pricing.output == 0.0 => "free".to_string(),
            Some(pricing) => format!("${}/{} per Mtok in/out", pricing.input, pricing.output),
            None => "pricing unknown".to_string(),
        }
    };
    let variants = model
        .variants()
        .iter()
        .map(|variant| variant.id)
        .collect::<Vec<_>>();
    let reasoning = if variants.is_empty() {
        String::new()
    } else {
        format!(" · reasoning: {}", variants.join("/"))
    };
    format!(
        "{full_id} · ctx {} · {pricing}{reasoning}",
        context_label(model.context_limit)
    )
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
        let listing = if self.models.is_empty() {
            "no models available".to_string()
        } else {
            self.models
                .iter()
                .map(|model| describe(model))
                .collect::<Vec<_>>()
                .join("\n")
        };
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
}
