use std::collections::BTreeMap;

use anyhow::Context as _;
use mothership_adapter_sdk::http;
use mothership_adapter_sdk::protocol::{FastModeCapabilities, Model, ReasoningCapabilities};
use mothership_adapter_sdk::provider_metadata::{
    harvest_reasoning_capabilities, ReasoningHarvestOptions, ReasoningMetadataFields,
};
use serde::Deserialize;

// `parse_model_ids` now lives in the SDK (shared with the Claude adapter).
// Re-export it so existing `crate::models::parse_model_ids` call sites resolve.
pub(crate) use mothership_adapter_sdk::provider_metadata::parse_model_ids;
use serde_json::Value;

use crate::settings::{auth_headers, OpenRouterSettings, HTTP_CONNECT_TIMEOUT};

pub(crate) const OPENROUTER_FAST_SERVICE_TIER: &str = "priority";

pub(crate) fn models_from_user_list(
    ids: Vec<String>,
    metadata: &BTreeMap<String, OpenRouterModelMetadata>,
) -> Vec<Model> {
    ids.into_iter()
        .enumerate()
        .map(|(index, id)| {
            let remote = metadata.get(&id);
            let fast_mode = remote
                .and_then(openrouter_fast_mode_capabilities)
                .or_else(|| openrouter_fast_mode_capabilities_for_model_id(&id));
            Model {
                label: remote
                    .and_then(|metadata| metadata.name.clone())
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| id.clone()),
                reasoning: remote.and_then(openrouter_reasoning_capabilities),
                id,
                recommended: index == 0,
                fast_mode,
            }
        })
        .collect()
}

pub(crate) fn openrouter_fast_service_tier_for_model(
    model_id: &str,
    fast_mode: bool,
) -> Option<&'static str> {
    (fast_mode && openrouter_model_supports_fast_mode(model_id))
        .then_some(OPENROUTER_FAST_SERVICE_TIER)
}

#[derive(Debug, Clone, Deserialize)]
struct OpenRouterModelEndpointResponse {
    data: OpenRouterModelMetadata,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OpenRouterModelMetadata {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) canonical_slug: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) architecture: Option<OpenRouterModelArchitecture>,
    #[serde(default)]
    pub(crate) supported_parameters: Value,
    #[serde(default)]
    pub(crate) reasoning_efforts: Value,
    #[serde(default)]
    pub(crate) supported_reasoning_efforts: Value,
    #[serde(default)]
    pub(crate) effort_levels: Value,
    #[serde(default)]
    pub(crate) supported_effort_levels: Value,
    #[serde(default)]
    pub(crate) reasoning_levels: Value,
    #[serde(default)]
    pub(crate) supported_reasoning_levels: Value,
    #[serde(default)]
    pub(crate) reasoning: Option<Value>,
    #[serde(default)]
    pub(crate) capabilities: Option<Value>,
    #[serde(default)]
    pub(crate) features: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OpenRouterModelArchitecture {
    #[serde(default)]
    pub(crate) output_modalities: Vec<String>,
    #[serde(default)]
    pub(crate) modality: Option<String>,
}

pub(crate) async fn fetch_model_metadata_for_ids(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
    model_ids: &[String],
) -> BTreeMap<String, OpenRouterModelMetadata> {
    let mut indexed = BTreeMap::new();
    for model_id in model_ids {
        match fetch_model_endpoint_metadata(client, settings, model_id).await {
            Ok(metadata) => {
                indexed.insert(model_id.clone(), metadata.clone());
                insert_model_metadata(&mut indexed, metadata);
            }
            Err(error) => {
                eprintln!("openrouter-adapter: metadata refresh failed for {model_id}: {error:#}");
            }
        }
    }
    indexed
}

pub(crate) async fn fetch_model_endpoint_metadata(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
    model_id: &str,
) -> anyhow::Result<OpenRouterModelMetadata> {
    let (author, slug) = model_id
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("OpenRouter model id `{model_id}` must be `author/slug`"))?;
    let base_url = settings.base_url().trim_end_matches('/');
    let url = format!("{base_url}/models/{author}/{slug}/endpoints");
    fetch_model_endpoint(client, settings, &url).await
}

async fn fetch_model_endpoint(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
    url: &str,
) -> anyhow::Result<OpenRouterModelMetadata> {
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
        .context("fetch OpenRouter model metadata")?;
    let response = http::ensure_success_redacted(
        response,
        http::DEFAULT_ERROR_BODY_TIMEOUT,
        http::DEFAULT_MAX_ERROR_BODY_CHARS,
        &headers,
        &[api_key],
    )
    .await?;
    let payload: OpenRouterModelEndpointResponse = response
        .json()
        .await
        .context("decode OpenRouter model metadata")?;
    Ok(payload.data)
}

