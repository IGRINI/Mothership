use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _, Result};
use mothership_adapter_sdk::protocol::AuthStatus;
use mothership_adapter_sdk::{http, oauth, Context};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use url::Url;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_ADDR: &str = "127.0.0.1:1455";
const SCOPE: &str = "openid profile email offline_access";
const REVOKE_ENDPOINT: &str = "https://auth.openai.com/oauth/revoke";
const REFRESH_MARGIN_MS: u64 = 60_000;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const OAUTH_ACCEPT_TIMEOUT: Duration = Duration::from_secs(300);
const OAUTH_READ_TIMEOUT: Duration = Duration::from_secs(30);
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CODEX_USER_AGENT: &str = concat!("codex_cli_rs/", env!("CARGO_PKG_VERSION"), " (Mothership)");

pub(crate) const CREDENTIAL_SETTINGS_KEY: &str = "credential";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CodexCredential {
    #[serde(rename = "type", default)]
    credential_type: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    pub(crate) expires_at: Option<String>,
    #[serde(default)]
    pub(crate) account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

pub(crate) fn credential_auth_status(credential: Option<&CodexCredential>) -> AuthStatus {
    match credential {
        None => AuthStatus::missing("Codex OAuth is not authorized"),
        Some(credential) => {
            let account_label = credential.account_id.clone();
            let expires_at = credential.expires_at.clone();
            let expired_without_refresh = expires_at
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
                .map(|expires_at| {
                    expires_at <= now_millis() && credential.refresh_token.trim().is_empty()
                })
                .unwrap_or(false);
            if expired_without_refresh {
                AuthStatus::expired(account_label, expires_at)
            } else {
                AuthStatus::authenticated(account_label, expires_at)
            }
        }
    }
}

pub(crate) async fn refresh_if_needed(
    client: &reqwest::Client,
    ctx: &Context,
    credential: &mut Option<CodexCredential>,
) -> Result<bool> {
    let refresh_token = match credential {
        Some(credential)
            if is_near_expiry(credential) && !credential.refresh_token.trim().is_empty() =>
        {
            credential.refresh_token.clone()
        }
        _ => return Ok(false),
    };

    let tokens = post_token(
        client,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh_token),
            ("client_id", CLIENT_ID),
        ],
    )
    .await?;

    if let Some(credential) = credential.as_mut() {
        credential.access_token = tokens.access_token;
        if !tokens.refresh_token.is_empty() {
            credential.refresh_token = tokens.refresh_token;
        }
        if let Some(id_token) = tokens.id_token {
            credential.id_token = Some(id_token);
        }
        credential.expires_at = tokens.expires_in.map(|seconds| {
            now_millis()
                .saturating_add(seconds.saturating_mul(1000))
                .to_string()
        });
    }

    if let Some(credential) = credential.clone() {
        persist_credential(ctx, &credential);
    }
    Ok(true)
}

fn is_near_expiry(credential: &CodexCredential) -> bool {
    credential
        .expires_at
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|expires_at| expires_at <= now_millis().saturating_add(REFRESH_MARGIN_MS))
        .unwrap_or(false)
}

pub(crate) fn auth_headers(access_token: &str, account_id: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![(
        "Authorization".to_string(),
        format!("Bearer {access_token}"),
    )];
    headers.push(("originator".to_string(), CODEX_ORIGINATOR.to_string()));
    headers.push(("User-Agent".to_string(), CODEX_USER_AGENT.to_string()));
    if let Some(account_id) = account_id.map(str::trim).filter(|value| !value.is_empty()) {
        headers.push(("ChatGPT-Account-Id".to_string(), account_id.to_string()));
    }
    headers
}

pub(crate) fn ws_headers(access_token: &str, account_id: Option<&str>) -> Vec<(String, String)> {
    let mut headers = auth_headers(access_token, account_id);
    headers.push((
        "OpenAI-Beta".to_string(),
        super::chat::WS_BETA_HEADER.to_string(),
    ));
    headers
}

pub(crate) fn persist_credential(ctx: &Context, credential: &CodexCredential) {
    if let Ok(json) = serde_json::to_string(credential) {
        let mut values = BTreeMap::new();
        values.insert(CREDENTIAL_SETTINGS_KEY.to_string(), json);
        ctx.store_secret(values);
    }
}

