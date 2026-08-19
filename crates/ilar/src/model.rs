//! Maintained model metadata used by provider discovery and context accounting.

/// Upstream catalog snapshot used to maintain this module.
pub const CATALOG_SOURCE: &str = "https://models.dev/api.json";
pub const CATALOG_UPDATED: &str = "2026-08-15";
pub const DEFAULT_CONTEXT_SOURCE: &str = "https://github.com/openai/codex/pull/34009";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInfo {
    pub provider: &'static str,
    pub id: &'static str,
    pub name: &'static str,
    /// Conservative working window used by telemetry and compaction.
    pub context_limit: u64,
    /// Provider-advertised upper bound retained for future configuration.
    pub max_context_limit: u64,
    pub input_limit: u64,
    pub output_limit: u64,
    pub(crate) reasoning_summaries: bool,
    pub(crate) access: ModelAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelVariant {
    pub id: &'static str,
    pub name: &'static str,
}

/// USD per million tokens, from the models.dev snapshot above.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    /// Absent when the provider offers no cache-read rate; such tokens
    /// are billed at the input rate.
    pub cache_read: Option<f64>,
    /// Absent when cache writes are not billed separately.
    pub cache_write: Option<f64>,
}

impl ModelPricing {
    /// Estimated dollars for a usage total.
    pub fn cost(&self, usage: &crate::session::Usage) -> f64 {
        let per_token = |rate: f64| rate / 1_000_000.0;
        per_token(self.input) * usage.input_tokens as f64
            + per_token(self.output) * usage.output_tokens as f64
            + per_token(self.cache_read.unwrap_or(self.input))
                * usage.cache_read_input_tokens as f64
            + per_token(self.cache_write.unwrap_or(self.input))
                * usage.cache_creation_input_tokens as f64
    }
}

macro_rules! pricing {
    ($input:expr, $output:expr, $read:expr, $write:expr) => {
        ModelPricing {
            input: $input,
            output: $output,
            cache_read: $read,
            cache_write: $write,
        }
    };
}

