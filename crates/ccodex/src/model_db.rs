//! Official model behavior table: data comes from the vendored
//! models-manager/models.json (embedded at build time). Carries every per-model
//! behavior parameter the official build_responses_request needs (a ModelInfo subset
//! from core/src/client.rs), so each model takes its official branch.

use std::collections::HashMap;

/// Official models list, embedded at build time — also seeds the /v1/models cache.
pub const MODELS_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/models.json"));
const BASE_INSTRUCTIONS: &str = include_str!(concat!(env!("OUT_DIR"), "/base_instructions.md"));

/// Official model names may carry a reasoning suffix (e.g. gpt-5.6-sol-high);
/// strip them level by level before lookup.
const REASONING_SUFFIXES: &[&str] = &["-none", "-minimal", "-low", "-medium", "-high", "-xhigh"];

#[derive(Debug, Clone)]
pub struct ModelBehavior {
    /// Model base instructions (text embedded in official models.json; falls back to the
    /// global prompt.md).
    pub base_instructions: String,
    /// Responses Lite branch: instructions/tools become input prefix items,
    /// parallel_tool_calls=false, reasoning.context="all_turns", plus the
    /// x-openai-internal-codex-responses-lite header.
    pub use_responses_lite: bool,
    /// Default reasoning effort (official config.model_reasoning_effort defaults to None
    /// → model default).
    pub default_reasoning_level: String,
    /// Supported effort levels (basis for resolve_reasoning_effort mapping).
    pub supported_reasoning_levels: Vec<String>,
    /// Multi-agent effort (preferred mapping target for ultra).
    pub multi_agent_reasoning_effort: Option<String>,
    /// Default reasoning summary; the summary field is omitted when this is "none" or
    /// the model doesn't support the parameter.
    pub default_reasoning_summary: String,
    pub supports_reasoning_summary_parameter: bool,
    /// text.verbosity support and its default.
    pub support_verbosity: bool,
    pub default_verbosity: Option<String>,
    pub node_repl_auto_review_required: bool,
    pub node_repl_disabled: bool,
}

pub struct ModelDb {
    by_slug: HashMap<String, ModelBehavior>,
}

impl ModelDb {
    pub fn load() -> Self {
        let parsed: serde_json::Value =
            serde_json::from_str(MODELS_JSON).expect("embedded models.json is valid");
        let mut by_slug = HashMap::new();
        if let Some(models) = parsed.get("models").and_then(|m| m.as_array()) {
            for model in models {
                let Some(slug) = model.get("slug").and_then(|s| s.as_str()) else {
                    continue;
                };
                let str_field =
                    |key: &str| model.get(key).and_then(|v| v.as_str()).map(str::to_string);
                let bool_field =
                    |key: &str| model.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
                let base_instructions = str_field("base_instructions")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| BASE_INSTRUCTIONS.to_string());
                let supported_reasoning_levels: Vec<String> = model
                    .get("supported_reasoning_levels")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|e| {
                                e.get("effort").and_then(|e| e.as_str()).map(str::to_string)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                by_slug.insert(
                    slug.to_string(),
                    ModelBehavior {
                        base_instructions,
                        use_responses_lite: bool_field("use_responses_lite"),
                        default_reasoning_level: str_field("default_reasoning_level")
                            .unwrap_or_else(|| "medium".to_string()),
                        supported_reasoning_levels,
                        multi_agent_reasoning_effort: str_field("multi_agent_reasoning_effort"),
                        default_reasoning_summary: str_field("default_reasoning_summary")
                            .unwrap_or_else(|| "none".to_string()),
                        supports_reasoning_summary_parameter: bool_field(
                            "supports_reasoning_summary_parameter",
                        ),
                        support_verbosity: bool_field("support_verbosity"),
                        default_verbosity: str_field("default_verbosity"),
                        node_repl_auto_review_required: bool_field(
                            "node_repl_auto_review_required",
                        ),
                        node_repl_disabled: bool_field("node_repl_disabled"),
                    },
                );
            }
        }
        tracing::info!(
            models = by_slug.len(),
            "loaded official model behavior table"
        );
        Self { by_slug }
    }

    /// Reasoning suffix on the model name ("gpt-5.6-sol-high" → Some("high")).
    /// Same stripping rule as for_model.
    pub fn effort_suffix(&self, model: &str) -> Option<String> {
        if self.by_slug.contains_key(model) {
            return None;
        }
        for suffix in REASONING_SUFFIXES {
            if let Some(base) = model.strip_suffix(suffix)
                && self.by_slug.contains_key(base)
            {
                return Some(suffix.trim_start_matches('-').to_string());
            }
        }
        None
    }

    /// Lookup: exact hit → suffix-stripped hit → unknown-model fallback
    /// (non-lite default shape + global instructions).
    pub fn for_model(&self, model: &str) -> ModelBehavior {
        if let Some(b) = self.by_slug.get(model) {
            return b.clone();
        }
        for suffix in REASONING_SUFFIXES {
            if let Some(base) = model.strip_suffix(suffix)
                && let Some(b) = self.by_slug.get(base)
            {
                return b.clone();
            }
        }
        tracing::debug!(model, "unknown model, using non-lite defaults");
        ModelBehavior {
            base_instructions: BASE_INSTRUCTIONS.to_string(),
            use_responses_lite: false,
            default_reasoning_level: "medium".to_string(),
            supported_reasoning_levels: Vec::new(),
            multi_agent_reasoning_effort: None,
            default_reasoning_summary: "none".to_string(),
            supports_reasoning_summary_parameter: false,
            support_verbosity: false,
            default_verbosity: None,
            node_repl_auto_review_required: false,
            node_repl_disabled: false,
        }
    }
}
