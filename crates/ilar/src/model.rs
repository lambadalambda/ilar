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
    pub input_limit: u64,
    pub output_limit: u64,
    pub(crate) reasoning_summaries: bool,
    /// Accepts image input.
    pub(crate) vision: bool,
    /// Effort ladder this model exposes; empty when it has none. Carried
    /// per row so a new id cannot inherit another one's ladder by
    /// spelling.
    pub(crate) variants: &'static [ModelVariant],
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
    /// A row with no optional capabilities: text in, no effort ladder,
    /// and an input budget that assumes a maximum-length reply. The
    /// builders below add whatever the model actually has.
    const fn new(
        provider: &'static str,
        id: &'static str,
        name: &'static str,
        context_limit: u64,
        output_limit: u64,
        access: ModelAccess,
    ) -> Self {
        Self {
            provider,
            id,
            name,
            context_limit,
            input_limit: context_limit - output_limit,
            output_limit,
            reasoning_summaries: false,
            vision: false,
            variants: NO_VARIANTS,
            access,
        }
    }

    /// Declared input cap, where it is not the window minus a full reply.
    const fn input(mut self, limit: u64) -> Self {
        self.input_limit = limit;
        self
    }

    /// Accepts image input.
    const fn vision(mut self) -> Self {
        self.vision = true;
        self
    }

    /// Emits reasoning summaries and exposes this effort ladder.
    const fn reasoning(mut self, variants: &'static [ModelVariant]) -> Self {
        self.reasoning_summaries = true;
        self.variants = variants;
        self
    }

    /// Exposes an effort ladder without emitting reasoning summaries.
    const fn effort(mut self, variants: &'static [ModelVariant]) -> Self {
        self.variants = variants;
        self
    }

    pub fn full_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// Whether the model accepts image input.
    pub fn supports_vision(&self) -> bool {
        self.vision
    }

    pub fn variants(&self) -> &'static [ModelVariant] {
        self.variants
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

/// A base catalog row; capabilities are appended with the builders on
/// [`ModelInfo`], so every row states its own.
macro_rules! model {
    ($provider:literal, $id:literal, $name:literal, $context:literal, $output:literal, $access:ident) => {
        ModelInfo::new(
            $provider,
            $id,
            $name,
            $context,
            $output,
            ModelAccess::$access,
        )
    };
}