/// API list prices keyed by (provider, model id). Coding-plan-only models
/// (subscription-billed: glm-5.2-highspeed, glm-5.3) are intentionally
/// absent — their effective token price depends on the plan, so the UI
/// shows tokens without dollars.
static PRICING: &[(&str, &str, ModelPricing)] = &[
    ("openai", "gpt-4.1", pricing!(2.0, 8.0, Some(0.5), None)),
    (
        "openai",
        "gpt-4.1-mini",
        pricing!(0.4, 1.6, Some(0.1), None),
    ),
    ("openai", "gpt-4o", pricing!(2.5, 10.0, Some(1.25), None)),
    (
        "openai",
        "gpt-4o-2024-08-06",
        pricing!(2.5, 10.0, Some(1.25), None),
    ),
    (
        "openai",
        "gpt-4o-2024-11-20",
        pricing!(2.5, 10.0, Some(1.25), None),
    ),
    (
        "openai",
        "gpt-4o-mini",
        pricing!(0.15, 0.6, Some(0.075), None),
    ),
    ("openai", "gpt-5", pricing!(1.25, 10.0, Some(0.125), None)),
    (
        "openai",
        "gpt-5-mini",
        pricing!(0.25, 2.0, Some(0.025), None),
    ),
    (
        "openai",
        "gpt-5-nano",
        pricing!(0.05, 0.4, Some(0.005), None),
    ),
    ("openai", "gpt-5-pro", pricing!(15.0, 120.0, None, None)),
    ("openai", "gpt-5.1", pricing!(1.25, 10.0, Some(0.125), None)),
    ("openai", "gpt-5.2", pricing!(1.75, 14.0, Some(0.175), None)),
    (
        "openai",
        "gpt-5.2-chat-latest",
        pricing!(1.75, 14.0, Some(0.175), None),
    ),
    ("openai", "gpt-5.2-pro", pricing!(21.0, 168.0, None, None)),
    (
        "openai",
        "gpt-5.3-chat-latest",
        pricing!(1.75, 14.0, Some(0.175), None),
    ),
    (
        "openai",
        "gpt-5.3-codex",
        pricing!(1.75, 14.0, Some(0.175), None),
    ),
    (
        "openai",
        "gpt-5.3-codex-spark",
        pricing!(1.75, 14.0, Some(0.175), None),
    ),
    ("openai", "gpt-5.4", pricing!(2.5, 15.0, Some(0.25), None)),
    (
        "openai",
        "gpt-5.4-mini",
        pricing!(0.75, 4.5, Some(0.075), None),
    ),
    (
        "openai",
        "gpt-5.4-nano",
        pricing!(0.2, 1.25, Some(0.02), None),
    ),
    ("openai", "gpt-5.4-pro", pricing!(30.0, 180.0, None, None)),
    ("openai", "gpt-5.5", pricing!(5.0, 30.0, Some(0.5), None)),
    ("openai", "gpt-5.5-pro", pricing!(30.0, 180.0, None, None)),
    (
        "openai",
        "gpt-5.6",
        pricing!(5.0, 30.0, Some(0.5), Some(6.25)),
    ),
    (
        "openai",
        "gpt-5.6-luna",
        pricing!(0.2, 1.2, Some(0.02), Some(0.25)),
    ),
    (
        "openai",
        "gpt-5.6-sol",
        pricing!(5.0, 30.0, Some(0.5), Some(6.25)),
    ),
    (
        "openai",
        "gpt-5.6-terra",
        pricing!(2.0, 12.0, Some(0.2), Some(2.5)),
    ),
    ("openai", "o3", pricing!(2.0, 8.0, Some(0.5), None)),
    ("openai", "o3-pro", pricing!(20.0, 80.0, None, None)),
    ("zai", "glm-4.5", pricing!(0.6, 2.2, Some(0.11), Some(0.0))),
    (
        "zai",
        "glm-4.5-air",
        pricing!(0.2, 1.1, Some(0.03), Some(0.0)),
    ),
    (
        "zai",
        "glm-4.5-flash",
        pricing!(0.0, 0.0, Some(0.0), Some(0.0)),
    ),
    ("zai", "glm-4.5v", pricing!(0.6, 1.8, None, None)),
    ("zai", "glm-4.6", pricing!(0.6, 2.2, Some(0.11), Some(0.0))),
    ("zai", "glm-4.6v", pricing!(0.3, 0.9, None, None)),
    ("zai", "glm-4.7", pricing!(0.6, 2.2, Some(0.11), Some(0.0))),
    (
        "zai",
        "glm-4.7-flash",
        pricing!(0.0, 0.0, Some(0.0), Some(0.0)),
    ),
    (
        "zai",
        "glm-4.7-flashx",
        pricing!(0.07, 0.4, Some(0.01), Some(0.0)),
    ),
    ("zai", "glm-5", pricing!(1.0, 3.2, Some(0.2), Some(0.0))),
    (
        "zai",
        "glm-5-turbo",
        pricing!(1.2, 4.0, Some(0.24), Some(0.0)),
    ),
    ("zai", "glm-5.1", pricing!(1.4, 4.4, Some(0.26), Some(0.0))),
    ("zai", "glm-5.2", pricing!(1.4, 4.4, Some(0.26), Some(0.0))),
    (
        "zai",
        "glm-5v-turbo",
        pricing!(1.2, 4.0, Some(0.24), Some(0.0)),
    ),
];

/// Models billed by subscription (coding plan) rather than per token.
pub fn plan_billed(full_id: &str) -> bool {
    find(full_id).is_some_and(|model| matches!(model.access, ModelAccess::ZaiCodingPlan))
}

/// Pricing for a `provider/model-id` string, if known.
pub fn pricing_for(full_id: &str) -> Option<ModelPricing> {
    let (provider, id) = full_id.split_once('/')?;
    PRICING
        .iter()
        .find(|(entry_provider, entry_id, _)| *entry_provider == provider && *entry_id == id)
        .map(|(_, _, pricing)| *pricing)
}

