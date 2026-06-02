use std::collections::BTreeMap;

use anyhow::Context as _;
use mothership_adapter_sdk::http;
use mothership_adapter_sdk::protocol::{Model, ReasoningCapabilities, ReasoningEffort};
use mothership_adapter_sdk::reasoning::{
    collect_effort_values, collect_nested_efforts, collect_parameter_names, dedupe_efforts,
    value_signals_reasoning,
};
use serde::Deserialize;
use serde_json::Value;

use crate::settings::{auth_headers, OpenRouterSettings, HTTP_CONNECT_TIMEOUT};

pub(crate) fn parse_model_ids(spec: &str) -> Vec<String> {
    spec.split(['\n', ','])
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub(crate) fn models_from_user_list(
    ids: Vec<String>,
    metadata: &BTreeMap<String, OpenRouterModelMetadata>,
) -> Vec<Model> {
    ids.into_iter()
        .enumerate()
        .map(|(index, id)| {
            let remote = metadata.get(&id);
            Model {
                label: remote
                    .and_then(|metadata| metadata.name.clone())
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| id.clone()),
                reasoning: remote.and_then(openrouter_reasoning_capabilities),
                id,
                recommended: index == 0,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
struct OpenRouterModelsResponse {
    #[serde(default)]
    data: Vec<OpenRouterModelMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OpenRouterModelMetadata {
    id: String,
    #[serde(default)]
    canonical_slug: Option<String>,
    #[serde(default)]
    name: Option<String>,
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

pub(crate) async fn fetch_model_metadata(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
) -> anyhow::Result<BTreeMap<String, OpenRouterModelMetadata>> {
    let url = format!("{}/models", settings.base_url().trim_end_matches('/'));
    let mut request = client.get(url).timeout(HTTP_CONNECT_TIMEOUT);
    let api_key = settings.api_key();
    let headers = if api_key.is_empty() {
        Vec::new()
    } else {
        auth_headers(api_key)
    };
    for (name, value) in &headers {
        request = request.header(name, value);
    }

    let response = request
        .send()
        .await
        .context("fetch OpenRouter model catalog")?;
    let response = http::ensure_success_redacted(
        response,
        http::DEFAULT_ERROR_BODY_TIMEOUT,
        http::DEFAULT_MAX_ERROR_BODY_CHARS,
        &headers,
        &[api_key],
    )
    .await?;
    let payload: OpenRouterModelsResponse = response
        .json()
        .await
        .context("decode OpenRouter model catalog")?;
    let mut indexed = BTreeMap::new();
    for metadata in payload.data {
        if let Some(canonical_slug) = metadata.canonical_slug.as_ref() {
            indexed.insert(canonical_slug.clone(), metadata.clone());
        }
        indexed.insert(metadata.id.clone(), metadata);
    }
    Ok(indexed)
}

fn openrouter_reasoning_capabilities(
    metadata: &OpenRouterModelMetadata,
) -> Option<ReasoningCapabilities> {
    let mut efforts = Vec::new();
    collect_effort_values(&metadata.reasoning_efforts, &mut efforts);
    collect_effort_values(&metadata.supported_reasoning_efforts, &mut efforts);
    collect_effort_values(&metadata.effort_levels, &mut efforts);
    collect_effort_values(&metadata.supported_effort_levels, &mut efforts);
    collect_effort_values(&metadata.reasoning_levels, &mut efforts);
    collect_effort_values(&metadata.supported_reasoning_levels, &mut efforts);
    if let Some(reasoning) = &metadata.reasoning {
        collect_nested_efforts(reasoning, &mut efforts);
    }

    let parameters = collect_parameter_names(&metadata.supported_parameters);
    let has_reasoning = parameters.contains("reasoning");
    let has_reasoning_effort = parameters.contains("reasoning_effort");
    let has_include_reasoning = parameters.contains("include_reasoning");
    let has_reasoning_flag = metadata
        .reasoning
        .as_ref()
        .map(value_signals_reasoning)
        .unwrap_or(false)
        || metadata
            .capabilities
            .as_ref()
            .map(value_signals_reasoning)
            .unwrap_or(false)
        || metadata
            .features
            .as_ref()
            .map(value_signals_reasoning)
            .unwrap_or(false);
    if !(has_reasoning
        || has_reasoning_effort
        || has_include_reasoning
        || has_reasoning_flag
        || !efforts.is_empty())
    {
        return None;
    }

    if efforts.is_empty() && (has_reasoning || has_reasoning_effort) {
        efforts.extend(ReasoningEffort::openai_responses_values());
    }
    dedupe_efforts(&mut efforts);

    Some(ReasoningCapabilities::from_efforts(
        efforts,
        has_reasoning,
        has_reasoning || has_include_reasoning,
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_metadata_exposes_reasoning_capabilities() {
        let metadata = OpenRouterModelMetadata {
            id: "openai/gpt-test".to_string(),
            canonical_slug: Some("openai/gpt-test".to_string()),
            name: Some("GPT Test".to_string()),
            supported_parameters: json!(["temperature", "reasoning", "reasoning_effort"]),
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

        let reasoning = openrouter_reasoning_capabilities(&metadata).expect("reasoning");

        assert!(reasoning.supported);
        assert!(reasoning.supports_budget);
        assert!(reasoning.efforts.contains(&ReasoningEffort::High));
        assert_eq!(
            reasoning
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec!["auto", "none", "minimal", "low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn model_metadata_prefers_explicit_reasoning_levels() {
        let metadata = OpenRouterModelMetadata {
            id: "anthropic/claude-test".to_string(),
            canonical_slug: None,
            name: None,
            supported_parameters: json!(["reasoning"]),
            reasoning_efforts: Value::Null,
            supported_reasoning_efforts: Value::Null,
            effort_levels: Value::Null,
            supported_effort_levels: Value::Null,
            reasoning_levels: json!(["low", "medium", "high", "xhigh", "max"]),
            supported_reasoning_levels: Value::Null,
            reasoning: None,
            capabilities: None,
            features: None,
        };

        let reasoning = openrouter_reasoning_capabilities(&metadata).expect("reasoning");

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
}