fn insert_model_metadata(
    indexed: &mut BTreeMap<String, OpenRouterModelMetadata>,
    metadata: OpenRouterModelMetadata,
) {
    if let Some(canonical_slug) = metadata.canonical_slug.as_ref() {
        indexed.insert(canonical_slug.clone(), metadata.clone());
    }
    indexed.insert(metadata.id.clone(), metadata);
}

pub(crate) fn image_output_modalities(metadata: &OpenRouterModelMetadata) -> Vec<String> {
    let output_modalities = metadata
        .architecture
        .as_ref()
        .map(|architecture| architecture.output_modalities.as_slice())
        .unwrap_or_default();
    if output_modalities
        .iter()
        .any(|modality| modality.eq_ignore_ascii_case("text"))
    {
        vec!["image".to_string(), "text".to_string()]
    } else {
        vec!["image".to_string()]
    }
}

pub(crate) fn supports_image_output(metadata: &OpenRouterModelMetadata) -> bool {
    metadata.architecture.as_ref().is_some_and(|architecture| {
        architecture
            .output_modalities
            .iter()
            .any(|modality| modality.eq_ignore_ascii_case("image"))
            || architecture
                .modality
                .as_deref()
                .map(|modality| modality.to_ascii_lowercase().contains("->image"))
                .unwrap_or(false)
    })
}

fn openrouter_reasoning_capabilities(
    metadata: &OpenRouterModelMetadata,
) -> Option<ReasoningCapabilities> {
    harvest_reasoning_capabilities(
        ReasoningMetadataFields {
            supported_parameters: &metadata.supported_parameters,
            reasoning_efforts: &metadata.reasoning_efforts,
            supported_reasoning_efforts: &metadata.supported_reasoning_efforts,
            effort_levels: &metadata.effort_levels,
            supported_effort_levels: &metadata.supported_effort_levels,
            reasoning_levels: &metadata.reasoning_levels,
            supported_reasoning_levels: &metadata.supported_reasoning_levels,
            reasoning: metadata.reasoning.as_ref(),
            capabilities: metadata.capabilities.as_ref(),
            features: metadata.features.as_ref(),
        },
        ReasoningHarvestOptions {
            // OpenRouter exposes `include_reasoning` (reasoning exclusion) but
            // does not surface a reasoning summary.
            include_reasoning_parameter: true,
            detect_summary: false,
        },
    )
}

fn openrouter_fast_mode_capabilities(
    metadata: &OpenRouterModelMetadata,
) -> Option<FastModeCapabilities> {
    let supports_fast = openrouter_model_supports_fast_mode(&metadata.id)
        || metadata
            .canonical_slug
            .as_deref()
            .map(openrouter_model_supports_fast_mode)
            .unwrap_or(false);
    supports_fast.then(|| {
        FastModeCapabilities::supported(
            "Fast",
            Some("Priority service tier for faster responses.".to_string()),
        )
    })
}

fn openrouter_fast_mode_capabilities_for_model_id(model_id: &str) -> Option<FastModeCapabilities> {
    openrouter_model_supports_fast_mode(model_id).then(|| {
        FastModeCapabilities::supported(
            "Fast",
            Some("Priority service tier for faster responses.".to_string()),
        )
    })
}

fn openrouter_model_supports_fast_mode(model_id: &str) -> bool {
    let normalized = model_id
        .trim()
        .to_ascii_lowercase()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();
    matches!(normalized.as_str(), "openai/gpt-5.5" | "openai/gpt-5.4")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mothership_adapter_sdk::protocol::ReasoningEffort;
    use serde_json::json;

    #[test]
    fn model_metadata_exposes_reasoning_capabilities() {
        let metadata = OpenRouterModelMetadata {
            id: "openai/gpt-test".to_string(),
            canonical_slug: Some("openai/gpt-test".to_string()),
            name: Some("GPT Test".to_string()),
            architecture: None,
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
            architecture: None,
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

    #[test]
    fn model_metadata_exposes_fast_mode_for_priority_capable_models() {
        let metadata = OpenRouterModelMetadata {
            id: "openai/gpt-5.5".to_string(),
            canonical_slug: Some("openai/gpt-5.5".to_string()),
            name: Some("GPT-5.5".to_string()),
            architecture: None,
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

        let fast = openrouter_fast_mode_capabilities(&metadata).expect("fast mode");

        assert!(fast.supported);
        assert_eq!(
            openrouter_fast_service_tier_for_model("openai/gpt-5.5", true),
            Some(OPENROUTER_FAST_SERVICE_TIER)
        );
        assert_eq!(
            openrouter_fast_service_tier_for_model("openai/gpt-5.4-mini", true),
            None
        );
        assert_eq!(
            openrouter_fast_service_tier_for_model("openai/gpt-5.5", false),
            None
        );
    }
}
