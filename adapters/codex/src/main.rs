//! Codex provider adapter — built on the shared adapter SDK.
//!
//! Provider-specific logic only: Codex OAuth endpoints/request shapes, the
//! model catalog, and wiring the
//! OpenAI Responses transport. Everything generic — the stdio protocol loop, the
//! HTTP/SSE/WebSocket transports, OAuth mechanics, the WS-primary→SSE→JSON
//! fallback, idle timeouts, structured Responses parsing — comes from
//! `mothership-adapter-sdk` and `mothership-openai-responses`, shared with any
//! other adapter.
//!
//! Credentials are pushed in by the host under the `credential` settings key and
//! pushed back via `StoreSecret`; nothing is stored next to the adapter.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use url::Url;

use mothership_adapter_sdk::oauth;
use mothership_adapter_sdk::protocol::{
    AuthKind, AuthStatus, Model, ModelManagement, PromptBundle, ToolDescriptor,
};
use mothership_adapter_sdk::tools::{dispatch_tool_calls, ProviderToolCall};
use mothership_adapter_sdk::ws::WsSession;
use mothership_adapter_sdk::{http, ChatRequest, Context, ProviderAdapter};
use mothership_openai_responses as responses;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_ADDR: &str = "127.0.0.1:1455";
const SCOPE: &str = "openid profile email offline_access";
const MODELS_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/models";
const RESPONSES_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const REVOKE_ENDPOINT: &str = "https://auth.openai.com/oauth/revoke";
const CLIENT_VERSION: &str = "0.133.0";
/// Codex Responses-over-WebSocket beta opt-in (matches the real Codex CLI).
const WS_BETA_HEADER: &str = "responses_websockets=2026-02-06";
const REFRESH_MARGIN_MS: u64 = 60_000;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ERROR_BODY_CHARS: usize = 300;
const OAUTH_ACCEPT_TIMEOUT: Duration = Duration::from_secs(300);
const OAUTH_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Settings key under which the host stores/loads the whole credential JSON.
const CREDENTIAL_SETTINGS_KEY: &str = "credential";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    mothership_adapter_sdk::run(CodexAdapter::new()?).await
}

struct CodexAdapter {
    client: reqwest::Client,
    endpoint: responses::Endpoint,
    credential: Option<CodexCredential>,
    /// Persistent backend WebSocket, lazily opened on first chat and reused
    /// across turns; dropped on idle / token change / WS failure.
    ws: Option<WsSession>,
    /// Set when a WS attempt fell back; skip WS until the credential changes.
    ws_disabled: bool,
}

impl CodexAdapter {
    fn new() -> Result<Self> {
        Ok(Self {
            client: mothership_adapter_sdk::http::client(Duration::from_secs(30)),
            endpoint: responses::Endpoint::from_https(RESPONSES_ENDPOINT)?,
            credential: None,
            ws: None,
            ws_disabled: false,
        })
    }

    /// Replace the credential and invalidate everything derived from it.
    fn set_credential(&mut self, credential: Option<CodexCredential>) {
        self.credential = credential;
        self.ws = None;
        self.ws_disabled = false;
    }

    /// A valid access token + account id, running browser OAuth if there is no
    /// credential and refreshing if near expiry. Persists anything minted.
    async fn ensure_token(&mut self, ctx: &Context) -> Result<(String, Option<String>)> {
        if self.credential.is_none() {
            let fresh = run_oauth(&self.client).await?;
            persist_credential(ctx, &fresh);
            self.set_credential(Some(fresh));
        }
        self.refresh_if_needed(ctx).await?;
        let credential = self
            .credential
            .as_ref()
            .expect("credential present after ensure");
        Ok((
            credential.access_token.clone(),
            credential.account_id.clone(),
        ))
    }

