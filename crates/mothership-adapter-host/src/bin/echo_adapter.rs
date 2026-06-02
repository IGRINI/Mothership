//! Sample provider adapter: a normal executable speaking the stdio protocol.
//!
//! It echoes the last user message back as word-by-word streamed deltas. Real
//! adapters do the same dance but call an HTTP/WS/SSE provider or spawn a CLI
//! (e.g. claude-code) in place of the echo.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use mothership_adapter_host::protocol::{
    AuthKind, AuthStatus, Model, ModelManagement, Outbound, Request, SettingsField,
    SettingsFieldKind, PROTOCOL_VERSION,
};

fn main() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    let mut api_key_configured = false;

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // host closed stdin
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
                    provider_id: "echo".to_string(),
                    provider_label: "Echo Provider".to_string(),
                },
            )?,
            Request::GetModels { id } => emit(
                &mut stdout,
                &Outbound::Models {
                    id,
                    management: ModelManagement::Fixed,
                    models: vec![Model {
                        id: "echo-1".to_string(),
                        label: "Echo 1".to_string(),
                        recommended: true,
                    }],
                },
            )?,
            Request::GetSettingsSchema { id } => emit(
                &mut stdout,
                &Outbound::SettingsSchema {
                    id,
                    fields: vec![
                        SettingsField {
                            key: "endpoint".to_string(),
                            label: "Endpoint".to_string(),
                            kind: SettingsFieldKind::Text,
                            required: false,
                        },
                        SettingsField {
                            key: "api_key".to_string(),
                            label: "API key".to_string(),
                            kind: SettingsFieldKind::Secret,
                            required: false,
                        },
                    ],
                },
            )?,
            Request::SetSettings { id, values } => {
                api_key_configured = values
                    .get("api_key")
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false);
                // Test hook for the reverse secret channel: if asked, push a
                // secret back to the host *before* acking. The host consumes the
                // `StoreSecret` transparently inside `recv`, so the ack still
                // lands as the reply to this request.
                if let Some(secret) = values.get("echo_store_secret") {
                    emit(
                        &mut stdout,
                        &Outbound::StoreSecret {
                            values: BTreeMap::from([("persisted".to_string(), secret.clone())]),
                        },
                    )?;
                }
                emit(&mut stdout, &Outbound::Ack { id })?;
            }
            Request::GetAuthSchema { id } => emit(
                &mut stdout,
                &Outbound::AuthSchema {
                    id,
                    auth: AuthKind::ApiKey {
                        label: "API key".to_string(),
                    },
                },
            )?,
            Request::GetAuthStatus { id } => emit(
                &mut stdout,
                &Outbound::AuthStatus {
                    id,
                    status: if api_key_configured {
                        AuthStatus::configured("API key is configured")
                    } else {
                        AuthStatus::missing("API key is not configured")
                    },
                },
            )?,
            // Api-key auth has no interactive flow; just ack.
            Request::Authenticate { id } => emit(&mut stdout, &Outbound::Ack { id })?,
            Request::ChatStart { id, messages, .. } => {
                let last = messages
                    .last()
                    .map(|message| message.content.clone())
                    .unwrap_or_default();
                for word in last.split_whitespace() {
                    emit(
                        &mut stdout,
                        &Outbound::Delta {
                            id,
                            text: format!("{word} "),
                        },
                    )?;
                }
                emit(&mut stdout, &Outbound::Done { id })?;
            }
            Request::ChatCancel { id } => emit(&mut stdout, &Outbound::Done { id })?,
            Request::ToolResult { .. } => {}
            Request::Logout { id } => emit(&mut stdout, &Outbound::Ack { id })?,
        }
    }

    Ok(())
}

fn emit(out: &mut impl Write, message: &Outbound) -> anyhow::Result<()> {
    let line = serde_json::to_string(message)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
