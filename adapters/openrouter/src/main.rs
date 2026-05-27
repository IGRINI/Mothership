//! OpenRouter provider adapter.
//!
//! Speaks the Mothership adapter protocol over stdio and the provider's
//! OpenAI-compatible HTTP API. Its API key and user-defined model list arrive
//! via `set_settings` (the host pushes them from the app's shared credential
//! vault, keyed by provider, on each spawn). Chat is streamed from
//! `/chat/completions` (SSE) back to the host as deltas — the core never learns
//! it is plain HTTP underneath.

use std::io::{BufRead, BufReader, Write};

use mothership_adapter_host::protocol::{
    AuthKind, ChatMessage, Model, ModelManagement, Outbound, Request, SettingsField,
    SettingsFieldKind, PROTOCOL_VERSION,
};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

#[derive(Default)]
struct Settings {
    api_key: String,
    base_url: String,
    /// The user's model-id list. A `string_list` setting, stored as items joined
    /// by `\n` (the host's list editor); also tolerates commas, just in case.
    models: String,
}

fn main() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    let mut settings = Settings::default();
    let client = reqwest::blocking::Client::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<Request>(trimmed)? {
            Request::Initialize { id, .. } => emit(
                &mut stdout,
                &Outbound::Initialized {
                    id,
                    protocol_version: PROTOCOL_VERSION,
                },
            )?,
            Request::GetIdentity { id } => emit(
                &mut stdout,
                &Outbound::Identity {
                    id,
                    provider_id: "openrouter".to_string(),
                    provider_label: "OpenRouter".to_string(),
                },
            )?,
            Request::GetSettingsSchema { id } => emit(
                &mut stdout,
                &Outbound::SettingsSchema {
                    id,
                    fields: vec![
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
                    ],
                },
            )?,
            Request::SetSettings { id, values } => {
                if let Some(value) = values.get("api_key") {
                    settings.api_key = value.clone();
                }
                if let Some(value) = values.get("base_url") {
                    settings.base_url = value.clone();
                }
                if let Some(value) = values.get("models") {
                    settings.models = value.clone();
                }
                emit(&mut stdout, &Outbound::Ack { id })?;
            }
            Request::GetAuthSchema { id } => emit(
                &mut stdout,
                &Outbound::AuthSchema {
                    id,
                    auth: AuthKind::ApiKey {
                        label: "OpenRouter API key".to_string(),
                    },
                },
            )?,
            // Api-key auth: no interactive flow, just ack.
            Request::Authenticate { id } => emit(&mut stdout, &Outbound::Ack { id })?,
            Request::GetModels { id } => emit(
                &mut stdout,
                &Outbound::Models {
                    id,
                    management: ModelManagement::UserDefined,
                    models: parse_models(&settings.models),
                },
            )?,
            Request::ChatStart {
                id,
                model,
                messages,
            } => {
                if let Err(error) =
                    stream_chat(&client, &settings, &model, &messages, id, &mut stdout)
                {
                    emit(
                        &mut stdout,
                        &Outbound::Error {
                            id,
                            message: error.to_string(),
                        },
                    )?;
                }
            }
            Request::ChatCancel { id } => emit(&mut stdout, &Outbound::Done { id })?,
            // No server-side revoke for a user-supplied API key — the host just
            // forgets it. Ack so logout completes.
            Request::Logout { id } => emit(&mut stdout, &Outbound::Ack { id })?,
        }
    }

    Ok(())
}

/// User model list (one id per line, commas also accepted) -> models, first one
/// recommended.
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

fn stream_chat(
    client: &reqwest::blocking::Client,
    settings: &Settings,
    model: &str,
    messages: &[ChatMessage],
    id: u64,
    out: &mut impl Write,
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

    let response = client
        .post(&url)
        .bearer_auth(settings.api_key.trim())
        .json(&body)
        .send()?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().unwrap_or_default();
        anyhow::bail!(
            "OpenRouter HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        );
    }

    let reader = BufReader::new(response);
    for line in reader.lines() {
        let line = line?;
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
            if let Some(delta) = value["choices"][0]["delta"]["content"].as_str() {
                if !delta.is_empty() {
                    emit(
                        out,
                        &Outbound::Delta {
                            id,
                            text: delta.to_string(),
                        },
                    )?;
                }
            }
        }
    }

    emit(out, &Outbound::Done { id })?;
    Ok(())
}

fn emit(out: &mut impl Write, message: &Outbound) -> anyhow::Result<()> {
    let line = serde_json::to_string(message)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