    async fn refresh_if_needed(&mut self, ctx: &Context) -> Result<()> {
        let refresh_token = match &self.credential {
            Some(credential)
                if is_near_expiry(credential) && !credential.refresh_token.trim().is_empty() =>
            {
                credential.refresh_token.clone()
            }
            _ => return Ok(()),
        };
        let tokens = post_token(
            &self.client,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("client_id", CLIENT_ID),
            ],
        )
        .await?;
        if let Some(credential) = self.credential.as_mut() {
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
        // Token changed: rebuild the WS with fresh headers and persist.
        self.ws = None;
        if let Some(credential) = self.credential.clone() {
            persist_credential(ctx, &credential);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for CodexAdapter {
    fn identity(&self) -> (String, String) {
        ("codex".to_string(), "Codex".to_string())
    }

    fn auth_schema(&self) -> AuthKind {
        AuthKind::OauthInternal
    }

    fn auth_status(&self) -> AuthStatus {
        match &self.credential {
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

    async fn set_settings(&mut self, values: BTreeMap<String, String>) -> Result<()> {
        self.set_credential(
            values
                .get(CREDENTIAL_SETTINGS_KEY)
                .and_then(|raw| serde_json::from_str(raw).ok()),
        );
        Ok(())
    }

    async fn models(&mut self, ctx: &Context) -> Result<(ModelManagement, Vec<Model>)> {
        // Never trigger OAuth from model listing — only use an existing credential.
        let models = if self.credential.is_some() {
            self.refresh_if_needed(ctx).await?;
            let (access_token, account_id) = {
                let credential = self.credential.as_ref().expect("credential present");
                (
                    credential.access_token.clone(),
                    credential.account_id.clone(),
                )
            };
            fetch_models(&self.client, &access_token, account_id.as_deref()).await?
        } else {
            Vec::new()
        };
        Ok((ModelManagement::Server, models))
    }

    async fn authenticate(&mut self, ctx: &Context) -> Result<()> {
        let fresh = run_oauth(&self.client).await?;
        persist_credential(ctx, &fresh);
        self.set_credential(Some(fresh));
        Ok(())
    }

    async fn chat(
        &mut self,
        request: ChatRequest,
        ctx: &Context,
        sink: &mut mothership_adapter_sdk::ChatSink,
    ) -> Result<()> {
        let (access_token, account_id) = self.ensure_token(ctx).await?;
        let headers = auth_headers(&access_token, account_id.as_deref());
        let instructions = provider_instructions(&request.prompt)?;

        let had_ws = !self.ws_disabled;
        if had_ws && self.ws.is_none() {
            self.ws = Some(WsSession::new(
                self.endpoint.wss_url.clone(),
                ws_headers(&access_token, account_id.as_deref()),
                responses::WS_SESSION_IDLE,
            ));
        }

        let cancellation = sink.cancellation_token();
        let mut on_delta = |text: &str| sink.delta(text);
        let tool_dispatcher = CodexToolDispatcher { sink: sink.clone() };
        let tools = request
            .tools
            .iter()
            .map(responses_tool_schema)
            .collect::<Vec<_>>();
        let ws_arg = if had_ws { self.ws.as_mut() } else { None };
        let transport = tokio::select! {
            result = responses::chat_with_tools(
                &self.client,
                &self.endpoint,
                &headers,
                &request.model,
                &instructions,
                &request.messages,
                ws_arg,
                &mut on_delta,
                &tools,
                &tool_dispatcher,
            ) => result?,
            _ = cancellation.cancelled() => {
                if let Some(session) = self.ws.as_mut() {
                    session.close().await;
                }
                return Ok(());
            }
        };

        // If WS was enabled but the answer came over a fallback, the WS tier is
        // unavailable on this backend/session — stop paying its connect cost.
        if had_ws && transport != responses::Transport::WebSocket {
            eprintln!("codex-adapter: websocket unavailable, served via {transport:?}; disabling WS for this session");
            self.ws = None;
            self.ws_disabled = true;
        }
        Ok(())
    }

    async fn logout(&mut self, _ctx: &Context) -> Result<()> {
        if let Some(credential) = self.credential.as_ref() {
            revoke_credential(&self.client, credential).await;
        }
        self.set_credential(None);
        Ok(())
    }

    async fn on_idle(&mut self, _ctx: &Context) {
        if let Some(session) = self.ws.as_mut() {
            session.close_if_idle().await;
        }
    }
}

struct CodexToolDispatcher {
    sink: mothership_adapter_sdk::ChatSink,
}

#[async_trait::async_trait]
impl responses::ToolDispatcher for CodexToolDispatcher {
    async fn dispatch(&self, call: responses::ToolCall) -> Result<responses::ToolOutput> {
        let result = self
            .sink
            .request_tool(call.call_id, call.name, call.arguments)
            .await?;
        Ok(responses::ToolOutput {
            output: result.content,
        })
    }

    async fn dispatch_many(
        &self,
        calls: Vec<responses::ToolCall>,
    ) -> Result<Vec<responses::ToolOutput>> {
        let calls = calls
            .into_iter()
            .map(|call| ProviderToolCall::new(call.call_id, call.name, call.arguments))
            .collect::<Vec<_>>();
        let results = dispatch_tool_calls(calls, &self.sink).await?;
        Ok(results
            .into_iter()
            .map(|result| responses::ToolOutput {
                output: result.result.content,
            })
            .collect())
    }
}

fn responses_tool_schema(tool: &ToolDescriptor) -> serde_json::Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters.clone(),
        "strict": tool.strict,
    })
}

// ---- credential + auth ----------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexCredential {
    #[serde(rename = "type", default)]
    credential_type: String,
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
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

fn is_near_expiry(credential: &CodexCredential) -> bool {
    credential
        .expires_at
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|expires_at| expires_at <= now_millis().saturating_add(REFRESH_MARGIN_MS))
        .unwrap_or(false)
}

fn provider_instructions(prompt: &PromptBundle) -> Result<String> {
    let instructions = prompt.rendered_text();
    if instructions.trim().is_empty() {
        bail!("core prompt bundle rendered empty; refusing to send empty Codex instructions")
    } else {
        Ok(instructions)
    }
}

fn auth_headers(access_token: &str, account_id: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![(
        "Authorization".to_string(),
        format!("Bearer {access_token}"),
    )];
    if let Some(account_id) = account_id {
        headers.push(("ChatGPT-Account-Id".to_string(), account_id.to_string()));
    }
    headers
}

fn ws_headers(access_token: &str, account_id: Option<&str>) -> Vec<(String, String)> {
    let mut headers = auth_headers(access_token, account_id);
    headers.push(("OpenAI-Beta".to_string(), WS_BETA_HEADER.to_string()));
    headers
}

fn persist_credential(ctx: &Context, credential: &CodexCredential) {
    if let Ok(json) = serde_json::to_string(credential) {
        let mut values = BTreeMap::new();
        values.insert(CREDENTIAL_SETTINGS_KEY.to_string(), json);
        ctx.store_secret(values);
    }
}

async fn run_oauth(client: &reqwest::Client) -> Result<CodexCredential> {
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
        ERROR_BODY_TIMEOUT,
        MAX_ERROR_BODY_CHARS,
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
async fn revoke_credential(client: &reqwest::Client, credential: &CodexCredential) {
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
                ERROR_BODY_TIMEOUT,
                MAX_ERROR_BODY_CHARS,
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

// ---- models ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RemoteModel {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    supported_in_api: Option<bool>,
    #[serde(default)]
    priority: Option<i64>,
}

async fn fetch_models(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<Vec<Model>> {
    let headers = auth_headers(access_token, account_id);
    let mut request = client
        .get(format!("{MODELS_ENDPOINT}?client_version={CLIENT_VERSION}"))
        .timeout(HTTP_TIMEOUT);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let response = http::ensure_success_redacted(
        response,
        ERROR_BODY_TIMEOUT,
        MAX_ERROR_BODY_CHARS,
        &headers,
        &[],
    )
    .await?;
    let payload: serde_json::Value = response.json().await?;
    let mut models: Vec<RemoteModel> =
        serde_json::from_value(payload.get("models").cloned().unwrap_or(json!([])))?;
    models.sort_by_key(|model| model.priority.unwrap_or(i64::MAX));

    Ok(models
        .into_iter()
        .filter(|model| model.supported_in_api.unwrap_or(true))
        .filter(|model| model.visibility.as_deref().unwrap_or("list") == "list")
        .enumerate()
        .map(|(index, model)| Model {
            label: model.display_name.unwrap_or_else(|| model.slug.clone()),
            id: model.slug,
            recommended: index == 0,
        })
        .collect())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
