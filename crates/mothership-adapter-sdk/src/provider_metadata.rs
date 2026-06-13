//! Shared provider model-metadata helpers.
//!
//! Several adapters fetch a provider's raw model metadata as loosely-typed
//! `serde_json::Value` fields and derive Mothership's [`ReasoningCapabilities`]
//! from the same handful of fields (the various `*_effort(s)`/`*_level(s)` lists,
//! a nested `reasoning` object, the `supported_parameters` list, and
//! `capabilities`/`features` flags). That harvest is identical across providers
//! except for a few provider-specific knobs, so it lives here parameterized by
//! [`ReasoningHarvestOptions`].
//!
//! Adapters keep their own provider wire structs (`RemoteModel`,
//! `OpenRouterModelMetadata`, …) and just hand the relevant fields to
//! [`harvest_reasoning_capabilities`].

use serde_json::Value;

use crate::protocol::{ReasoningCapabilities, ReasoningEffort};
use crate::reasoning::{
    collect_effort_values, collect_nested_efforts, collect_parameter_names,
    value_signals_reasoning, value_signals_reasoning_summary,
};

/// Split a user-maintained model-id spec (newline- and/or comma-separated) into
/// trimmed, non-empty ids, preserving order. Shared by every adapter whose model
/// list is user-defined.
pub fn parse_model_ids(spec: &str) -> Vec<String> {
    spec.split(['\n', ','])
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// The loosely-typed metadata fields shared by providers that publish reasoning
/// capabilities. Every field defaults to [`Value::Null`]; an adapter supplies
/// only the ones its provider returns. Borrowed so callers pass references into
/// their own wire structs without cloning.
#[derive(Debug, Clone, Copy)]
pub struct ReasoningMetadataFields<'a> {
    pub supported_parameters: &'a Value,
    pub reasoning_efforts: &'a Value,
    pub supported_reasoning_efforts: &'a Value,
    pub effort_levels: &'a Value,
    pub supported_effort_levels: &'a Value,
    pub reasoning_levels: &'a Value,
    pub supported_reasoning_levels: &'a Value,
    pub reasoning: Option<&'a Value>,
    pub capabilities: Option<&'a Value>,
    pub features: Option<&'a Value>,
}

impl Default for ReasoningMetadataFields<'_> {
    fn default() -> Self {
        Self {
            supported_parameters: &Value::Null,
            reasoning_efforts: &Value::Null,
            supported_reasoning_efforts: &Value::Null,
            effort_levels: &Value::Null,
            supported_effort_levels: &Value::Null,
            reasoning_levels: &Value::Null,
            supported_reasoning_levels: &Value::Null,
            reasoning: None,
            capabilities: None,
            features: None,
        }
    }
}

/// Provider-specific knobs for [`harvest_reasoning_capabilities`].
///
/// These capture the only behavioral differences between the providers that
/// share the harvest: whether the provider's `include_reasoning` parameter
/// participates in the "is reasoning supported" decision and in
/// `supports_exclusion`, and whether a reasoning summary is offered.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReasoningHarvestOptions {
    /// Treat a `supported_parameters` entry named `include_reasoning` as both a
    /// reasoning signal and as enabling `supports_exclusion`. OpenRouter sets
    /// this; Codex does not.
    pub include_reasoning_parameter: bool,
    /// Derive `supports_summary` from the nested `reasoning` value (Codex). When
    /// false, `supports_summary` is always false (OpenRouter).
    pub detect_summary: bool,
}

