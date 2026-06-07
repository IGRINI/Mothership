use std::time::Duration;

use anyhow::Result;
use mothership_adapter_sdk::http;
use mothership_adapter_sdk::protocol::{
    FastModeCapabilities, Model, ReasoningCapabilities, ReasoningEffort,
};
use mothership_adapter_sdk::reasoning::{
    collect_effort_values, collect_nested_efforts, collect_parameter_names, dedupe_efforts,
    value_signals_reasoning, value_signals_reasoning_summary,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth;

const MODELS_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/models";
const CLIENT_VERSION: &str = "0.133.0";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const CODEX_FAST_SERVICE_TIER: &str = "priority";

#[derive(Debug, Deserialize)]
struct RemoteModel {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    service_tiers: Value,
    #[serde(default)]
    additional_speed_tiers: Value,
    #[serde(default)]
    supported_parameters: Value,
    #[serde(default)]
    reasoning_efforts: Value,
    #[serde(default)]
    supported_reasoning_efforts: Value,
    #[serde(default)]
    effort_levels: Value,
    #[serde(default)]
    supported_effort_levels: Value,
    #[serde(default)]
    reasoning_levels: Value,
    #[serde(default)]
    supported_reasoning_levels: Value,
    #[serde(default)]
    reasoning: Option<Value>,
    #[serde(default)]
    capabilities: Option<Value>,
    #[serde(default)]
    features: Option<Value>,
}

pub(crate) async fn fetch_models(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<Vec<Model>> {
    let headers = auth::auth_headers(access_token, account_id);
    let mut request = client
        .get(format!("{MODELS_ENDPOINT}?client_version={CLIENT_VERSION}"))
        .timeout(HTTP_TIMEOUT);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let response = http::ensure_success_redacted(
        response,
        http::DEFAULT_ERROR_BODY_TIMEOUT,
        http::DEFAULT_MAX_ERROR_BODY_CHARS,
        &headers,
        &[],
    )
    .await?;
    let payload: serde_json::Value = response.json().await?;
    let mut models: Vec<RemoteModel> =
        serde_json::from_value(payload.get("models").cloned().unwrap_or(json!([])))?;
    models.sort_by_key(|model| model.priority.unwrap_or(i64::MAX));

    Ok(models
        .into_iter()
        .filter(should_show_codex_model)
        .enumerate()
        .map(|(index, model)| {
            let reasoning = codex_reasoning_capabilities(&model);
            let fast_mode = codex_fast_mode_capabilities(&model);
            Model {
                label: model.display_name.unwrap_or_else(|| model.slug.clone()),
                reasoning,
                id: model.slug,
                recommended: index == 0,
                fast_mode,
            }
        })
        .collect())
}

pub(crate) fn codex_fast_service_tier_for_model(
    model_id: &str,
    fast_mode: bool,
) -> Option<&'static str> {
    (fast_mode && codex_model_supports_fast_mode(model_id)).then_some(CODEX_FAST_SERVICE_TIER)
}

fn codex_model_supports_fast_mode(model_id: &str) -> bool {
    matches!(model_id.trim(), "gpt-5.5" | "gpt-5.4")
}

fn should_show_codex_model(model: &RemoteModel) -> bool {
    // `supported_in_api` only applies to public/API-key access. This adapter uses
    // Codex OAuth against the ChatGPT Codex backend, where backend-only models
    // such as GPT-5.3 Codex Spark are valid as long as the catalog lists them.
    model.visibility.as_deref().unwrap_or("list") == "list"
}

fn codex_reasoning_capabilities(model: &RemoteModel) -> Option<ReasoningCapabilities> {
    let mut efforts = Vec::new();
    collect_effort_values(&model.reasoning_efforts, &mut efforts);
    collect_effort_values(&model.supported_reasoning_efforts, &mut efforts);
    collect_effort_values(&model.effort_levels, &mut efforts);
    collect_effort_values(&model.supported_effort_levels, &mut efforts);
    collect_effort_values(&model.reasoning_levels, &mut efforts);
    collect_effort_values(&model.supported_reasoning_levels, &mut efforts);
    if let Some(reasoning) = &model.reasoning {
        collect_nested_efforts(reasoning, &mut efforts);
    }

    let parameters = collect_parameter_names(&model.supported_parameters);
    let has_reasoning_parameter = parameters.contains("reasoning");
    let has_reasoning_effort_parameter = parameters.contains("reasoning_effort");
    let has_reasoning_flag = model
        .reasoning
        .as_ref()
        .map(value_signals_reasoning)
        .unwrap_or(false)
        || model
            .capabilities
            .as_ref()
            .map(value_signals_reasoning)
            .unwrap_or(false)
        || model
            .features
            .as_ref()
            .map(value_signals_reasoning)
            .unwrap_or(false);

    let supported = has_reasoning_parameter
        || has_reasoning_effort_parameter
        || has_reasoning_flag
        || !efforts.is_empty();
    if !supported {
        return None;
    }
    if efforts.is_empty() && (has_reasoning_parameter || has_reasoning_effort_parameter) {
        efforts.extend(ReasoningEffort::openai_responses_values());
    }
    dedupe_efforts(&mut efforts);

    Some(ReasoningCapabilities::from_efforts(
        efforts,
        has_reasoning_parameter,
        false,
        model
            .reasoning
            .as_ref()
            .map(value_signals_reasoning_summary)
            .unwrap_or(false),
    ))
}

fn codex_fast_mode_capabilities(model: &RemoteModel) -> Option<FastModeCapabilities> {
    let service_tier = priority_service_tier(&model.service_tiers);
    let speed_tier_alias = value_contains_string(&model.additional_speed_tiers, "fast");
    if service_tier.is_none() && !speed_tier_alias && !codex_model_supports_fast_mode(&model.slug) {
        return None;
    }

    let (label, description) = service_tier.unwrap_or_else(|| {
        (
            "Fast".to_string(),
            Some("1.5x speed, increased usage".to_string()),
        )
    });
    Some(FastModeCapabilities::supported(label, description))
}

fn priority_service_tier(value: &Value) -> Option<(String, Option<String>)> {
    match value {
        Value::Array(items) => items.iter().find_map(priority_service_tier),
        Value::Object(object) => {
            let id = object.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if id == CODEX_FAST_SERVICE_TIER || name.eq_ignore_ascii_case("fast") {
                let label = object
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or("Fast")
                    .to_string();
                let description = object
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|description| !description.trim().is_empty())
                    .map(ToOwned::to_owned);
                return Some((label, description));
            }
            object
                .get(CODEX_FAST_SERVICE_TIER)
                .and_then(priority_service_tier)
                .or_else(|| object.values().find_map(priority_service_tier))
        }
        Value::String(text)
            if text == CODEX_FAST_SERVICE_TIER || text.eq_ignore_ascii_case("fast") =>
        {
            Some(("Fast".to_string(), None))
        }
        _ => None,
    }
}