// Active text-output models with tool calling, from the models.dev OpenAI,
// Z.AI, and Z.AI Coding Plan provider records. Image, embedding, realtime,
// and deprecated models are intentionally excluded. GPT-5.6 coding defaults
// follow Codex while models.dev remains the source for their maximum windows.
static CATALOG: &[ModelInfo] = &[
    model!(
        "openai",
        "gpt-5.6-sol",
        "GPT-5.6 Sol",
        272_000,
        128_000,
        OpenAiBoth
    )
    .input(272_000)
    .vision()
    .reasoning(OPENAI_GPT52_VARIANTS),
    model!("openai", "gpt-5.6", "GPT-5.6", 1_050_000, 128_000, OpenAi)
        .vision()
        .reasoning(OPENAI_GPT52_VARIANTS),
    model!(
        "openai",
        "gpt-5.6-luna",
        "GPT-5.6 Luna",
        272_000,
        128_000,
        OpenAiBoth
    )
    .input(272_000)
    .vision()
    .reasoning(OPENAI_GPT52_VARIANTS),
    model!(
        "openai",
        "gpt-5.6-terra",
        "GPT-5.6 Terra",
        272_000,
        128_000,
        OpenAiBoth
    )
    .input(272_000)
    .vision()
    .reasoning(OPENAI_GPT52_VARIANTS),
    model!(
        "openai",
        "gpt-5.5-pro",
        "GPT-5.5 Pro",
        1_050_000,
        128_000,
        OpenAi
    )
    .vision()
    .reasoning(OPENAI_VERSIONED_PRO_VARIANTS),
    model!(
        "openai", "gpt-5.5", "GPT-5.5", 1_050_000, 128_000, OpenAiBoth
    )
    .vision()
    .reasoning(OPENAI_GPT52_VARIANTS),
    model!(
        "openai",
        "gpt-5.4-pro",
        "GPT-5.4 Pro",
        1_050_000,
        128_000,
        OpenAi
    )
    .vision()
    .reasoning(OPENAI_VERSIONED_PRO_VARIANTS),
    model!("openai", "gpt-5.4", "GPT-5.4", 1_050_000, 128_000, OpenAi)
        .vision()
        .reasoning(OPENAI_GPT52_VARIANTS),
    model!(
        "openai",
        "gpt-5.4-mini",
        "GPT-5.4 mini",
        400_000,
        128_000,
        OpenAi
    )
    .vision()
    .reasoning(OPENAI_GPT52_VARIANTS),
    model!(
        "openai",
        "gpt-5.4-nano",
        "GPT-5.4 nano",
        400_000,
        128_000,
        OpenAi
    )
    .vision()
    .reasoning(OPENAI_GPT52_VARIANTS),
    model!(
        "openai",
        "gpt-5.3-codex",
        "GPT-5.3 Codex",
        400_000,
        128_000,
        OpenAi
    )
    .vision()
    .reasoning(OPENAI_GPT52_VARIANTS),
    model!(
        "openai",
        "gpt-5.3-codex-spark",
        "GPT-5.3 Codex Spark",
        128_000,
        32_000,
        OpenAiBoth
    )
    .input(100_000)
    .vision()
    .reasoning(OPENAI_GPT52_VARIANTS),
    model!(
        "openai",
        "gpt-5.3-chat-latest",
        "GPT-5.3 Chat (latest)",
        128_000,
        16_384,
        OpenAi
    )
    .vision(),
    model!(
        "openai",
        "gpt-5.2-pro",
        "GPT-5.2 Pro",
        400_000,
        128_000,
        OpenAi
    )
    .vision()
    .reasoning(OPENAI_VERSIONED_PRO_VARIANTS),
    model!("openai", "gpt-5.2", "GPT-5.2", 400_000, 128_000, OpenAi)
        .vision()
        .reasoning(OPENAI_GPT52_VARIANTS),
    model!(
        "openai",
        "gpt-5.2-chat-latest",
        "GPT-5.2 Chat",
        128_000,
        16_384,
        OpenAi
    )
    .vision()
    .effort(OPENAI_CHAT_VARIANTS),
    model!("openai", "gpt-5.1", "GPT-5.1", 400_000, 128_000, OpenAi)
        .vision()
        .reasoning(OPENAI_GPT51_VARIANTS),
    model!("openai", "gpt-5-pro", "GPT-5 Pro", 400_000, 272_000, OpenAi)
        .input(272_000)
        .vision()
        .reasoning(OPENAI_PRO_VARIANTS),
    model!("openai", "gpt-5", "GPT-5", 400_000, 128_000, OpenAi)
        .vision()
        .reasoning(OPENAI_GPT5_VARIANTS),
    model!(
        "openai",
        "gpt-5-mini",
        "GPT-5 Mini",
        400_000,
        128_000,
        OpenAi
    )
    .vision()
    .reasoning(OPENAI_GPT5_VARIANTS),
    model!(
        "openai",
        "gpt-5-nano",
        "GPT-5 Nano",
        400_000,
        128_000,
        OpenAi
    )
    .vision()
    .reasoning(OPENAI_GPT5_VARIANTS),
    model!("openai", "o3-pro", "o3-pro", 200_000, 100_000, OpenAi)
        .vision()
        .reasoning(OPENAI_WIDE_VARIANTS),
    model!("openai", "o3", "o3", 200_000, 100_000, OpenAi)
        .vision()
        .reasoning(OPENAI_WIDE_VARIANTS),
    model!("openai", "gpt-4.1", "GPT-4.1", 1_047_576, 32_768, OpenAi).vision(),
    model!(
        "openai",
        "gpt-4.1-mini",
        "GPT-4.1 mini",
        1_047_576,
        32_768,
        OpenAi
    )
    .vision(),
    model!("openai", "gpt-4o", "GPT-4o", 128_000, 16_384, OpenAi).vision(),
    model!(
        "openai",
        "gpt-4o-mini",
        "GPT-4o mini",
        128_000,
        16_384,
        OpenAi
    )
    .vision(),
    model!(
        "openai",
        "gpt-4o-2024-11-20",
        "GPT-4o (2024-11-20)",
        128_000,
        16_384,
        OpenAi
    )
    .vision(),
    model!(
        "openai",
        "gpt-4o-2024-08-06",
        "GPT-4o (2024-08-06)",
        128_000,
        16_384,
        OpenAi
    )
    .vision(),
    model!(
        "zai",
        "glm-5.3",
        "GLM-5.3",
        1_000_000,
        131_072,
        ZaiCodingPlan
    )
    .effort(ZAI_EFFORT_VARIANTS),
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
    model!(
        "zai",
        "glm-5v-turbo",
        "GLM-5V-Turbo",
        200_000,
        131_072,
        ZaiBoth
    )
    .vision(),
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
    model!("zai", "glm-4.6v", "GLM-4.6V", 128_000, 32_768, ZaiBoth).vision(),
    model!("zai", "glm-4.6", "GLM-4.6", 204_800, 131_072, Zai),
    model!("zai", "glm-4.5v", "GLM-4.5V", 64_000, 16_384, ZaiBoth).vision(),
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

/// The limit compaction must measure against. Providers reject a request
/// on its *input* size, not on the whole window, so measuring against
/// `context_limit` fires after the request is already unsendable — that
/// is what let a gpt-5.3-codex-spark session run to 127k of a 100k input
/// cap with the trigger sitting at 108.8k.
///
/// This is deliberately the conservative reading. For models with an
/// explicitly declared cap (`.input(n)`, and OpenAI's 272k of 400k)
/// it is exactly right. For models where `input_limit` is merely
/// `context_limit - output_limit` it assumes a maximum-length reply and
/// so triggers earlier than strictly necessary — GLM-4.7 compacts around
/// 63k of its 205k window. Erring this way costs extra summaries; erring
/// the other way loses the session. Separating "hard cap" from "shared
/// budget" needs an explicit catalog marker, since the two are
/// arithmetically identical today.
pub fn compaction_limit(model: &ModelInfo) -> u64 {
    model.input_limit.min(model.context_limit)
}

pub fn find(full_id: &str) -> Option<&'static ModelInfo> {
    CATALOG.iter().find(|model| {
        full_id
            .split_once('/')
            .is_some_and(|(provider, id)| provider == model.provider && id == model.id)
    })
}

