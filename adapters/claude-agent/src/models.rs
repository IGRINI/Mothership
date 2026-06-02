use mothership_adapter_sdk::protocol::{
    Model, ReasoningCapabilities, ReasoningEffort, ReasoningOption,
};

use crate::cli::ClaudeModelInfo;

pub(crate) fn from_sdk_models(models: Vec<ClaudeModelInfo>) -> Vec<Model> {
    let mut mapped = models
        .into_iter()
        .map(|model| {
            let recommended = model.recommended || model.value == "default";
            let reasoning = reasoning_from_model(&model);
            Model {
                id: model.value,
                label: model.display_name,
                recommended,
                reasoning,
            }
        })
        .collect::<Vec<_>>();

    if mapped.is_empty() {
        mapped = fallback_models();
    }
    mapped
}

pub(crate) fn fallback_models() -> Vec<Model> {
    vec![
        Model {
            id: "default".to_string(),
            label: "Default (Claude Code)".to_string(),
            recommended: true,
            reasoning: Some(ReasoningCapabilities::from_efforts(
                vec![
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::XHigh,
                    ReasoningEffort::Max,
                ],
                false,
                false,
                false,
            )),
        },
        Model {
            id: "sonnet".to_string(),
            label: "Sonnet".to_string(),
            recommended: false,
            reasoning: Some(ReasoningCapabilities::from_efforts(
                vec![
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::Max,
                ],
                false,
                false,
                false,
            )),
        },
        Model {
            id: "haiku".to_string(),
            label: "Haiku".to_string(),
            recommended: false,
            reasoning: None,
        },
    ]
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
