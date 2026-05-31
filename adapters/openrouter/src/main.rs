//! OpenRouter provider adapter.
//!
//! Provider-specific logic only: settings, user-defined model list, and the
//! OpenAI-compatible streaming HTTP call. The shared adapter SDK owns stdio
//! framing, request dispatch, and cooperative chat cancellation.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context as _;
use mothership_adapter_sdk::protocol::{
    AuthKind, ChatMessage, Model, ModelManagement, SettingsField, SettingsFieldKind,
};
use mothership_adapter_sdk::{Context, ProviderAdapter};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const SSE_BUFFER_BYTES: usize = 8192;
const MAX_SSE_LINE_BYTES: usize = 256 * 1024;
const MAX_ERROR_BODY_CHARS: usize = 300;

#[derive(Default)]
struct OpenRouterSettings {
    api_key: String,
    base_url: String,
    models: String,
}

struct OpenRouterAdapter {
    client: reqwest::Client,
    settings: OpenRouterSettings,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    mothership_adapter_sdk::run(OpenRouterAdapter {
        client: http_client()?,
        settings: OpenRouterSettings::default(),
    })
    .await
}

#[async_trait::async_trait]
impl ProviderAdapter for OpenRouterAdapter {
    fn identity(&self) -> (String, String) {
        ("openrouter".to_string(), "OpenRouter".to_string())
    }

    fn settings_schema(&self) -> Vec<SettingsField> {
        vec![
            SettingsField {
                key: "api_key".to_string(),
                label: "OpenRouter API key".to_string(),
                kind: SettingsFieldKind::Secret,
                required: true,
            },
            SettingsField {
                key: "base_url".to_string(),
                label: "Base URL (optional)".to_string(),
                kind: SettingsFieldKind::Text,
                required: false,
            },
            SettingsField {
                key: "models".to_string(),
                label: "Models".to_string(),
                kind: SettingsFieldKind::StringList,
                required: false,
            },
        ]
    }

    fn auth_schema(&self) -> AuthKind {
        AuthKind::ApiKey {
            label: "OpenRouter API key".to_string(),
        }
    }

    async fn set_settings(&mut self, values: BTreeMap<String, String>) -> anyhow::Result<()> {
        self.settings.api_key = values.get("api_key").cloned().unwrap_or_default();
        self.settings.base_url = values.get("base_url").cloned().unwrap_or_default();
        self.settings.models = values.get("models").cloned().unwrap_or_default();
        Ok(())
    }

    async fn models(&mut self, _ctx: &Context) -> anyhow::Result<(ModelManagement, Vec<Model>)> {
        Ok((ModelManagement::UserDefined, parse_models(&self.settings.models)))
    }

    async fn chat(
        &mut self,
        model: &str,
        messages: Vec<ChatMessage>,
        _ctx: &Context,
        sink: &mut mothership_adapter_sdk::ChatSink,
    ) -> anyhow::Result<()> {
        stream_chat(&self.client, &self.settings, model, &messages, sink).await
    }
}

fn parse_models(spec: &str) -> Vec<Model> {
    spec.split(['\n', ','])
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .enumerate()
        .map(|(index, id)| Model {
            id: id.to_string(),
            label: id.to_string(),
            recommended: index == 0,
        })
        .collect()
}

async fn stream_chat(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
    model: &str,
    messages: &[ChatMessage],
    sink: &mut mothership_adapter_sdk::ChatSink,
) -> anyhow::Result<()> {
    if settings.api_key.trim().is_empty() {
        anyhow::bail!("missing OpenRouter API key (set it in adapter settings)");
    }
    let base = if settings.base_url.trim().is_empty() {
        DEFAULT_BASE_URL
    } else {
        settings.base_url.trim()
    };
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "messages": messages
            .iter()
            .map(|message| serde_json::json!({ "role": message.role, "content": message.content }))
            .collect::<Vec<_>>(),
        "stream": true,
    });

    let request = client
        .post(&url)
        .bearer_auth(settings.api_key.trim())
        .json(&body)
        .send();
    let response = tokio::select! {
        result = request => result.context("send OpenRouter chat request")?,
        _ = sink.cancelled() => return Ok(()),
    };

    if !response.status().is_success() {
        let status = response.status();
        let text = read_error_body(response).await;
        let text = sanitize_provider_error(&text, settings.api_key.trim());
        anyhow::bail!(
            "OpenRouter HTTP {status}: {}",
            text.chars().take(MAX_ERROR_BODY_CHARS).collect::<String>()
        );
    }

    read_sse_stream(response, sink).await
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .context("build OpenRouter HTTP client")
}

async fn read_error_body(response: reqwest::Response) -> String {
    match tokio::time::timeout(STREAM_IDLE_TIMEOUT, response.text()).await {
        Ok(Ok(text)) => text,
        Ok(Err(_)) | Err(_) => String::new(),
    }
}

async fn read_sse_stream(
    mut response: reqwest::Response,
    sink: &mut mothership_adapter_sdk::ChatSink,
) -> anyhow::Result<()> {
    let mut pending = Vec::with_capacity(SSE_BUFFER_BYTES);

    loop {
        let chunk = tokio::select! {
            result = tokio::time::timeout(STREAM_IDLE_TIMEOUT, response.chunk()) => {
                result
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "OpenRouter SSE stream idle timeout after {} seconds",
                            STREAM_IDLE_TIMEOUT.as_secs()
                        )
                    })?
                    .context("read OpenRouter SSE stream")?
            }
            _ = sink.cancelled() => return Ok(()),
        };
        let Some(chunk) = chunk else {
            return Ok(());
        };
        if chunk.is_empty() {
            continue;
        }
        pending.extend_from_slice(&chunk);
        if pending.len() > MAX_SSE_LINE_BYTES && !pending.contains(&b'\n') {
            anyhow::bail!(
                "OpenRouter SSE line exceeded {} bytes without newline",
                MAX_SSE_LINE_BYTES
            );
        }
        while let Some(newline_index) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line = pending.drain(..=newline_index).collect::<Vec<_>>();
            trim_line_ending(&mut line);
            let line = std::str::from_utf8(&line).context("decode OpenRouter SSE line as UTF-8")?;
            if handle_sse_line(line, sink)? {
                return Ok(());
            }
        }
        if pending.len() > MAX_SSE_LINE_BYTES {
            anyhow::bail!(
                "OpenRouter SSE line exceeded {} bytes without newline",
                MAX_SSE_LINE_BYTES
            );
        }
    }
}

fn handle_sse_line(line: &str, sink: &mothership_adapter_sdk::ChatSink) -> anyhow::Result<bool> {
    let Some(data) = line.trim().strip_prefix("data:") else {
        return Ok(false);
    };
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(true);
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
        if let Some(delta) = value["choices"][0]["delta"]["content"].as_str() {
            if !delta.is_empty() {
                sink.delta(delta);
            }
        }
    }
    Ok(false)
}

fn trim_line_ending(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
}

fn sanitize_provider_error(text: &str, api_key: &str) -> String {
    let redacted = if api_key.is_empty() {
        text.to_string()
    } else {
        text.replace(api_key, "[redacted]")
    };

    redacted
        .lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if lower.contains("authorization")
                || lower.contains("api-key")
                || lower.contains("api_key")
            {
                "[redacted sensitive provider error line]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
