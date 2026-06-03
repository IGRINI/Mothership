use std::collections::BTreeSet;

use mothership_adapter_sdk::protocol::{
    Model, ReasoningCapabilities, ReasoningEffort, ReasoningOption,
};

use crate::builtin_models::{ClaudeModelFamily, BUILTIN_CLAUDE_MODELS};
use crate::model_catalog::{ClaudeModelCatalog, ClaudeModelInfo};
use crate::settings::ClaudeAgentSettings;

pub(crate) fn from_sdk_catalog(
    catalog: ClaudeModelCatalog,
    settings: &ClaudeAgentSettings,
) -> Vec<Model> {
    from_catalog_parts(
        catalog.models,
        catalog.available_models.as_deref(),
        settings.custom_models(),
        settings.hidden_builtin_models(),
    )
}

fn from_catalog_parts(
    sdk_models: Vec<ClaudeModelInfo>,
    available_models: Option<&[String]>,
    custom_models: &str,
    hidden_builtin_models: &str,
) -> Vec<Model> {
    let sdk_templates = SdkReasoningTemplates::new(&sdk_models);
    let hidden_builtin_ids = parse_model_ids(hidden_builtin_models)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();

    for spec in BUILTIN_CLAUDE_MODELS {
        if hidden_builtin_ids.contains(spec.id) {
            continue;
        }

        let recommended = spec.recommended || models.is_empty();
        push_model(
            &mut models,
            &mut seen,
            Model {
                id: spec.id.to_string(),
                label: spec.label.to_string(),
                recommended,
                reasoning: sdk_templates.reasoning_for(spec.family),
            },
            available_models,
        );
    }

    for id in parse_model_ids(custom_models) {
        let family = infer_model_family(&id);
        let recommended = models.is_empty();
        push_model(
            &mut models,
            &mut seen,
            Model {
                label: id.clone(),
                reasoning: family.and_then(|family| sdk_templates.reasoning_for(family)),
                id,
                recommended,
            },
            available_models,
        );
    }

    models
}