/// Derive [`ReasoningCapabilities`] from a provider's raw metadata fields.
///
/// Returns `None` when nothing in the metadata indicates reasoning support. This
/// is the exact harvest previously duplicated in the Codex and OpenRouter
/// adapters; `options` selects the provider-specific behavior.
pub fn harvest_reasoning_capabilities(
    fields: ReasoningMetadataFields<'_>,
    options: ReasoningHarvestOptions,
) -> Option<ReasoningCapabilities> {
    let mut efforts = Vec::new();
    collect_effort_values(fields.reasoning_efforts, &mut efforts);
    collect_effort_values(fields.supported_reasoning_efforts, &mut efforts);
    collect_effort_values(fields.effort_levels, &mut efforts);
    collect_effort_values(fields.supported_effort_levels, &mut efforts);
    collect_effort_values(fields.reasoning_levels, &mut efforts);
    collect_effort_values(fields.supported_reasoning_levels, &mut efforts);
    if let Some(reasoning) = fields.reasoning {
        collect_nested_efforts(reasoning, &mut efforts);
    }

    let parameters = collect_parameter_names(fields.supported_parameters);
    let has_reasoning_parameter = parameters.contains("reasoning");
    let has_reasoning_effort_parameter = parameters.contains("reasoning_effort");
    let has_include_reasoning_parameter =
        options.include_reasoning_parameter && parameters.contains("include_reasoning");
    let has_reasoning_flag = fields
        .reasoning
        .map(value_signals_reasoning)
        .unwrap_or(false)
        || fields
            .capabilities
            .map(value_signals_reasoning)
            .unwrap_or(false)
        || fields
            .features
            .map(value_signals_reasoning)
            .unwrap_or(false);

    let supported = has_reasoning_parameter
        || has_reasoning_effort_parameter
        || has_include_reasoning_parameter
        || has_reasoning_flag
        || !efforts.is_empty();
    if !supported {
        return None;
    }
    if efforts.is_empty() && (has_reasoning_parameter || has_reasoning_effort_parameter) {
        efforts.extend(ReasoningEffort::openai_responses_values());
    }
    crate::reasoning::dedupe_efforts(&mut efforts);

    let supports_summary = options.detect_summary
        && fields
            .reasoning
            .map(value_signals_reasoning_summary)
            .unwrap_or(false);

    // Exclusion (dropping reasoning from the response) is an OpenRouter concept
    // signaled by its `reasoning`/`include_reasoning` parameters. The Codex
    // Responses API has no equivalent, so when `include_reasoning_parameter` is
    // off we keep the prior Codex behavior of never advertising exclusion.
    let supports_exclusion = options.include_reasoning_parameter
        && (has_reasoning_parameter || has_include_reasoning_parameter);

    Some(ReasoningCapabilities::from_efforts(
        efforts,
        has_reasoning_parameter,
        supports_exclusion,
        supports_summary,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_model_ids_accepts_newlines_and_commas() {
        assert_eq!(
            parse_model_ids("opus, sonnet\nclaude-opus-4-7"),
            vec!["opus", "sonnet", "claude-opus-4-7"]
        );
        assert!(parse_model_ids("  ,\n , ").is_empty());
    }

    #[test]
    fn harvest_returns_none_without_any_reasoning_signal() {
        let capabilities = json!({ "vision": true, "tools": true });
        let features = json!({ "attachments": true });
        let fields = ReasoningMetadataFields {
            capabilities: Some(&capabilities),
            features: Some(&features),
            ..Default::default()
        };

        assert!(
            harvest_reasoning_capabilities(fields, ReasoningHarvestOptions::default()).is_none()
        );
    }

    #[test]
    fn harvest_collects_efforts_and_summary_when_enabled() {
        let supported_parameters = json!(["reasoning"]);
        let reasoning_efforts = json!(["low", "high"]);
        let reasoning = json!({ "summary": ["auto"] });
        let fields = ReasoningMetadataFields {
            supported_parameters: &supported_parameters,
            reasoning_efforts: &reasoning_efforts,
            reasoning: Some(&reasoning),
            ..Default::default()
        };

        let capabilities = harvest_reasoning_capabilities(
            fields,
            ReasoningHarvestOptions {
                include_reasoning_parameter: false,
                detect_summary: true,
            },
        )
        .expect("reasoning");

        assert!(capabilities.supported);
        assert!(capabilities.supports_budget);
        assert!(capabilities.supports_summary);
        assert!(!capabilities.supports_exclusion);
        assert_eq!(
            capabilities.efforts,
            vec![ReasoningEffort::Low, ReasoningEffort::High]
        );
        assert_eq!(
            capabilities
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec!["auto", "low", "high"]
        );
    }

    #[test]
    fn harvest_does_not_detect_summary_when_disabled() {
        let supported_parameters = json!(["reasoning"]);
        let reasoning = json!({ "summary": ["auto"] });
        let fields = ReasoningMetadataFields {
            supported_parameters: &supported_parameters,
            reasoning: Some(&reasoning),
            ..Default::default()
        };

        let capabilities =
            harvest_reasoning_capabilities(fields, ReasoningHarvestOptions::default())
                .expect("reasoning");

        assert!(!capabilities.supports_summary);
    }

    #[test]
    fn harvest_include_reasoning_parameter_drives_exclusion() {
        let supported_parameters = json!(["include_reasoning"]);
        let fields = ReasoningMetadataFields {
            supported_parameters: &supported_parameters,
            ..Default::default()
        };

        // With the flag off, include_reasoning is not even a reasoning signal.
        assert!(
            harvest_reasoning_capabilities(fields, ReasoningHarvestOptions::default()).is_none()
        );

        // With the flag on, it both signals support and enables exclusion.
        let capabilities = harvest_reasoning_capabilities(
            fields,
            ReasoningHarvestOptions {
                include_reasoning_parameter: true,
                detect_summary: false,
            },
        )
        .expect("reasoning");
        assert!(capabilities.supported);
        assert!(!capabilities.supports_budget);
        assert!(capabilities.supports_exclusion);
    }

    #[test]
    fn harvest_fills_default_efforts_for_reasoning_parameter() {
        let supported_parameters = json!(["temperature", "reasoning", "reasoning_effort"]);
        let fields = ReasoningMetadataFields {
            supported_parameters: &supported_parameters,
            ..Default::default()
        };

        let capabilities = harvest_reasoning_capabilities(
            fields,
            ReasoningHarvestOptions {
                include_reasoning_parameter: true,
                detect_summary: false,
            },
        )
        .expect("reasoning");

        assert!(capabilities.supports_budget);
        assert!(capabilities.efforts.contains(&ReasoningEffort::High));
        assert_eq!(
            capabilities
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec!["auto", "none", "minimal", "low", "medium", "high", "xhigh"]
        );
    }
}