const NO_VARIANTS: &[ModelVariant] = &[];
const OPENAI_WIDE_VARIANTS: &[ModelVariant] = &[
    ModelVariant {
        id: "low",
        name: "Low",
    },
    ModelVariant {
        id: "medium",
        name: "Medium",
    },
    ModelVariant {
        id: "high",
        name: "High",
    },
];
const OPENAI_GPT5_VARIANTS: &[ModelVariant] = &[
    ModelVariant {
        id: "minimal",
        name: "Minimal",
    },
    ModelVariant {
        id: "low",
        name: "Low",
    },
    ModelVariant {
        id: "medium",
        name: "Medium",
    },
    ModelVariant {
        id: "high",
        name: "High",
    },
];
const OPENAI_GPT51_VARIANTS: &[ModelVariant] = &[
    ModelVariant {
        id: "none",
        name: "None",
    },
    ModelVariant {
        id: "low",
        name: "Low",
    },
    ModelVariant {
        id: "medium",
        name: "Medium",
    },
    ModelVariant {
        id: "high",
        name: "High",
    },
];
const OPENAI_GPT52_VARIANTS: &[ModelVariant] = &[
    ModelVariant {
        id: "none",
        name: "None",
    },
    ModelVariant {
        id: "low",
        name: "Low",
    },
    ModelVariant {
        id: "medium",
        name: "Medium",
    },
    ModelVariant {
        id: "high",
        name: "High",
    },
    ModelVariant {
        id: "xhigh",
        name: "Extra high",
    },
];
const OPENAI_PRO_VARIANTS: &[ModelVariant] = &[ModelVariant {
    id: "high",
    name: "High",
}];
const OPENAI_CHAT_VARIANTS: &[ModelVariant] = &[ModelVariant {
    id: "medium",
    name: "Medium",
}];
const OPENAI_VERSIONED_PRO_VARIANTS: &[ModelVariant] = &[
    ModelVariant {
        id: "medium",
        name: "Medium",
    },
    ModelVariant {
        id: "high",
        name: "High",
    },
    ModelVariant {
        id: "xhigh",
        name: "Extra high",
    },
];

/// GLM-5.3 thinking effort levels (https://z.ai/blog/glm-5.3). The server
/// default is `max`; disabling thinking is not supported by the model.
const ZAI_EFFORT_VARIANTS: &[ModelVariant] = &[
    ModelVariant {
        id: "low",
        name: "Low",
    },
    ModelVariant {
        id: "high",
        name: "High",
    },
    ModelVariant {
        id: "max",
        name: "Max",
    },
];