/// Vision by full id; unknown models refuse conservatively.
pub fn supports_vision(full_id: &str) -> bool {
    find(full_id).is_some_and(ModelInfo::supports_vision)
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
    fn vision_is_every_openai_model_and_only_the_v_series_on_zai() {
        // The whole cataloged OpenAI lineup is multimodal.
        assert!(
            catalog()
                .iter()
                .filter(|model| model.provider == "openai")
                .all(|model| model.supports_vision())
        );
        assert!(supports_vision("zai/glm-4.6v"));
        assert!(supports_vision("zai/glm-4.5v"));
        assert!(supports_vision("zai/glm-5v-turbo"));
        assert!(!supports_vision("zai/glm-5.3"));
        assert!(!supports_vision("zai/glm-4.7"));
        // Unknown models refuse conservatively.
        assert!(!supports_vision("custom/mystery"));
    }

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
    fn pricing_and_catalog_rows_cover_each_other() {
        // The two arrays stay separate, so every price has to name a row
        // that exists — a typo here would silently show no dollars.
        for (provider, id, _) in PRICING {
            let full_id = format!("{provider}/{id}");
            assert!(
                find(&full_id).is_some(),
                "{full_id} is priced but absent from the catalog"
            );
        }
    }

    #[test]
    fn a_catalog_row_is_the_only_site_a_model_is_added_at() {
        // Everything this module reports about a model is on its row, so
        // a hypothetical GPT-5.10 needs no edit anywhere else. The
        // prefix parser this replaced read that id as version 10 and
        // handed it the 5.2 ladder.
        const FUTURE: ModelInfo =
            model!("openai", "gpt-5.10", "GPT-5.10", 400_000, 128_000, OpenAi)
                .input(272_000)
                .vision()
                .reasoning(OPENAI_GPT51_VARIANTS);
        let future = &FUTURE;
        assert!(future.supports_vision());
        assert_eq!(future.variants(), OPENAI_GPT51_VARIANTS);
        assert!(future.reasoning_summaries);
        assert_eq!(future.input_limit, 272_000);

        // A row that claims nothing gets nothing.
        const PLAIN: ModelInfo = model!("acme", "q-1", "Q-1", 100_000, 10_000, OpenAi);
        let plain = &PLAIN;
        assert!(!plain.supports_vision());
        assert!(plain.variants().is_empty());
        assert!(!plain.reasoning_summaries);
        assert_eq!(plain.input_limit, 90_000);
    }

    #[test]
    fn declared_capabilities_are_internally_consistent() {
        for model in CATALOG {
            assert!(
                !model.reasoning_summaries || !model.variants().is_empty(),
                "{} claims reasoning summaries without an effort ladder",
                model.full_id()
            );
            assert!(
                model.input_limit <= model.context_limit,
                "{} has an incoherent window",
                model.full_id()
            );
        }
        // Vision is a row flag, so the z.ai V-series is exactly the set
        // that carries it.
        let seeing = CATALOG
            .iter()
            .filter(|model| model.provider == "zai" && model.supports_vision())
            .map(|model| model.id)
            .collect::<Vec<_>>();
        assert_eq!(seeing, ["glm-5v-turbo", "glm-4.6v", "glm-4.5v"]);
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
        assert_eq!(ids("openai/gpt-5-pro"), vec!["high"]);
        assert_eq!(ids("openai/o3"), vec!["low", "medium", "high"]);
        assert_eq!(ids("openai/o3-pro"), vec!["low", "medium", "high"]);
        assert_eq!(ids("openai/gpt-5.2-chat-latest"), vec!["medium"]);
        assert!(ids("openai/gpt-5.3-chat-latest").is_empty());
        assert!(ids("openai/gpt-4.1").is_empty());
        assert!(ids("zai/glm-5.2").is_empty());
    }

    #[test]
    fn declared_input_caps_are_the_ones_compaction_measures_against() {
        // The window this model rejects requests on is well below its
        // context limit, which is what compaction has to see.
        let spark = find("openai/gpt-5.3-codex-spark").unwrap();
        assert_eq!(spark.input_limit, 100_000);
        assert_eq!(spark.context_limit, 128_000);
        assert_eq!(compaction_limit(spark), 100_000);

        let pro = find("openai/gpt-5-pro").unwrap();
        assert_eq!(pro.input_limit, 272_000);
        assert_eq!(pro.context_limit, 400_000);
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
