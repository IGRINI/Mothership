//! Codex provider adapter.
//!
//! Makes the built-in Codex provider a first-class subprocess adapter — the same
//! shape as any other plugin. It reuses the battle-tested Codex internals from
//! `mothership-core` (model catalog fetch + token refresh + the WS/SSE/JSON chat
//! transport) rather than reimplementing them; it just wraps them in the stdio
//! adapter protocol.
//!
//! Auth is a pasted Codex credential JSON (access/refresh tokens + account id),
//! supplied via the adapter's settings (the "oauth_token_paste" pattern). Core's
//! refresh logic keeps it alive. A browser OAuth flow inside the adapter is a
//! later step.

use std::io::{BufRead, Write};

use mothership_adapter_host::protocol::{
    AuthKind, ChatMessage, Model, ModelManagement, Outbound, Request, SettingsField,
    SettingsFieldKind,
};
use mothership_core::auth::{
    AuthMethodId, ConnectionStatus, CredentialKind, CredentialRecordId, CredentialRef,
    CredentialVault, InMemoryCredentialVault, ProviderConnection, ProviderConnectionId, ProviderId,
    SecretMaterial, SecretPayload, StoreCredentialRequest, VaultHandle,
};
use mothership_core::llm::{LlmConnectorAdapter, OpenAiCodexLlmConnector, RemoteModelCatalogInput};
use mothership_core::{
    chat_system_prompt, LlmChatCompletionEventSink, LlmChatCompletionGateway,
    LlmChatCompletionRequest, LlmChatMessage, LlmChatRole, LlmTransportKind,
    OpenAiCodexChatCompletionGateway,
};

// Codex's gateway/catalog validate the connection's provider id against this.
const CODEX_PROVIDER_ID: &str = "openai";
const CREDENTIAL_KEY: &str = "credential";

fn main() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    let mut credential = String::new();

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
            Request::Initialize { id } => emit(&mut stdout, &Outbound::Ack { id })?,
            Request::GetIdentity { id } => emit(
                &mut stdout,
                &Outbound::Identity {
                    id,
                    provider_id: "codex".to_string(),
                    provider_label: "Codex".to_string(),
                },
            )?,
            Request::GetSettingsSchema { id } => emit(
                &mut stdout,
                &Outbound::SettingsSchema {
                    id,
                    fields: vec![SettingsField {
                        key: CREDENTIAL_KEY.to_string(),
                        label: "Codex credential JSON (paste)".to_string(),
                        kind: SettingsFieldKind::Secret,
                        required: true,
                    }],
                },
            )?,
            Request::SetSettings { id, values } => {
                if let Some(value) = values.get(CREDENTIAL_KEY) {
                    credential = value.clone();
                }
                emit(&mut stdout, &Outbound::Ack { id })?;
            }
            Request::GetAuthSchema { id } => {
                emit(&mut stdout, &Outbound::AuthSchema { id, auth: AuthKind::OauthInternal })?
            }
            Request::GetModels { id } => {
                let models = if credential.trim().is_empty() {
                    Vec::new()
                } else {
                    list_models(&credential).unwrap_or_default()
                };
                emit(
                    &mut stdout,
                    &Outbound::Models {
                        id,
                        management: ModelManagement::Server,
                        models,
                    },
                )?;
            }
            Request::ChatStart {
                id,
                model,
                messages,
            } => {
                handle_chat(&mut stdout, &credential, id, &model, messages)?;
            }
            Request::ChatCancel { id } => emit(&mut stdout, &Outbound::Done { id })?,
        }
    }

    Ok(())
}

