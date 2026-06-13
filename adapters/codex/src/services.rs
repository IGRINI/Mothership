use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use mothership_adapter_sdk::media::{
    content_type_for_format, extension_for_content_type, split_base64_data_url,
};
use mothership_adapter_sdk::protocol::{
    ImageGenerationRequest, ImageGenerationResult, Model, ProviderMediaBlob, ProviderService,
    ProviderServiceMode, ProviderServiceModel, FEATURE_IMAGE_GENERATE,
};
use mothership_adapter_sdk::{http, sse};
use mothership_openai_responses as responses;
use serde_json::{json, Value};

use crate::auth;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CODEX_IMAGE_GENERATION_INSTRUCTIONS: &str =
    "Generate images from the user's prompt by using the image_generation tool. Return the generated image artifact from the tool call.";

pub(crate) fn service_catalog_from_models(models: Vec<Model>) -> Vec<ProviderService> {
    let mut service_models = models
        .into_iter()
        .filter(|model| model_supports_responses_image_generation(&model.id))
        .enumerate()
        .map(|(index, model)| ProviderServiceModel {
            id: model.id,
            label: model.label,
            recommended: index == 0,
            options_schema: image_generation_options_schema(),
        })
        .collect::<Vec<_>>();

    if service_models.is_empty() {
        return Vec::new();
    }

    if !service_models.iter().any(|model| model.recommended) {
        if let Some(first) = service_models.first_mut() {
            first.recommended = true;
        }
    }

    vec![ProviderService {
        feature: FEATURE_IMAGE_GENERATE.to_string(),
        label: "Image generation".to_string(),
        models: service_models,
        options_schema: image_generation_options_schema(),
        mode: ProviderServiceMode::Sync,
    }]
}

pub(crate) async fn generate_image(
    client: &reqwest::Client,
    endpoint: &responses::Endpoint,
    access_token: &str,
    account_id: Option<&str>,
    request: ImageGenerationRequest,
) -> Result<ImageGenerationResult> {
    if request.prompt.trim().is_empty() {
        bail!("image prompt cannot be empty");
    }

    let headers = auth::auth_headers(access_token, account_id);
    let body = codex_image_generation_body(&request);
    let response = http::post_stream_redacted(
        client,
        &endpoint.https_url,
        &headers,
        &body,
        http::DEFAULT_ERROR_BODY_TIMEOUT,
        http::DEFAULT_MAX_ERROR_BODY_CHARS,
        &[],
    )
    .await
    .context("send Codex image generation stream")?;
    let (images, metadata) = collect_response_images_stream(response).await?;
    if images.is_empty() {
        bail!("Codex response did not include generated images");
    }

    Ok(ImageGenerationResult { images, metadata })
}

fn codex_image_generation_body(request: &ImageGenerationRequest) -> Value {
    let image_tool = image_tool_from_options(&request.options);
    json!({
        "model": request.model,
        "instructions": CODEX_IMAGE_GENERATION_INSTRUCTIONS,
        "input": [image_prompt_input_item(&request.prompt)],
        "tools": [image_tool],
        "tool_choice": { "type": "image_generation" },
        "stream": true,
        "store": false,
    })
}

fn image_prompt_input_item(prompt: &str) -> Value {
    json!({
        "role": "user",
        "content": [{ "type": "input_text", "text": prompt }],
    })
}

fn image_tool_from_options(options: &Value) -> Value {
    let mut tool = serde_json::Map::new();
    tool.insert(
        "type".to_string(),
        Value::String("image_generation".to_string()),
    );

    if let Some(options) = options.as_object() {
        for key in [
            "action",
            "background",
            "moderation",
            "output_compression",
            "output_format",
            "partial_images",
            "quality",
            "size",
        ] {
            if let Some(value) = options.get(key) {
                tool.insert(key.to_string(), value.clone());
            }
        }
    }

    Value::Object(tool)
}