pub(crate) async fn run_oauth(client: &reqwest::Client) -> Result<CodexCredential> {
    let pkce = oauth::pkce_pair(32);
    let state = oauth::random_state(16);

    // Bind the callback listener before opening the browser to avoid a race.
    let listener = TcpListener::bind(CALLBACK_ADDR)
        .await
        .context("bind OAuth callback listener")?;
    let auth_url = authorize_url(&pkce.challenge, &state);
    let _ = open::that(&auth_url);
    eprintln!(
        "codex-adapter: waiting for browser OAuth on {REDIRECT_URI}\nif the browser didn't open, paste this URL:\n{auth_url}"
    );

    let callback = oauth::wait_for_localhost_callback(
        &listener,
        OAUTH_ACCEPT_TIMEOUT,
        OAUTH_READ_TIMEOUT,
        OAUTH_SUCCESS_BODY,
    )
    .await?;
    if callback.state != state {
        bail!("OAuth state mismatch");
    }

    let tokens = post_token(
        client,
        &[
            ("grant_type", "authorization_code"),
            ("code", &callback.code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", &pkce.verifier),
        ],
    )
    .await?;

    let account_id = tokens
        .id_token
        .as_deref()
        .or(Some(tokens.access_token.as_str()))
        .and_then(oauth::parse_jwt_claims)
        .and_then(|claims| account_id_from_claims(&claims));

    Ok(CodexCredential {
        credential_type: "openai_codex_oauth".to_string(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        id_token: tokens.id_token,
        expires_at: tokens.expires_in.map(|seconds| {
            now_millis()
                .saturating_add(seconds.saturating_mul(1000))
                .to_string()
        }),
        account_id,
    })
}

const OAUTH_SUCCESS_BODY: &str = "<!doctype html><meta charset=utf-8><title>Mothership</title>\
        <body style=\"font-family:system-ui;background:#0b0f14;color:#eaf1f8\">\
        <p>Codex authorized. You can close this tab and return to Mothership.</p>";

async fn post_token(client: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let response = client
        .post(format!("{ISSUER}/oauth/token"))
        .timeout(HTTP_TIMEOUT)
        .form(form)
        .send()
        .await?;
    let redacted_values = form
        .iter()
        .filter_map(|(key, value)| {
            let key = key.to_ascii_lowercase();
            if key.contains("token") || key.contains("code") || key.contains("verifier") {
                Some(*value)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let response = http::ensure_success_redacted(
        response,
        http::DEFAULT_ERROR_BODY_TIMEOUT,
        http::DEFAULT_MAX_ERROR_BODY_CHARS,
        &[],
        &redacted_values,
    )
    .await?;
    response.json().await.context("decode token response")
}

fn authorize_url(code_challenge: &str, state: &str) -> String {
    let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize")).expect("valid issuer url");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "mothership");
    url.to_string()
}

#[derive(Debug, Serialize)]
struct RevokeTokenRequest<'a> {
    token: &'a str,
    token_type_hint: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<&'static str>,
}

/// Best-effort OAuth token revoke (prefer refresh token); never blocks logout.
pub(crate) async fn revoke_credential(client: &reqwest::Client, credential: &CodexCredential) {
    let refresh = credential.refresh_token.trim();
    let access = credential.access_token.trim();
    let (token, token_type_hint, client_id) = if !refresh.is_empty() {
        (refresh, "refresh_token", Some(CLIENT_ID))
    } else if !access.is_empty() {
        (access, "access_token", None)
    } else {
        return;
    };
    let request = RevokeTokenRequest {
        token,
        token_type_hint,
        client_id,
    };
    match client
        .post(REVOKE_ENDPOINT)
        .timeout(Duration::from_secs(10))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await
    {
        Ok(response) => {
            if let Err(error) = http::ensure_success_redacted(
                response,
                http::DEFAULT_ERROR_BODY_TIMEOUT,
                http::DEFAULT_MAX_ERROR_BODY_CHARS,
                &[],
                &[token],
            )
            .await
            {
                eprintln!("codex-adapter: token revoke rejected: {error:#}");
            }
        }
        Err(error) => eprintln!("codex-adapter: token revoke failed: {error}"),
    }
}

fn account_id_from_claims(claims: &serde_json::Value) -> Option<String> {
    claims
        .get("chatgpt_account_id")
        .and_then(|value| value.as_str())
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|value| value.get("chatgpt_account_id"))
                .and_then(|value| value.as_str())
        })
        .map(ToOwned::to_owned)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_headers_include_codex_client_identity() {
        let headers = auth_headers("access-token", Some(" account-123 "));

        assert_eq!(
            header_value(&headers, "Authorization"),
            Some("Bearer access-token")
        );
        assert_eq!(header_value(&headers, "originator"), Some(CODEX_ORIGINATOR));
        assert!(header_value(&headers, "User-Agent")
            .expect("user agent header")
            .starts_with("codex_cli_rs/"));
        assert_eq!(
            header_value(&headers, "ChatGPT-Account-Id"),
            Some("account-123")
        );
    }

    #[test]
    fn auth_headers_omit_empty_account_id() {
        let headers = auth_headers("access-token", Some("   "));

        assert_eq!(header_value(&headers, "ChatGPT-Account-Id"), None);
    }

    #[test]
    fn ws_headers_keep_codex_client_identity_and_add_beta() {
        let headers = ws_headers("access-token", Some("account-123"));

        assert_eq!(header_value(&headers, "originator"), Some(CODEX_ORIGINATOR));
        assert_eq!(
            header_value(&headers, "OpenAI-Beta"),
            Some(super::super::chat::WS_BETA_HEADER)
        );
    }

    fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}