impl ModelInfo {
    pub fn full_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    pub fn variants(&self) -> &'static [ModelVariant] {
        if self.provider == "zai" {
            return if self.id == "glm-5.3" {
                ZAI_EFFORT_VARIANTS
            } else {
                NO_VARIANTS
            };
        }
        if self.provider != "openai" {
            return NO_VARIANTS;
        }
        if self.id == "gpt-5.2-chat-latest" {
            return OPENAI_CHAT_VARIANTS;
        }
        if !self.reasoning_summaries {
            return NO_VARIANTS;
        }
        if self.id == "gpt-5-pro" {
            return OPENAI_PRO_VARIANTS;
        }
        if self.id.starts_with("gpt-5.") && self.id.contains("-pro") {
            return OPENAI_VERSIONED_PRO_VARIANTS;
        }
        if let Some(version) = self
            .id
            .strip_prefix("gpt-5.")
            .and_then(|id| id.split('-').next())
            .and_then(|version| version.parse::<u8>().ok())
        {
            return if version == 1 {
                OPENAI_GPT51_VARIANTS
            } else {
                OPENAI_GPT52_VARIANTS
            };
        }
        if self.id.starts_with("gpt-5") {
            return OPENAI_GPT5_VARIANTS;
        }
        if self.id.starts_with("o3") {
            return OPENAI_WIDE_VARIANTS;
        }
        NO_VARIANTS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelAccess {
    OpenAi,
    OpenAiBoth,
    Zai,
    ZaiCodingPlan,
    ZaiBoth,
}

macro_rules! model {
    ($provider:literal, $id:literal, $name:literal, $context:literal, $output:literal, $access:ident) => {
        model!($provider, $id, $name, $context, $output, $access, false)
    };
    ($provider:literal, $id:literal, $name:literal, $context:literal, $output:literal, $access:ident, $reasoning_summaries:literal) => {
        ModelInfo {
            provider: $provider,
            id: $id,
            name: $name,
            context_limit: $context,
            max_context_limit: $context,
            input_limit: $context - $output,
            output_limit: $output,
            reasoning_summaries: $reasoning_summaries,
            access: ModelAccess::$access,
        }
    };
}

macro_rules! model_input {
    ($provider:literal, $id:literal, $name:literal, $context:literal, $input:literal, $output:literal, $access:ident) => {
        model_input!(
            $provider, $id, $name, $context, $input, $output, $access, false
        )
    };
    ($provider:literal, $id:literal, $name:literal, $context:literal, $input:literal, $output:literal, $access:ident, $reasoning_summaries:literal) => {
        ModelInfo {
            provider: $provider,
            id: $id,
            name: $name,
            context_limit: $context,
            max_context_limit: $context,
            input_limit: $input,
            output_limit: $output,
            reasoning_summaries: $reasoning_summaries,
            access: ModelAccess::$access,
        }
    };
}

macro_rules! model_window {
    ($provider:literal, $id:literal, $name:literal, $context:literal, $max_context:literal, $input:literal, $output:literal, $access:ident) => {
        model_window!(
            $provider,
            $id,
            $name,
            $context,
            $max_context,
            $input,
            $output,
            $access,
            false
        )
    };
    ($provider:literal, $id:literal, $name:literal, $context:literal, $max_context:literal, $input:literal, $output:literal, $access:ident, $reasoning_summaries:literal) => {
        ModelInfo {
            provider: $provider,
            id: $id,
            name: $name,
            context_limit: $context,
            max_context_limit: $max_context,
            input_limit: $input,
            output_limit: $output,
            reasoning_summaries: $reasoning_summaries,
            access: ModelAccess::$access,
        }
    };
}

// Active text-output models with tool calling, from the models.dev OpenAI,
// Z.AI, and Z.AI Coding Plan provider records. Image, embedding, realtime,
// and deprecated models are intentionally excluded. GPT-5.6 coding defaults
// follow Codex while models.dev remains the source for their maximum windows.
static CATALOG: &[ModelInfo] = &[
    model_window!(
        "openai",
        "gpt-5.6-sol",
        "GPT-5.6 Sol",
        272_000,
        1_050_000,
        272_000,
        128_000,
        OpenAiBoth,
        true
    ),
    model!(
        "openai", "gpt-5.6", "GPT-5.6", 1_050_000, 128_000, OpenAi, true
    ),
    model_window!(
        "openai",
        "gpt-5.6-luna",
        "GPT-5.6 Luna",
        272_000,
        1_050_000,
        272_000,
        128_000,
        OpenAiBoth,
        true
    ),
    model_window!(
        "openai",
        "gpt-5.6-terra",
        "GPT-5.6 Terra",
        272_000,
        1_050_000,
        272_000,
        128_000,
        OpenAiBoth,
        true
    ),
    model!(
        "openai",
        "gpt-5.5-pro",
        "GPT-5.5 Pro",
        1_050_000,
        128_000,
        OpenAi,
        true
    ),
    model!(
        "openai", "gpt-5.5", "GPT-5.5", 1_050_000, 128_000, OpenAiBoth, true
    ),
    model!(
        "openai",
        "gpt-5.4-pro",
        "GPT-5.4 Pro",
        1_050_000,
        128_000,
        OpenAi,
        true
    ),
    model!(
        "openai", "gpt-5.4", "GPT-5.4", 1_050_000, 128_000, OpenAi, true
    ),
    model!(
        "openai",
        "gpt-5.4-mini",
        "GPT-5.4 mini",
        400_000,
        128_000,
        OpenAi,
        true
    ),
    model!(
        "openai",
        "gpt-5.4-nano",
        "GPT-5.4 nano",
        400_000,
        128_000,
        OpenAi,
        true
    ),
    model!(
        "openai",
        "gpt-5.3-codex",
        "GPT-5.3 Codex",
        400_000,
        128_000,
        OpenAi,
        true
    ),
    model_input!(
        "openai",
        "gpt-5.3-codex-spark",
        "GPT-5.3 Codex Spark",
        128_000,
        100_000,
        32_000,
        OpenAiBoth,
        true
    ),
    model!(
        "openai",
        "gpt-5.3-chat-latest",
        "GPT-5.3 Chat (latest)",
        128_000,
        16_384,
        OpenAi
    ),
    model!(
        "openai",
        "gpt-5.2-pro",
        "GPT-5.2 Pro",
        400_000,
        128_000,
        OpenAi,
        true
    ),
    model!(
        "openai", "gpt-5.2", "GPT-5.2", 400_000, 128_000, OpenAi, true
    ),
    model!(
        "openai",
        "gpt-5.2-chat-latest",
        "GPT-5.2 Chat",
        128_000,
        16_384,
        OpenAi
    ),
    model!(
        "openai", "gpt-5.1", "GPT-5.1", 400_000, 128_000, OpenAi, true
    ),
    model_input!(
        "openai",
        "gpt-5-pro",
        "GPT-5 Pro",
        400_000,
        272_000,
        272_000,
        OpenAi,
        true
    ),
    model!("openai", "gpt-5", "GPT-5", 400_000, 128_000, OpenAi, true),
    model!(
        "openai",
        "gpt-5-mini",
        "GPT-5 Mini",
        400_000,
        128_000,
        OpenAi,
        true
    ),
    model!(
        "openai",
        "gpt-5-nano",
        "GPT-5 Nano",
        400_000,
        128_000,
        OpenAi,
        true
    ),
    model!("openai", "o3-pro", "o3-pro", 200_000, 100_000, OpenAi, true),
    model!("openai", "o3", "o3", 200_000, 100_000, OpenAi, true),
    model!("openai", "gpt-4.1", "GPT-4.1", 1_047_576, 32_768, OpenAi),
    model!(
        "openai",
        "gpt-4.1-mini",
        "GPT-4.1 mini",
        1_047_576,
        32_768,
        OpenAi
    ),
    model!("openai", "gpt-4o", "GPT-4o", 128_000, 16_384, OpenAi),
    model!(
        "openai",
        "gpt-4o-mini",
        "GPT-4o mini",
        128_000,
        16_384,
        OpenAi
    ),
    model!(
        "openai",
        "gpt-4o-2024-11-20",
        "GPT-4o (2024-11-20)",
        128_000,
        16_384,
        OpenAi
    ),
    model!(
        "openai",
        "gpt-4o-2024-08-06",
        "GPT-4o (2024-08-06)",
        128_000,
        16_384,
        OpenAi
    ),
    model!(
        "zai",
        "glm-5.3",
        "GLM-5.3",
        1_000_000,
        131_072,
        ZaiCodingPlan
    ),
    model!(
        "zai",
        "glm-5.2-highspeed",
        "GLM-5.2 Highspeed",
        1_000_000,
        131_072,
        ZaiCodingPlan
    ),
    model!("zai", "glm-5.2", "GLM-5.2", 1_000_000, 131_072, ZaiBoth),
    model!("zai", "glm-5.1", "GLM-5.1", 200_000, 131_072, Zai),
    model!(
        "zai",
        "glm-5-turbo",
        "GLM-5-Turbo",
        200_000,
        131_072,
        ZaiBoth
    ),
    model!("zai", "glm-5", "GLM-5", 204_800, 131_072, Zai),
    model!("zai", "glm-5v-turbo", "GLM-5V-Turbo", 200_000, 131_072, Zai),
    model!("zai", "glm-4.7", "GLM-4.7", 204_800, 131_072, ZaiBoth),
    model!(
        "zai",
        "glm-4.7-flashx",
        "GLM-4.7-FlashX",
        200_000,
        131_072,
        Zai
    ),
    model!(
        "zai",
        "glm-4.7-flash",
        "GLM-4.7-Flash",
        200_000,
        131_072,
        Zai
    ),
    model!("zai", "glm-4.6v", "GLM-4.6V", 128_000, 32_768, Zai),
    model!("zai", "glm-4.6", "GLM-4.6", 204_800, 131_072, Zai),
    model!("zai", "glm-4.5v", "GLM-4.5V", 64_000, 16_384, Zai),
    model!("zai", "glm-4.5-air", "GLM-4.5-Air", 131_072, 98_304, Zai),
    model!(
        "zai",
        "glm-4.5-flash",
        "GLM-4.5-Flash",
        131_072,
        98_304,
        Zai
    ),
    model!("zai", "glm-4.5", "GLM-4.5", 131_072, 98_304, Zai),
];

pub fn catalog() -> &'static [ModelInfo] {
    CATALOG
}