fn extract_response_images(payload: &Value) -> Result<Vec<ProviderMediaBlob>> {
    let mut images = Vec::new();
    let Some(output) = payload.get("output").and_then(Value::as_array) else {
        return Ok(images);
    };

    for item in output {
        push_image_generation_item(item, &mut images);
    }

    Ok(images)
}

async fn collect_response_images_stream(
    response: reqwest::Response,
) -> Result<(Vec<ProviderMediaBlob>, Value)> {
    let mut images = Vec::new();
    let mut metadata = Value::Null;

    sse::read_sse(response, HTTP_TIMEOUT, |payload| {
        if payload == "[DONE]" {
            return Ok(false);
        }
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            return Ok(true);
        };
        if let Some(message) = response_error_message(&event) {
            bail!("{message}");
        }
        collect_images_from_stream_event(&event, &mut images, &mut metadata);
        Ok(event.get("type").and_then(Value::as_str) != Some("response.completed"))
    })
    .await?;

    Ok((images, metadata))
}

fn collect_images_from_stream_event(
    event: &Value,
    images: &mut Vec<ProviderMediaBlob>,
    metadata: &mut Value,
) {
    if let Some(item) = event.get("item") {
        push_image_generation_item(item, images);
    }
    if let Some(response) = event.get("response") {
        if event.get("type").and_then(Value::as_str) == Some("response.completed") {
            *metadata = response_metadata(response);
        }
        if images.is_empty() {
            if let Ok(mut response_images) = extract_response_images(response) {
                images.append(&mut response_images);
            }
        }
    }
}

fn push_image_generation_item(item: &Value, images: &mut Vec<ProviderMediaBlob>) {
    if item.get("type").and_then(Value::as_str) != Some("image_generation_call") {
        return;
    }
    let Some(raw_result) = item.get("result").and_then(Value::as_str) else {
        return;
    };
    let output_format = item
        .get("output_format")
        .or_else(|| item.get("outputFormat"))
        .and_then(Value::as_str)
        .unwrap_or("png");
    let fallback_content_type = content_type_for_format(output_format);
    // Codex keeps the data-URL mime for the blob's content type but derives the
    // filename extension from `output_format` (via `fallback_content_type`), not
    // from the parsed mime. Preserve that quirk.
    let (content_type, bytes_base64) = split_base64_data_url(raw_result).unwrap_or_else(|| {
        (
            fallback_content_type.to_string(),
            raw_result.trim().to_string(),
        )
    });
    images.push(ProviderMediaBlob {
        content_type,
        bytes_base64,
        filename: Some(format!(
            "codex-image-{}.{}",
            images.len() + 1,
            extension_for_content_type(fallback_content_type)
        )),
        metadata: metadata_without_large_fields(item, &["result"]),
    });
}

fn response_error_message(event: &Value) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    if event_type != "response.failed" && event_type != "error" {
        return None;
    }
    event
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            event
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        .or_else(|| Some("Codex image generation stream failed".to_string()))
}

fn metadata_without_large_fields(value: &Value, large_keys: &[&str]) -> Value {
    let mut metadata = match value {
        Value::Object(object) => object.clone(),
        _ => return Value::Null,
    };
    for key in large_keys {
        metadata.remove(*key);
    }
    Value::Object(metadata)
}

fn response_metadata(payload: &Value) -> Value {
    json!({
        "id": payload.get("id").cloned().unwrap_or(Value::Null),
        "model": payload.get("model").cloned().unwrap_or(Value::Null),
        "usage": payload.get("usage").cloned().unwrap_or(Value::Null),
    })
}

fn image_generation_options_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["auto", "generate", "edit"]
            },
            "background": {
                "type": "string",
                "enum": ["auto", "opaque", "transparent"]
            },
            "output_compression": {
                "type": "integer",
                "minimum": 0,
                "maximum": 100
            },
            "output_format": {
                "type": "string",
                "enum": ["png", "jpeg", "webp"]
            },
            "quality": {
                "type": "string",
                "enum": ["auto", "low", "medium", "high"]
            },
            "size": {
                "type": "string",
                "enum": ["auto", "1024x1024", "1024x1536", "1536x1024"]
            }
        },
        "additionalProperties": false
    })
}

