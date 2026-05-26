use std::{collections::HashMap, time::Duration};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::{MothershipError, Result};

use super::{
    oauth::{decode_base64_url_json, generate_pkce_pair, generate_state},
    AdapterAuthCompletion, AdapterRevokeAuthResult, AdapterStartAuthInput, AdapterStartAuthResult,
    AuthMethod, AuthMethodId, AuthMethodKind, AuthMode, AuthNextAction, AuthSession,
    AuthSessionStatus, CompleteAuthAdapterInput, CredentialKind, ProviderAuthAdapter, ProviderId,
    RevokeAuthAdapterInput, SecretMaterial, SecretPayload,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const DEFAULT_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CODEX_API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const REVOKE_TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/revoke";
const SCOPE: &str = "openid profile email offline_access";
const REVOKE_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

pub struct OpenAiCodexOAuthAdapter {
    client: Client,
    redirect_uri: String,
}

impl Default for OpenAiCodexOAuthAdapter {
    fn default() -> Self {
        Self {
            client: Client::new(),
            redirect_uri: DEFAULT_REDIRECT_URI.to_string(),
        }
    }
}

impl OpenAiCodexOAuthAdapter {
    pub const PROVIDER_ID: &'static str = "openai";
    pub const AUTH_METHOD_ID: &'static str = "codex_oauth_browser";

    pub fn codex_api_endpoint() -> &'static str {
        CODEX_API_ENDPOINT
    }

    pub fn default_redirect_uri() -> &'static str {
        DEFAULT_REDIRECT_URI
    }
}

impl ProviderAuthAdapter for OpenAiCodexOAuthAdapter {
    fn provider_id(&self) -> ProviderId {
        ProviderId::from(Self::PROVIDER_ID)
    }

    fn provider_label(&self) -> String {
        "OpenAI".to_string()
    }

    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![AuthMethod {
            id: AuthMethodId::from(Self::AUTH_METHOD_ID),
            provider_id: self.provider_id(),
            kind: AuthMethodKind::OAuthBrowser,
            label: "ChatGPT Plus/Pro via Codex OAuth".to_string(),
            description: "Browser OAuth flow compatible with Codex subscription access."
                .to_string(),
        }]
    }

    fn start_auth(&self, input: AdapterStartAuthInput) -> Result<AdapterStartAuthResult> {
        if input.auth_method_id.as_str() != Self::AUTH_METHOD_ID {
            return Err(MothershipError::InvalidRequest(format!(
                "unsupported OpenAI auth method: {}",
                input.auth_method_id
            )));
        }

        let pkce = generate_pkce_pair()?;
        let state = generate_state()?;
        let authorization_url = build_authorize_url(&self.redirect_uri, &pkce.challenge, &state)?;

        Ok(AdapterStartAuthResult {
            mode: AuthMode::Browser,
            next_action: AuthNextAction {
                authorization_url: Some(authorization_url),
                user_code: None,
                verification_uri: None,
                message: Some("Complete OpenAI Codex authorization in the browser.".to_string()),
            },
            expires_at: None,
            provider_metadata: json!({
                "pkceVerifier": pkce.verifier,
                "state": state,
                "redirectUri": self.redirect_uri,
            }),
        })
    }

    fn complete_auth(&self, input: CompleteAuthAdapterInput) -> Result<AdapterAuthCompletion> {
        if input.session.status != AuthSessionStatus::Pending {
            return Err(MothershipError::InvalidRequest(format!(
                "OpenAI auth session is not pending: {}",
                input.session.id
            )));
        }

        let pending = PendingCodexOAuth::from_session(&input.session)?;
        let callback = CodexCallbackPayload::from_payload(input.payload)?;

        if callback.state.as_deref() != Some(pending.state.as_str()) {
            return Err(MothershipError::InvalidRequest(
                "invalid OpenAI OAuth state".to_string(),
            ));
        }

        let token_response = exchange_code_for_tokens(
            &self.client,
            &callback.code,
            &pending.redirect_uri,
            &pending.pkce_verifier,
        )?;
        let claims = extract_claims(&token_response);
        let account_id = extract_account_id_from_claims(claims.as_ref());
        let email = claims
            .as_ref()
            .and_then(|claims| claims.get("email"))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);
        let expires_at = token_response.expires_in.map(|expires_in| {
            (unix_timestamp_millis() + expires_in.saturating_mul(1000)).to_string()
        });

        let secret_payload = CodexCredentialPayload {
            credential_type: "openai_codex_oauth".to_string(),
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token,
            id_token: token_response.id_token,
            expires_at: expires_at.clone(),
            account_id: account_id.clone(),
        };

        Ok(AdapterAuthCompletion {
            account_label: email.clone().or(account_id.clone()),
            account_email: email,
            scopes: vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string(),
                "offline_access".to_string(),
            ],
            capabilities: vec!["codex_responses".to_string()],
            secret: SecretMaterial {
                credential_kind: CredentialKind::OAuthTokenSet,
                payload: SecretPayload::new(serde_json::to_string(&secret_payload)?),
                expires_at,
                fingerprint_hash: account_id,
            },
        })
    }

    fn revoke_auth(&self, input: RevokeAuthAdapterInput) -> Result<AdapterRevokeAuthResult> {
        if input.connection.provider_id.as_str() != Self::PROVIDER_ID {
            return Ok(AdapterRevokeAuthResult::unsupported());
        }

        let credential: CodexCredentialPayload =
            serde_json::from_str(input.secret.payload.expose_for_vault())?;
        revoke_codex_credential(&self.client, &credential)
    }
}