fn value_contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(text) => text.eq_ignore_ascii_case(expected),
        Value::Array(items) => items
            .iter()
            .any(|item| value_contains_string(item, expected)),
        Value::Object(object) => {
            object.keys().any(|key| key.eq_ignore_ascii_case(expected))
                || object
                    .values()
                    .any(|item| value_contains_string(item, expected))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_model_parser_extracts_reasoning_efforts() {
        let model = RemoteModel {
            slug: "gpt-test".to_string(),
            display_name: Some("GPT Test".to_string()),
            visibility: Some("list".to_string()),
            priority: Some(1),
            service_tiers: Value::Null,
            additional_speed_tiers: Value::Null,
            supported_parameters: json!(["reasoning"]),
            reasoning_efforts: json!(["low", "high"]),
            supported_reasoning_efforts: Value::Null,
            effort_levels: Value::Null,
            supported_effort_levels: Value::Null,
            reasoning_levels: Value::Null,
            supported_reasoning_levels: Value::Null,
            reasoning: Some(json!({ "summary": ["auto"] })),
            capabilities: None,
            features: None,
        };

        let reasoning = codex_reasoning_capabilities(&model).expect("reasoning");

        assert!(reasoning.supported);
        assert!(reasoning.supports_budget);
        assert!(reasoning.supports_summary);
        assert_eq!(
            reasoning.efforts,
            vec![ReasoningEffort::Low, ReasoningEffort::High]
        );
        assert_eq!(
            reasoning
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec!["auto", "low", "high"]
        );
    }

    #[test]
    fn codex_model_parser_does_not_treat_unrelated_true_flags_as_reasoning() {
        let model = RemoteModel {
            slug: "vision-test".to_string(),
            display_name: None,
            visibility: Some("list".to_string()),
            priority: None,
            service_tiers: Value::Null,
            additional_speed_tiers: Value::Null,
            supported_parameters: Value::Null,
            reasoning_efforts: Value::Null,
            supported_reasoning_efforts: Value::Null,
            effort_levels: Value::Null,
            supported_effort_levels: Value::Null,
            reasoning_levels: Value::Null,
            supported_reasoning_levels: Value::Null,
            reasoning: None,
            capabilities: Some(json!({ "vision": true, "tools": true })),
            features: Some(json!({ "attachments": true })),
        };

        assert!(codex_reasoning_capabilities(&model).is_none());
    }

    #[test]
    fn codex_model_parser_ignores_disabled_reasoning_summary() {
        let model = RemoteModel {
            slug: "gpt-test".to_string(),
            display_name: None,
            visibility: Some("list".to_string()),
            priority: None,
            service_tiers: Value::Null,
            additional_speed_tiers: Value::Null,
            supported_parameters: json!(["reasoning"]),
            reasoning_efforts: Value::Null,
            supported_reasoning_efforts: Value::Null,
            effort_levels: Value::Null,
            supported_effort_levels: Value::Null,
            reasoning_levels: Value::Null,
            supported_reasoning_levels: Value::Null,
            reasoning: Some(json!({ "summary": false })),
            capabilities: None,
            features: None,
        };

        let reasoning = codex_reasoning_capabilities(&model).expect("reasoning");

        assert!(!reasoning.supports_summary);
    }

    #[test]
    fn codex_model_parser_does_not_guess_reasoning_from_model_name() {
        let model = RemoteModel {
            slug: "gpt-5.5".to_string(),
            display_name: Some("GPT-5.5".to_string()),
            visibility: Some("list".to_string()),
            priority: None,
            service_tiers: Value::Null,
            additional_speed_tiers: Value::Null,
            supported_parameters: Value::Null,
            reasoning_efforts: Value::Null,
            supported_reasoning_efforts: Value::Null,
            effort_levels: Value::Null,
            supported_effort_levels: Value::Null,
            reasoning_levels: Value::Null,
            supported_reasoning_levels: Value::Null,
            reasoning: None,
            capabilities: None,
            features: None,
        };

        assert!(codex_reasoning_capabilities(&model).is_none());
    }

    #[test]
    fn codex_model_parser_keeps_claude_style_xhigh_and_max_separate() {
        let model = RemoteModel {
            slug: "provider-model".to_string(),
            display_name: None,
            visibility: Some("list".to_string()),
            priority: None,
            service_tiers: Value::Null,
            additional_speed_tiers: Value::Null,
            supported_parameters: Value::Null,
            reasoning_efforts: Value::Null,
            supported_reasoning_efforts: Value::Null,
            effort_levels: Value::Null,
            supported_effort_levels: Value::Null,
            reasoning_levels: json!(["low", "medium", "high", "xhigh", "max"]),
            supported_reasoning_levels: Value::Null,
            reasoning: None,
            capabilities: None,
            features: Some(json!({ "reasoning": true })),
        };

        let reasoning = codex_reasoning_capabilities(&model).expect("reasoning");

        assert_eq!(
            reasoning.efforts,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max,
            ]
        );
        assert_eq!(
            reasoning
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec!["auto", "low", "medium", "high", "xhigh", "max"]
        );
    }

    #[test]
    fn codex_model_parser_tolerates_metadata_maps() {
        let mut models: Vec<RemoteModel> = serde_json::from_value(json!([
            {
                "slug": "provider-model",
                "visibility": "list",
                "supported_in_api": true,
                "supported_parameters": [
                    { "name": "temperature", "supported": true },
                    { "name": "reasoning", "supported": true }
                ],
                "reasoning_efforts": [
                    { "id": "low", "label": "Low" },
                    { "id": "high", "label": "High" }
                ]
            }
        ]))
        .expect("metadata maps should not break model decoding");
        let model = models.pop().expect("model");

        let reasoning = codex_reasoning_capabilities(&model).expect("reasoning");

        assert_eq!(
            reasoning.efforts,
            vec![ReasoningEffort::Low, ReasoningEffort::High]
        );
    }

    #[test]
    fn codex_model_parser_extracts_fast_mode_from_service_tiers() {
        let model: RemoteModel = serde_json::from_value(json!({
            "slug": "gpt-5.5",
            "display_name": "GPT-5.5",
            "visibility": "list",
            "service_tiers": [
                { "id": "default", "name": "Standard" },
                {
                    "id": "priority",
                    "name": "Fast",
                    "description": "1.5x speed, increased usage"
                }
            ],
            "additional_speed_tiers": ["fast"]
        }))
        .expect("codex model metadata");

        let fast = codex_fast_mode_capabilities(&model).expect("fast mode");

        assert!(fast.supported);
        assert_eq!(fast.label, "Fast");
        assert_eq!(
            fast.description.as_deref(),
            Some("1.5x speed, increased usage")
        );
    }

    #[test]
    fn codex_fast_service_tier_only_applies_to_fast_capable_models() {
        assert_eq!(
            codex_fast_service_tier_for_model("gpt-5.5", true),
            Some(CODEX_FAST_SERVICE_TIER)
        );
        assert_eq!(
            codex_fast_service_tier_for_model("gpt-5.4-mini", true),
            None
        );
        assert_eq!(codex_fast_service_tier_for_model("gpt-5.5", false), None);
    }

    #[test]
    fn codex_model_picker_keeps_backend_only_models() {
        let model: RemoteModel = serde_json::from_value(json!({
            "slug": "gpt-5.3-codex-spark",
            "display_name": "GPT-5.3-Codex-Spark",
            "visibility": "list",
            "supported_in_api": false,
            "priority": 26
        }))
        .expect("backend-only model metadata");

        assert!(should_show_codex_model(&model));
    }

    #[test]
    fn codex_model_picker_hides_hidden_models() {
        let model = RemoteModel {
            slug: "codex-auto-review".to_string(),
            display_name: Some("Codex Auto Review".to_string()),
            visibility: Some("hide".to_string()),
            priority: Some(43),
            service_tiers: Value::Null,
            additional_speed_tiers: Value::Null,
            supported_parameters: Value::Null,
            reasoning_efforts: Value::Null,
            supported_reasoning_efforts: Value::Null,
            effort_levels: Value::Null,
            supported_effort_levels: Value::Null,
            reasoning_levels: Value::Null,
            supported_reasoning_levels: Value::Null,
            reasoning: None,
            capabilities: None,
            features: None,
        };

        assert!(!should_show_codex_model(&model));
    }
}