pub fn find(full_id: &str) -> Option<&'static ModelInfo> {
    CATALOG.iter().find(|model| {
        full_id
            .split_once('/')
            .is_some_and(|(provider, id)| provider == model.provider && id == model.id)
    })
}

pub fn variant_options(full_id: &str, variant: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let Some(variant) = variant else {
        return Ok(serde_json::Value::Null);
    };
    let model = find(full_id).ok_or_else(|| anyhow::anyhow!("unknown model {full_id}"))?;
    if !model
        .variants()
        .iter()
        .any(|candidate| candidate.id == variant)
    {
        anyhow::bail!("unsupported variant {variant:?} for {full_id}");
    }
    match model.provider {
        "openai" => Ok(serde_json::json!({"reasoning": {"effort": variant}})),
        // GLM-5.3 thinking levels; `thinking.type` must be "enabled"
        // (disabling is unsupported and rejected by the API).
        "zai" => Ok(serde_json::json!({
            "thinking": {"type": "enabled"},
            "reasoning_effort": variant,
        })),
        provider => anyhow::bail!("provider {provider} does not support reasoning variants"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pricing_lookup_and_cost_arithmetic() {
        let pricing = pricing_for("zai/glm-4.7").unwrap();
        assert_eq!(pricing.input, 0.6);
        let usage = crate::session::Usage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_read_input_tokens: 2_000_000,
            cache_creation_input_tokens: 0,
            input_token_accounting: None,
        };
        let cost = pricing.cost(&usage);
        // 1M * 0.6 + 0.5M * 2.2 + 2M * 0.11 = 0.6 + 1.1 + 0.22
        assert!((cost - 1.92).abs() < 1e-9, "{cost}");

        // Providers without a cache-read rate bill reads at the input rate.
        let pro = pricing_for("openai/gpt-5-pro").unwrap();
        let cached = crate::session::Usage {
            cache_read_input_tokens: 1_000_000,
            ..Default::default()
        };
        assert!((pro.cost(&cached) - 15.0).abs() < 1e-9);

        assert!(pricing_for("openai/unknown-model").is_none());
        assert!(pricing_for("no-slash").is_none());
        // Coding-plan-only models intentionally have no API pricing.
        assert!(pricing_for("zai/glm-5.3").is_none());
        assert!(plan_billed("zai/glm-5.3"));
        assert!(!plan_billed("zai/glm-4.7"));
        assert!(!plan_billed("openai/gpt-5.6-sol"));
        assert!(!plan_billed("custom/unknown"));
    }

    #[test]
    fn every_catalog_model_with_api_access_has_pricing_or_is_plan_only() {
        for model in CATALOG {
            let priced = pricing_for(&model.full_id()).is_some();
            let plan_only = matches!(model.access, ModelAccess::ZaiCodingPlan);
            assert!(
                priced || plan_only,
                "{} lacks pricing and is not coding-plan-only",
                model.full_id()
            );
        }
    }

    #[test]
    fn openai_reasoning_variants_are_model_specific() {
        let ids = |model: &str| {
            find(model)
                .unwrap()
                .variants()
                .iter()
                .map(|variant| variant.id)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            ids("openai/gpt-5.6-sol"),
            vec!["none", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(ids("openai/gpt-5.5-pro"), vec!["medium", "high", "xhigh"]);
        assert_eq!(ids("openai/gpt-5.1"), vec!["none", "low", "medium", "high"]);
        assert_eq!(
            ids("openai/gpt-5"),
            vec!["minimal", "low", "medium", "high"]
        );
        assert_eq!(ids("openai/gpt-5.2-chat-latest"), vec!["medium"]);
        assert!(ids("openai/gpt-5.3-chat-latest").is_empty());
        assert!(ids("openai/gpt-4.1").is_empty());
        assert!(ids("zai/glm-5.2").is_empty());
    }

    #[test]
    fn glm53_exposes_thinking_effort_variants() {
        let ids = |model: &str| {
            find(model)
                .unwrap()
                .variants()
                .iter()
                .map(|variant| variant.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("zai/glm-5.3"), ["low", "high", "max"]);
        assert!(ids("zai/glm-5.2").is_empty());
        assert!(ids("zai/glm-4.7").is_empty());

        let options = variant_options("zai/glm-5.3", Some("max")).unwrap();
        assert_eq!(options["reasoning_effort"], "max");
        assert_eq!(options["thinking"]["type"], "enabled");
        assert!(variant_options("zai/glm-5.3", Some("xhigh")).is_err());
        assert!(variant_options("zai/glm-4.7", Some("max")).is_err());
    }

    #[test]
    fn variant_options_validate_before_building_provider_fields() {
        assert_eq!(
            variant_options("openai/gpt-5.2", Some("high")).unwrap(),
            serde_json::json!({"reasoning": {"effort": "high"}})
        );
        assert_eq!(
            variant_options("openai/gpt-5.2", None).unwrap(),
            serde_json::Value::Null
        );
        assert!(variant_options("openai/gpt-5.2", Some("max")).is_err());
        assert!(variant_options("zai/glm-5.2", Some("high")).is_err());
    }
}
