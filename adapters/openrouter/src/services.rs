use std::collections::BTreeMap;

use anyhow::{bail, Context as _, Result};
use mothership_adapter_sdk::http;
use mothership_adapter_sdk::media::extension_for_content_type;
use mothership_adapter_sdk::protocol::{
    ImageGenerationRequest, ImageGenerationResult, ProviderMediaBlob, ProviderService,
    ProviderServiceMode, ProviderServiceModel, FEATURE_IMAGE_GENERATE,
};
use serde_json::{json, Value};

use crate::models::{
    fetch_model_endpoint_metadata, fetch_model_metadata_for_ids, image_output_modalities,
    supports_image_output, OpenRouterModelMetadata,
};
use crate::settings::{auth_headers, OpenRouterSettings};

pub(crate) async fn service_catalog(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
) -> Result<Vec<ProviderService>> {
    if settings.api_key().is_empty() {
        return Ok(Vec::new());
    }

    let model_ids = crate::models::parse_model_ids(settings.image_models());
    if model_ids.is_empty() {
        return Ok(Vec::new());
    }
    let metadata = fetch_model_metadata_for_ids(client, settings, &model_ids).await;
    let models = image_service_models(model_ids, &metadata);
    if models.is_empty() {
        return Ok(Vec::new());
    }

    Ok(vec![ProviderService {
        feature: FEATURE_IMAGE_GENERATE.to_string(),
        label: "Image generation".to_string(),
        models,
        options_schema: image_generation_options_schema(),
        mode: ProviderServiceMode::Sync,
    }])
}

pub(crate) async fn generate_image(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
    request: ImageGenerationRequest,
) -> Result<ImageGenerationResult> {
    if settings.api_key().is_empty() {
        bail!("missing OpenRouter API key (set it in adapter settings)");
    }
    if request.prompt.trim().is_empty() {
        bail!("image prompt cannot be empty");
    }

    let modalities = image_modalities_for_model(client, settings, &request.model).await;
    let body = openrouter_image_generation_body(&request, modalities);
    let url = format!(
        "{}/chat/completions",
        settings.base_url().trim_end_matches('/')
    );
    let api_key = settings.api_key();
    let headers = auth_headers(api_key);
    let mut builder = client.post(url).json(&body);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }

    let response = builder
        .send()
        .await
        .context("send OpenRouter image generation request")?;
    let response = http::ensure_success_redacted(
        response,
        http::DEFAULT_ERROR_BODY_TIMEOUT,
        http::DEFAULT_MAX_ERROR_BODY_CHARS,
        &headers,
        &[api_key],
    )
    .await?;
    let payload: Value = response
        .json()
        .await
        .context("decode OpenRouter image generation response")?;
    let images = extract_openrouter_images(&payload)?;
    if images.is_empty() {
        bail!("OpenRouter response did not include generated images");
    }

    Ok(ImageGenerationResult {
        images,
        metadata: response_metadata(&payload),
    })
}

fn image_service_models(
    model_ids: Vec<String>,
    metadata: &BTreeMap<String, OpenRouterModelMetadata>,
) -> Vec<ProviderServiceModel> {
    model_ids
        .into_iter()
        .enumerate()
        .map(|(index, id)| {
            let remote = metadata.get(&id);
            ProviderServiceModel {
                id: id.clone(),
                label: remote
                    .and_then(|item| item.name.clone())
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(id),
                recommended: index == 0,
                options_schema: image_generation_options_schema(),
            }
        })
        .collect()
}

async fn image_modalities_for_model(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
    model_id: &str,
) -> Vec<String> {
    match fetch_model_endpoint_metadata(client, settings, model_id).await {
        Ok(metadata) if supports_image_output(&metadata) => image_output_modalities(&metadata),
        Ok(metadata) => {
            eprintln!(
                "openrouter-adapter: configured image model {} did not report image output; sending default image/text modalities",
                metadata.id
            );
            default_image_modalities()
        }
        Err(error) => {
            eprintln!(
                "openrouter-adapter: image model metadata refresh failed for {model_id}: {error:#}; sending default image/text modalities"
            );
            default_image_modalities()
        }
    }
}

fn default_image_modalities() -> Vec<String> {
    vec!["image".to_string(), "text".to_string()]
}

fn openrouter_image_generation_body(
    request: &ImageGenerationRequest,
    modalities: Vec<String>,
) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": [
            {
                "role": "user",
                "content": request.prompt
            }
        ],
        "modalities": modalities,
        "stream": false,
    });

    apply_image_options(&mut body, &request.options);
    body
}