/// Stores the pasted credential in an in-memory vault and builds the synthetic
/// connection Codex's reused gateway/catalog expect.
fn build_connection(
    vault: &InMemoryCredentialVault,
    credential_json: &str,
) -> anyhow::Result<ProviderConnection> {
    let handle = vault.store(StoreCredentialRequest {
        provider_id: ProviderId::from(CODEX_PROVIDER_ID),
        credential: SecretMaterial {
            credential_kind: CredentialKind::OAuthTokenSet,
            payload: SecretPayload::new(credential_json.to_string()),
            expires_at: None,
            fingerprint_hash: None,
        },
    })?;

    Ok(ProviderConnection {
        id: ProviderConnectionId::from("codex_adapter_connection"),
        provider_id: ProviderId::from(CODEX_PROVIDER_ID),
        auth_method_id: AuthMethodId::from("openai_oauth"),
        status: ConnectionStatus::Active,
        account_label: None,
        account_email: None,
        scopes: Vec::new(),
        capabilities: Vec::new(),
        credential_ref: CredentialRef {
            record_id: CredentialRecordId::from("codex_adapter_record"),
            vault_handle: VaultHandle::new(handle.as_str().to_string()),
        },
        expires_at: None,
        created_at: "0".to_string(),
        updated_at: "0".to_string(),
    })
}

fn list_models(credential_json: &str) -> anyhow::Result<Vec<Model>> {
    let vault = InMemoryCredentialVault::default();
    let connection = build_connection(&vault, credential_json)?;
    let secret = vault.load(&connection.credential_ref.vault_handle)?;

    let catalog = OpenAiCodexLlmConnector.remote_model_catalog(RemoteModelCatalogInput {
        connection: &connection,
        secret: &secret,
    })?;

    Ok(catalog
        .map(|catalog| catalog.models)
        .unwrap_or_default()
        .into_iter()
        .map(|model| Model {
            id: model.id,
            label: model.label,
            recommended: model.recommended,
        })
        .collect())
}

fn handle_chat(
    stdout: &mut impl Write,
    credential_json: &str,
    id: u64,
    model: &str,
    messages: Vec<ChatMessage>,
) -> anyhow::Result<()> {
    if credential_json.trim().is_empty() {
        return emit(
            stdout,
            &Outbound::Error {
                id,
                message: "missing Codex credential (paste it in adapter settings)".to_string(),
            },
        );
    }

    let outcome = (|| -> anyhow::Result<()> {
        let vault = InMemoryCredentialVault::default();
        let connection = build_connection(&vault, credential_json)?;
        let gateway = OpenAiCodexChatCompletionGateway::new(&vault, &connection)?;
        let system_prompt = chat_system_prompt(CODEX_PROVIDER_ID, model).unwrap_or_default();
        let request = LlmChatCompletionRequest {
            provider_id: CODEX_PROVIDER_ID.to_string(),
            model_id: model.to_string(),
            system_prompt,
            messages: messages
                .into_iter()
                .map(|message| LlmChatMessage {
                    role: if message.role == "assistant" {
                        LlmChatRole::Assistant
                    } else {
                        LlmChatRole::User
                    },
                    content: message.content,
                })
                .collect(),
        };

        let mut sink = ProtocolSink {
            out: stdout,
            id,
            error: None,
        };
        let result = gateway.complete_chat(request, &mut sink);
        let sink_error = sink.error.take();
        drop(sink);

        match result {
            Ok(_) => match sink_error {
                Some(error) => anyhow::bail!(error),
                None => Ok(()),
            },
            Err(error) => anyhow::bail!(error.to_string()),
        }
    })();

    match outcome {
        Ok(()) => emit(stdout, &Outbound::Done { id }),
        Err(error) => emit(
            stdout,
            &Outbound::Error {
                id,
                message: error.to_string(),
            },
        ),
    }
}

/// Forwards Codex stream deltas to the host as protocol events.
struct ProtocolSink<'a, W: Write> {
    out: &'a mut W,
    id: u64,
    error: Option<String>,
}

impl<W: Write> LlmChatCompletionEventSink for ProtocolSink<'_, W> {
    fn transport_selected(&mut self, _transport: LlmTransportKind) {}

    fn delta(&mut self, delta: &str) {
        if self.error.is_some() {
            return;
        }
        if let Err(error) = emit(
            self.out,
            &Outbound::Delta {
                id: self.id,
                text: delta.to_string(),
            },
        ) {
            self.error = Some(error.to_string());
        }
    }
}

fn emit(out: &mut impl Write, message: &Outbound) -> anyhow::Result<()> {
    let line = serde_json::to_string(message)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