pub(crate) fn parse_model_ids(spec: &str) -> Vec<String> {
    spec.split(['\n', ','])
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn push_model(
    models: &mut Vec<Model>,
    seen: &mut BTreeSet<String>,
    model: Model,
    available_models: Option<&[String]>,
) {
    if model_allowed(&model.id, available_models) && seen.insert(model.id.clone()) {
        models.push(model);
    }
}

#[derive(Default)]
struct SdkReasoningTemplates {
    opus: Option<ReasoningCapabilities>,
    sonnet: Option<ReasoningCapabilities>,
    haiku: Option<ReasoningCapabilities>,
}

impl SdkReasoningTemplates {
    fn new(models: &[ClaudeModelInfo]) -> Self {
        let mut templates = Self::default();
        for model in models {
            let reasoning = reasoning_from_model(model);
            match model.value.as_str() {
                "default" => templates.opus = reasoning,
                "sonnet" => templates.sonnet = reasoning,
                "haiku" => templates.haiku = reasoning,
                _ => {}
            }
        }
        templates
    }

    fn reasoning_for(&self, family: ClaudeModelFamily) -> Option<ReasoningCapabilities> {
        match family {
            ClaudeModelFamily::Opus => self.opus.clone(),
            ClaudeModelFamily::Sonnet => self.sonnet.clone(),
            ClaudeModelFamily::Haiku => self.haiku.clone(),
        }
    }
}

fn infer_model_family(id: &str) -> Option<ClaudeModelFamily> {
    let normalized = normalize_model_id(id);
    match model_family(&normalized) {
        Some("opus") => Some(ClaudeModelFamily::Opus),
        Some("sonnet") => Some(ClaudeModelFamily::Sonnet),
        Some("haiku") => Some(ClaudeModelFamily::Haiku),
        _ => None,
    }
}

fn reasoning_from_model(model: &ClaudeModelInfo) -> Option<ReasoningCapabilities> {
    if !model.supports_effort && !model.supports_adaptive_thinking {
        return None;
    }

    let efforts = model
        .supported_effort_levels
        .iter()
        .filter_map(|effort| ReasoningEffort::parse(effort))
        .collect::<Vec<_>>();
    let mut capabilities = ReasoningCapabilities::from_efforts(efforts, false, false, false);
    if model.supports_adaptive_thinking && !capabilities.options.iter().any(|o| o.id == "auto") {
        capabilities.options.insert(0, ReasoningOption::auto());
    }
    Some(capabilities)
}

fn model_allowed(id: &str, available_models: Option<&[String]>) -> bool {
    let Some(available_models) = available_models else {
        return true;
    };
    if available_models.is_empty() {
        return false;
    }

    let normalized_id = normalize_model_id(id);
    available_models
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .any(|allowed| allowed_model_matches(allowed, id, &normalized_id))
}

fn allowed_model_matches(allowed: &str, id: &str, normalized_id: &str) -> bool {
    if allowed == id {
        return true;
    }

    let normalized_allowed = normalize_model_id(allowed);
    if normalized_allowed == normalized_id {
        return true;
    }

    if matches!(normalized_allowed.as_str(), "opus" | "sonnet" | "haiku") {
        return model_family(normalized_id) == Some(normalized_allowed.as_str());
    }

    normalized_id.starts_with(&normalized_allowed)
}

fn normalize_model_id(id: &str) -> String {
    id.trim()
        .trim_end_matches("[1m]")
        .strip_prefix("claude-")
        .unwrap_or_else(|| id.trim().trim_end_matches("[1m]"))
        .to_ascii_lowercase()
}

fn model_family(normalized_id: &str) -> Option<&'static str> {
    if normalized_id.starts_with("opus") {
        Some("opus")
    } else if normalized_id.starts_with("sonnet") {
        Some("sonnet")
    } else if normalized_id.starts_with("haiku") {
        Some("haiku")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdk_model(value: &str, display_name: &str) -> ClaudeModelInfo {
        let _ = display_name;
        ClaudeModelInfo {
            value: value.to_string(),
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_adaptive_thinking: false,
        }
    }

    fn all_builtin_model_ids() -> String {
        BUILTIN_CLAUDE_MODELS
            .iter()
            .map(|model| model.id)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn exposes_builtin_claude_catalog_by_default() {
        let mut default = sdk_model("default", "Default");
        default.supports_effort = true;
        default.supports_adaptive_thinking = true;
        default.supported_effort_levels = vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
            "xhigh".to_string(),
            "max".to_string(),
        ];
        let mut sonnet = sdk_model("sonnet", "Sonnet");
        sonnet.supports_effort = true;
        sonnet.supported_effort_levels = vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
            "max".to_string(),
        ];

        let models = from_catalog_parts(
            vec![default, sonnet, sdk_model("haiku", "Haiku")],
            None,
            "",
            "",
        );

        let ids = models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "opus[1m]",
                "opus",
                "sonnet",
                "haiku",
                "claude-opus-4-7",
                "claude-opus-4-7[1m]",
                "claude-opus-4-6",
            ]
        );
        assert_eq!(models[0].label, "Opus 4.8 (1M context)");
        assert!(models[0].recommended);
        assert_eq!(
            models[0].reasoning.as_ref().unwrap().efforts,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max,
            ]
        );
        assert_eq!(
            models[2].reasoning.as_ref().unwrap().efforts,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ]
        );
    }

    #[test]
    fn can_hide_individual_builtin_models_and_use_custom_list() {
        let models = from_catalog_parts(
            vec![sdk_model("default", "Default")],
            None,
            "custom-model",
            "opus[1m]\nclaude-opus-4-6",
        );
        let ids = models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            vec![
                "opus",
                "sonnet",
                "haiku",
                "claude-opus-4-7",
                "claude-opus-4-7[1m]",
                "custom-model",
            ]
        );
        assert!(models[0].recommended);
    }

    #[test]
    fn custom_models_are_deduped_against_builtins() {
        let models = from_catalog_parts(Vec::new(), None, "opus[1m]\nmy-model", "");
        let ids = models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            vec![
                "opus[1m]",
                "opus",
                "sonnet",
                "haiku",
                "claude-opus-4-7",
                "claude-opus-4-7[1m]",
                "claude-opus-4-6",
                "my-model",
            ]
        );
    }

    #[test]
    fn respects_empty_available_models_allowlist() {
        let allowlist = Vec::new();
        let models = from_catalog_parts(
            vec![sdk_model("default", "Default")],
            Some(&allowlist),
            "claude-opus-4-7",
            "",
        );

        assert!(models.is_empty());
    }

    #[test]
    fn respects_available_models_family_allowlist() {
        let allowlist = vec!["sonnet".to_string()];
        let models = from_catalog_parts(
            vec![sdk_model("default", "Default")],
            Some(&allowlist),
            "claude-opus-4-7",
            "",
        );
        let ids = models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["sonnet"]);
    }

    #[test]
    fn custom_models_inherit_family_reasoning_from_sdk_metadata() {
        let mut default = sdk_model("default", "Default");
        default.supports_effort = true;
        default.supports_adaptive_thinking = true;
        default.supported_effort_levels = vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
            "max".to_string(),
        ];

        let models = from_catalog_parts(
            vec![default],
            None,
            "claude-opus-custom",
            &all_builtin_model_ids(),
        );

        assert_eq!(
            models[0].reasoning.as_ref().unwrap().efforts,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ]
        );
    }

    #[test]
    fn parse_model_ids_accepts_newlines_and_commas() {
        assert_eq!(
            parse_model_ids("opus, sonnet\nclaude-opus-4-7"),
            vec!["opus", "sonnet", "claude-opus-4-7"]
        );
    }
}