fn model_supports_responses_image_generation(model_id: &str) -> bool {
    let normalized = model_id
        .trim()
        .to_ascii_lowercase()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();

    matches!(
        normalized.as_str(),
        "gpt-5.5"
            | "gpt-5.4"
            | "gpt-5.4-mini"
            | "gpt-5.4-nano"
            | "gpt-5.2"
            | "gpt-5"
            | "gpt-5-nano"
            | "o3"
            | "gpt-4.1"
            | "gpt-4.1-mini"
            | "gpt-4.1-nano"
            | "gpt-4o"
            | "gpt-4o-mini"
    ) || normalized.starts_with("gpt-5.5-")
        || normalized.starts_with("gpt-5.4-")
        || normalized.starts_with("gpt-5.2-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_services_use_mainline_models_for_image_tool() {
        let services = service_catalog_from_models(vec![
            Model {
                id: "gpt-5.5".to_string(),
                label: "GPT-5.5".to_string(),
                recommended: true,
                reasoning: None,
                fast_mode: None,
            },
            Model {
                id: "gpt-5.3-codex-spark".to_string(),
                label: "GPT-5.3 Codex Spark".to_string(),
                recommended: false,
                reasoning: None,
                fast_mode: None,
            },
        ]);

        assert_eq!(services.len(), 1);
        assert_eq!(services[0].feature, FEATURE_IMAGE_GENERATE);
        assert_eq!(services[0].models.len(), 1);
        assert_eq!(services[0].models[0].id, "gpt-5.5");
    }

    #[test]
    fn codex_image_parser_strips_data_url_prefix() {
        let payload = json!({
            "id": "resp_1",
            "model": "gpt-5.5",
            "output": [
                {
                    "type": "image_generation_call",
                    "status": "completed",
                    "output_format": "png",
                    "result": "data:image/png;base64,Zm9v"
                }
            ]
        });

        let images = extract_response_images(&payload).expect("images");

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].content_type, "image/png");
        assert_eq!(images[0].bytes_base64, "Zm9v");
        assert!(images[0].metadata.get("result").is_none());
    }

    #[test]
    fn codex_image_body_includes_required_instructions() {
        let body = codex_image_generation_body(&ImageGenerationRequest {
            model: "gpt-5.5".to_string(),
            prompt: "fantasy idle game icon".to_string(),
            options: Value::Null,
        });

        assert_eq!(
            body.get("instructions").and_then(Value::as_str),
            Some(CODEX_IMAGE_GENERATION_INSTRUCTIONS)
        );
        assert!(body["input"].is_array());
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "fantasy idle game icon"
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"]["type"], "image_generation");
    }

    #[test]
    fn codex_image_stream_parser_collects_output_item_done() {
        let event = json!({
            "type": "response.output_item.done",
            "item": {
                "id": "ig_1",
                "type": "image_generation_call",
                "status": "completed",
                "output_format": "png",
                "result": "data:image/png;base64,Zm9v"
            }
        });
        let mut images = Vec::new();
        let mut metadata = Value::Null;

        collect_images_from_stream_event(&event, &mut images, &mut metadata);

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].content_type, "image/png");
        assert_eq!(images[0].bytes_base64, "Zm9v");
        assert!(images[0].metadata.get("result").is_none());
    }

    #[test]
    fn codex_image_stream_parser_uses_completed_response_fallback() {
        let event = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "model": "gpt-5.5",
                "output": [
                    {
                        "type": "image_generation_call",
                        "status": "completed",
                        "output_format": "webp",
                        "result": "Zm9v"
                    }
                ]
            }
        });
        let mut images = Vec::new();
        let mut metadata = Value::Null;

        collect_images_from_stream_event(&event, &mut images, &mut metadata);

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].content_type, "image/webp");
        assert_eq!(images[0].filename.as_deref(), Some("codex-image-1.webp"));
        assert_eq!(metadata["id"], "resp_1");
    }
}
