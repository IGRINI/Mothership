use std::collections::BTreeMap;

use crate::{MothershipError, Result};

use super::{
    refresh_codex_token_with_client, CodexCredentialPayload, CredentialKind, CredentialVault,
    OpenAiCodexOAuthAdapter, ProviderConnection, ProviderId, SecretMaterial, SecretPayload,
    StoreCredentialRequest,
};

const REFRESH_SAFETY_MARGIN_MS: u64 = 60_000;

#[derive(Debug, Clone)]
pub struct PreparedProviderRequest {
    pub url: String,
    pub headers: BTreeMap<String, String>,
}

pub struct OpenAiCodexGateway<'a> {
    vault: &'a dyn CredentialVault,
}

impl<'a> OpenAiCodexGateway<'a> {
    pub fn new(vault: &'a dyn CredentialVault) -> Self {
        Self { vault }
    }

    pub fn prepare_request(
        &self,
        connection: &ProviderConnection,
        original_url: &str,
    ) -> Result<PreparedProviderRequest> {
        if connection.provider_id.as_str() != OpenAiCodexOAuthAdapter::PROVIDER_ID {
            return Err(MothershipError::InvalidRequest(format!(
                "connection is not OpenAI: {}",
                connection.id
            )));
        }

        let mut credential = load_codex_credential(self.vault, connection)?;
        if should_refresh(&credential) {
            credential = refresh_credential(self.vault, connection, &credential)?;
        }

        let mut headers = BTreeMap::new();
        headers.insert(
            "authorization".to_string(),
            format!("Bearer {}", credential.access_token),
        );
        if let Some(account_id) = credential.account_id {
            headers.insert("ChatGPT-Account-Id".to_string(), account_id);
        }

        Ok(PreparedProviderRequest {
            url: rewrite_codex_url(original_url),
            headers,
        })
    }
}

fn load_codex_credential(
    vault: &dyn CredentialVault,
    connection: &ProviderConnection,
) -> Result<CodexCredentialPayload> {
    let secret = vault.load(&connection.credential_ref.vault_handle)?;
    if secret.credential_kind != CredentialKind::OAuthTokenSet {
        return Err(MothershipError::CredentialVault(format!(
            "connection credential is not OAuth token set: {}",
            connection.id
        )));
    }
    serde_json::from_str(secret.payload.expose_for_vault()).map_err(|error| {
        MothershipError::CredentialVault(format!(
            "invalid Codex credential payload for {}: {error}",
            connection.id
        ))
    })
}

fn refresh_credential(
    vault: &dyn CredentialVault,
    connection: &ProviderConnection,
    credential: &CodexCredentialPayload,
) -> Result<CodexCredentialPayload> {
    let tokens = refresh_codex_token_with_client(
        &reqwest::blocking::Client::new(),
        &credential.refresh_token,
    )?;
    let expires_at = tokens
        .expires_in
        .map(|expires_in| (unix_timestamp_millis() + expires_in.saturating_mul(1000)).to_string());
    let refreshed = CodexCredentialPayload {
        credential_type: "openai_codex_oauth".to_string(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        id_token: tokens.id_token,
        expires_at: expires_at.clone(),
        account_id: credential.account_id.clone(),
    };
    vault.replace(
        &connection.credential_ref.vault_handle,
        StoreCredentialRequest {
            provider_id: ProviderId::from(OpenAiCodexOAuthAdapter::PROVIDER_ID),
            credential: SecretMaterial {
                credential_kind: CredentialKind::OAuthTokenSet,
                payload: SecretPayload::new(serde_json::to_string(&refreshed)?),
                expires_at,
                fingerprint_hash: refreshed.account_id.clone(),
            },
        },
    )?;

    Ok(refreshed)
}

fn should_refresh(credential: &CodexCredentialPayload) -> bool {
    let Some(expires_at) = credential
        .expires_at
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return false;
    };

    expires_at <= unix_timestamp_millis().saturating_add(REFRESH_SAFETY_MARGIN_MS)
}

fn rewrite_codex_url(original_url: &str) -> String {
    if original_url.contains("/v1/responses") || original_url.contains("/chat/completions") {
        OpenAiCodexOAuthAdapter::codex_api_endpoint().to_string()
    } else {
        original_url.to_string()
    }
}

fn unix_timestamp_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::auth::{CredentialRef, ProviderConnectionId, VaultHandle};

    #[test]
    fn rewrites_responses_url_to_codex_backend() {
        assert_eq!(
            rewrite_codex_url("https://api.openai.com/v1/responses"),
            OpenAiCodexOAuthAdapter::codex_api_endpoint()
        );
    }

    #[test]
    fn prepares_codex_headers_without_serializing_tokens_elsewhere() {
        let vault = crate::auth::InMemoryCredentialVault::default();
        let handle = vault
            .store(StoreCredentialRequest {
                provider_id: ProviderId::from(OpenAiCodexOAuthAdapter::PROVIDER_ID),
                credential: SecretMaterial {
                    credential_kind: CredentialKind::OAuthTokenSet,
                    payload: SecretPayload::new(
                        json!({
                            "type": "openai_codex_oauth",
                            "accessToken": "access",
                            "refreshToken": "refresh",
                            "idToken": null,
                            "expiresAt": null,
                            "accountId": "acc"
                        })
                        .to_string(),
                    ),
                    expires_at: None,
                    fingerprint_hash: Some("acc".to_string()),
                },
            })
            .expect("store");
        let connection = ProviderConnection {
            id: ProviderConnectionId::from("connection"),
            provider_id: ProviderId::from(OpenAiCodexOAuthAdapter::PROVIDER_ID),
            auth_method_id: OpenAiCodexOAuthAdapter::AUTH_METHOD_ID.into(),
            status: crate::auth::ConnectionStatus::Active,
            account_label: None,
            account_email: None,
            scopes: vec![],
            capabilities: vec![],
            credential_ref: CredentialRef {
                record_id: "record".into(),
                vault_handle: VaultHandle::from(handle.as_str().to_string()),
            },
            expires_at: None,
            created_at: "0".to_string(),
            updated_at: "0".to_string(),
        };

        let prepared = OpenAiCodexGateway::new(&vault)
            .prepare_request(&connection, "https://api.openai.com/v1/responses")
            .expect("prepare");
        assert_eq!(
            prepared.headers.get("authorization"),
            Some(&"Bearer access".to_string())
        );
        assert_eq!(
            prepared.headers.get("ChatGPT-Account-Id"),
            Some(&"acc".to_string())
        );
    }
}