#[derive(Debug, Deserialize)]
struct PendingCodexOAuth {
    #[serde(rename = "pkceVerifier")]
    pkce_verifier: String,
    state: String,
    #[serde(rename = "redirectUri")]
    redirect_uri: String,
}

impl PendingCodexOAuth {
    fn from_session(session: &AuthSession) -> Result<Self> {
        serde_json::from_value(session.provider_metadata.clone()).map_err(Into::into)
    }
}

#[derive(Debug)]
struct CodexCallbackPayload {
    code: String,
    state: Option<String>,
}

impl CodexCallbackPayload {
    fn from_payload(payload: serde_json::Value) -> Result<Self> {
        if let Some(callback_url) = payload.get("callbackUrl").and_then(|value| value.as_str()) {
            let url = Url::parse(callback_url).map_err(|error| {
                MothershipError::InvalidRequest(format!("invalid callback URL: {error}"))
            })?;
            let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
            let code = query.get("code").cloned().ok_or_else(|| {
                MothershipError::InvalidRequest("callback URL missing code".to_string())
            })?;
            return Ok(Self {
                code,
                state: query.get("state").cloned(),
            });
        }

        let code = payload
            .get("code")
            .and_then(|value| value.as_str())
            .ok_or_else(|| MothershipError::InvalidRequest("missing OAuth code".to_string()))?
            .to_string();
        let state = payload
            .get("state")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);

        Ok(Self { code, state })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CodexTokenResponse {
    pub id_token: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCredentialPayload {
    #[serde(rename = "type")]
    pub credential_type: String,
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: Option<String>,
    pub expires_at: Option<String>,
    pub account_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct RevokeTokenRequest<'a> {
    token: &'a str,
    token_type_hint: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<&'static str>,
}

pub fn build_authorize_url(
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> Result<String> {
    let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize")).map_err(|error| {
        MothershipError::InvalidRequest(format!("invalid Codex OAuth issuer URL: {error}"))
    })?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "mothership");
    Ok(url.to_string())
}

pub fn refresh_codex_token(refresh_token: &str) -> Result<CodexTokenResponse> {
    let client = Client::new();
    refresh_codex_token_with_client(&client, refresh_token)
}

pub fn refresh_codex_token_with_client(
    client: &Client,
    refresh_token: &str,
) -> Result<CodexTokenResponse> {
    let response = client
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .map_err(|error| {
            MothershipError::InvalidRequest(format!("Codex token refresh failed: {error}"))
        })?
        .error_for_status()
        .map_err(|error| {
            MothershipError::InvalidRequest(format!("Codex token refresh rejected: {error}"))
        })?;

    response.json().map_err(|error| {
        MothershipError::InvalidRequest(format!("invalid Codex token refresh response: {error}"))
    })
}