fn apply_image_options(body: &mut Value, options: &Value) {
    let Some(options) = options.as_object() else {
        return;
    };
    let Some(body_object) = body.as_object_mut() else {
        return;
    };

    if let Some(modalities) = options.get("modalities") {
        body_object.insert("modalities".to_string(), modalities.clone());
    }
    if let Some(image_config) = options.get("image_config") {
        body_object.insert("image_config".to_string(), image_config.clone());
        return;
    }

    let mut image_config = serde_json::Map::new();
    for key in [
        "aspect_ratio",
        "background_hex_color",
        "background_mode",
        "background_rgb_color",
        "image_size",
        "scoring_prompt",
        "scoring_rubric",
        "strength",
        "style",
    ] {
        if let Some(value) = options.get(key) {
            image_config.insert(key.to_string(), value.clone());
        }
    }
    if !image_config.is_empty() {
        body_object.insert("image_config".to_string(), Value::Object(image_config));
    }
}

fn extract_openrouter_images(payload: &Value) -> Result<Vec<ProviderMediaBlob>> {
    let mut images = Vec::new();
    let Some(choices) = payload.get("choices").and_then(Value::as_array) else {
        return Ok(images);
    };

    for choice in choices {
        let Some(message_images) = choice
            .get("message")
            .and_then(|message| message.get("images"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for image in message_images {
            let Some(url) = image_url(image) else {
                continue;
            };
            let (content_type, bytes_base64) = split_base64_image_url(url)?;
            let fallback_extension = extension_for_content_type(&content_type);
            images.push(ProviderMediaBlob {
                content_type,
                bytes_base64,
                filename: Some(format!(
                    "openrouter-image-{}.{}",
                    images.len() + 1,
                    fallback_extension
                )),
                metadata: metadata_without_image_url(image),
            });
        }
    }

    Ok(images)
}

fn image_url(image: &Value) -> Option<&str> {
    image
        .get("image_url")
        .or_else(|| image.get("imageUrl"))
        .and_then(|image_url| image_url.get("url"))
        .and_then(Value::as_str)
}

fn split_base64_image_url(value: &str) -> Result<(String, String)> {
    let trimmed = value.trim();
    let Some(rest) = trimmed.strip_prefix("data:") else {
        bail!("OpenRouter returned an image URL that is not a base64 data URL");
    };
    let Some((mime, data)) = rest.split_once(";base64,") else {
        bail!("OpenRouter returned an unsupported image data URL");
    };
    Ok((mime.to_string(), data.to_string()))
}

fn metadata_without_image_url(value: &Value) -> Value {
    let mut metadata = match value {
        Value::Object(object) => object.clone(),
        _ => return Value::Null,
    };
    if let Some(Value::Object(image_url)) = metadata.get_mut("image_url") {
        image_url.remove("url");
    }
    if let Some(Value::Object(image_url)) = metadata.get_mut("imageUrl") {
        image_url.remove("url");
    }
    Value::Object(metadata)
}

fn response_metadata(payload: &Value) -> Value {
    let content = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .cloned()
        .unwrap_or(Value::Null);

    json!({
        "id": payload.get("id").cloned().unwrap_or(Value::Null),
        "model": payload.get("model").cloned().unwrap_or(Value::Null),
        "usage": payload.get("usage").cloned().unwrap_or(Value::Null),
        "content": content,
    })
}

fn image_generation_options_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "aspect_ratio": {
                "type": "string",
                "enum": ["1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9"]
            },
            "image_size": {
                "type": "string",
                "enum": ["0.5K", "1K", "2K", "4K"]
            },
            "image_config": {
                "type": "object"
            },
            "modalities": {
                "type": "array",
                "items": {
                    "type": "string",
                    "enum": ["image", "text"]
                }
            }
        },
        "additionalProperties": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::OpenRouterModelArchitecture;

    #[test]
    fn image_service_models_keep_configured_order() {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "google/gemini-image".to_string(),
            OpenRouterModelMetadata {
                id: "google/gemini-image".to_string(),
                canonical_slug: Some("google/gemini-image".to_string()),
                name: Some("Gemini Image".to_string()),
                architecture: Some(OpenRouterModelArchitecture {
                    output_modalities: vec!["text".to_string(), "image".to_string()],
                    modality: Some("text->text+image".to_string()),
                }),
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
            },
        );

        let models = image_service_models(
            vec![
                "black-forest-labs/flux.2-pro".to_string(),
                "google/gemini-image".to_string(),
            ],
            &metadata,
        );

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "black-forest-labs/flux.2-pro");
        assert_eq!(models[0].label, "black-forest-labs/flux.2-pro");
        assert!(models[0].recommended);
        assert_eq!(models[1].id, "google/gemini-image");
        assert_eq!(models[1].label, "Gemini Image");
    }

    #[test]
    fn openrouter_image_parser_strips_data_url() {
        let payload = json!({
            "id": "gen_1",
            "choices": [
                {
                    "message": {
                        "content": "done",
                        "images": [
                            {
                                "type": "image_url",
                                "image_url": {
                                    "url": "data:image/png;base64,Zm9v"
                                }
                            }
                        ]
                    }
                }
            ]
        });

        let images = extract_openrouter_images(&payload).expect("images");

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].content_type, "image/png");
        assert_eq!(images[0].bytes_base64, "Zm9v");
        assert!(images[0].metadata["image_url"].get("url").is_none());
    }
}