pub fn revoke_codex_credential(
    client: &Client,
    credential: &CodexCredentialPayload,
) -> Result<AdapterRevokeAuthResult> {
    let refresh_token = credential.refresh_token.trim();
    let access_token = credential.access_token.trim();

    let (token, token_type_hint, client_id) = if !refresh_token.is_empty() {
        (refresh_token, "refresh_token", Some(CLIENT_ID))
    } else if !access_token.is_empty() {
        (access_token, "access_token", None)
    } else {
        return Ok(AdapterRevokeAuthResult::nothing_to_revoke());
    };

    let request = RevokeTokenRequest {
        token,
        token_type_hint,
        client_id,
    };

    let response = client
        .post(REVOKE_TOKEN_ENDPOINT)
        .timeout(REVOKE_HTTP_TIMEOUT)
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .map_err(|error| {
            MothershipError::InvalidRequest(format!("Codex token revoke failed: {error}"))
        })?;

    if response.status().is_success() {
        return Ok(AdapterRevokeAuthResult::attempted());
    }

    let status = response.status();
    let body = response.text().unwrap_or_default();
    Err(MothershipError::InvalidRequest(format!(
        "Codex token revoke rejected: {status}: {}",
        sanitize_provider_error(&body)
    )))
}

fn exchange_code_for_tokens(
    client: &Client,
    code: &str,
    redirect_uri: &str,
    pkce_verifier: &str,
) -> Result<CodexTokenResponse> {
    let response = client
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", pkce_verifier),
        ])
        .send()
        .map_err(|error| {
            MothershipError::InvalidRequest(format!("Codex token exchange failed: {error}"))
        })?
        .error_for_status()
        .map_err(|error| {
            MothershipError::InvalidRequest(format!("Codex token exchange rejected: {error}"))
        })?;

    response.json().map_err(|error| {
        MothershipError::InvalidRequest(format!("invalid Codex token response: {error}"))
    })
}

fn sanitize_provider_error(body: &str) -> String {
    if body.trim().is_empty() {
        return "empty response body".to_string();
    }

    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| match error {
                    serde_json::Value::Object(error) => error
                        .get("message")
                        .or_else(|| error.get("code"))
                        .and_then(|value| value.as_str())
                        .map(ToOwned::to_owned),
                    serde_json::Value::String(error) => Some(error.to_string()),
                    _ => None,
                })
                .or_else(|| {
                    value
                        .get("message")
                        .and_then(|value| value.as_str())
                        .map(ToOwned::to_owned)
                })
        })
        .unwrap_or_else(|| body.chars().take(300).collect())
}

fn extract_claims(tokens: &CodexTokenResponse) -> Option<serde_json::Value> {
    tokens
        .id_token
        .as_ref()
        .and_then(|token| parse_jwt_claims(token))
        .or_else(|| parse_jwt_claims(&tokens.access_token))
}

pub fn parse_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    decode_base64_url_json(payload)
}

pub fn extract_account_id_from_claims(claims: Option<&serde_json::Value>) -> Option<String> {
    let claims = claims?;
    claims
        .get("chatgpt_account_id")
        .and_then(|value| value.as_str())
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|value| value.get("chatgpt_account_id"))
                .and_then(|value| value.as_str())
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(|value| value.as_array())
                .and_then(|organizations| organizations.first())
                .and_then(|organization| organization.get("id"))
                .and_then(|value| value.as_str())
        })
        .map(ToOwned::to_owned)
}

fn unix_timestamp_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde_json::json;

    use super::*;

    #[test]
    fn authorize_url_contains_codex_parameters() {
        let url = build_authorize_url("http://localhost:1455/auth/callback", "challenge", "state")
            .expect("url");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
    }

    #[test]
    fn extracts_account_id_from_jwt_claims() {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acc-nested"
                }
            }))
            .expect("payload"),
        );
        let claims = parse_jwt_claims(&format!("header.{payload}.sig"));
        assert_eq!(
            extract_account_id_from_claims(claims.as_ref()),
            Some("acc-nested".to_string())
        );
    }
}
