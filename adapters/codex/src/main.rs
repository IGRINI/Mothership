//! Codex provider adapter — fully self-contained.
//!
//! All Codex provider logic lives here, in the plugin: its own OAuth (PKCE +
//! browser + localhost callback + token refresh), credential storage, model
//! catalog, and SSE chat transport. It depends only on the adapter protocol
//! SDK + raw crates, NEVER on mothership-core. The host stays provider-agnostic.
//!
//! Auth is lazy: the first chat with no valid credential opens the browser to
//! log in (an explicit "Connect" trigger is a later step). The credential is
//! stored next to the adapter executable and refreshed as needed.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use mothership_adapter_host::protocol::{
    AuthKind, ChatMessage, Model, ModelManagement, Outbound, Request,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use url::Url;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_ADDR: &str = "127.0.0.1:1455";
const SCOPE: &str = "openid profile email offline_access";
const MODELS_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/models";
const RESPONSES_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const CLIENT_VERSION: &str = "0.133.0";
const REFRESH_MARGIN_MS: u64 = 60_000;

fn main() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
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
            Request::Initialize { id } => emit(&mut stdout, &Outbound::Ack { id })?,
            Request::GetIdentity { id } => emit(
                &mut stdout,
                &Outbound::Identity {
                    id,
                    provider_id: "codex".to_string(),
                    provider_label: "Codex".to_string(),
                },
            )?,
            // Codex has no user-entered settings: models come from the server and
            // auth is browser OAuth.
            Request::GetSettingsSchema { id } => {
                emit(&mut stdout, &Outbound::SettingsSchema { id, fields: Vec::new() })?
            }
            Request::SetSettings { id, .. } => emit(&mut stdout, &Outbound::Ack { id })?,
            Request::GetAuthSchema { id } => {
                emit(&mut stdout, &Outbound::AuthSchema { id, auth: AuthKind::OauthInternal })?
            }
            // Never trigger OAuth here — listing models must not pop a browser
            // when the user merely opens Settings. Return what we can with an
            // already-stored credential, else empty.
            Request::GetModels { id } => {
                let models = stored_credential()
                    .and_then(|mut cred| refresh_if_needed(&client, &mut cred).ok().map(|_| cred))
                    .and_then(|cred| {
                        fetch_models(&client, &cred.access_token, cred.account_id.as_deref()).ok()
                    })
                    .unwrap_or_default();
                emit(
                    &mut stdout,
                    &Outbound::Models { id, management: ModelManagement::Server, models },
                )?;
            }
            Request::ChatStart { id, model, messages } => {
                if let Err(error) = chat(&client, id, &model, messages, &mut stdout) {
                    emit(&mut stdout, &Outbound::Error { id, message: error.to_string() })?;
                }
            }
            Request::ChatCancel { id } => emit(&mut stdout, &Outbound::Done { id })?,
        }
    }

    Ok(())
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

fn credential_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("credential.json")))
        .unwrap_or_else(|| PathBuf::from("credential.json"))
}

fn stored_credential() -> Option<CodexCredential> {
    std::fs::read_to_string(credential_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn save_credential(credential: &CodexCredential) -> anyhow::Result<()> {
    std::fs::write(credential_path(), serde_json::to_string_pretty(credential)?)?;
    Ok(())
}

/// Returns a valid access token + account id, running browser OAuth if there is
/// no stored credential and refreshing if it is near expiry.
fn ensure_token(client: &reqwest::blocking::Client) -> anyhow::Result<(String, Option<String>)> {
    let mut credential = match stored_credential() {
        Some(credential) => credential,
        None => {
            let credential = run_oauth(client)?;
            save_credential(&credential)?;
            credential
        }
    };
    refresh_if_needed(client, &mut credential)?;
    Ok((credential.access_token.clone(), credential.account_id.clone()))
}

fn refresh_if_needed(
    client: &reqwest::blocking::Client,
    credential: &mut CodexCredential,
) -> anyhow::Result<()> {
    let near_expiry = credential
        .expires_at
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|expires_at| expires_at <= now_millis().saturating_add(REFRESH_MARGIN_MS))
        .unwrap_or(false);
    if !near_expiry || credential.refresh_token.trim().is_empty() {
        return Ok(());
    }

    let tokens = post_token(
        client,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &credential.refresh_token),
            ("client_id", CLIENT_ID),
        ],
    )?;
    credential.access_token = tokens.access_token;
    if !tokens.refresh_token.is_empty() {
        credential.refresh_token = tokens.refresh_token;
    }
    if let Some(id_token) = tokens.id_token {
        credential.id_token = Some(id_token);
    }
    credential.expires_at = tokens
        .expires_in
        .map(|expires_in| now_millis().saturating_add(expires_in.saturating_mul(1000)).to_string());
    save_credential(credential)?;
    Ok(())
}

fn run_oauth(client: &reqwest::blocking::Client) -> anyhow::Result<CodexCredential> {
    let verifier = random_b64url(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64url(16);

    // Bind the callback listener before opening the browser to avoid a race.
    let listener = TcpListener::bind(CALLBACK_ADDR)?;
    open_browser(&authorize_url(&challenge, &state));
    eprintln!("codex-adapter: waiting for browser OAuth on {REDIRECT_URI}");

    let (code, returned_state) = wait_for_callback(listener)?;
    if returned_state != state {
        anyhow::bail!("OAuth state mismatch");
    }

    let tokens = post_token(
        client,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", &verifier),
        ],
    )?;

    let account_id = tokens
        .id_token
        .as_deref()
        .or(Some(tokens.access_token.as_str()))
        .and_then(parse_jwt_claims)
        .and_then(|claims| account_id_from_claims(&claims));

    Ok(CodexCredential {
        credential_type: "openai_codex_oauth".to_string(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        id_token: tokens.id_token,
        expires_at: tokens
            .expires_in
            .map(|e| now_millis().saturating_add(e.saturating_mul(1000)).to_string()),
        account_id,
    })
}

fn post_token(
    client: &reqwest::blocking::Client,
    form: &[(&str, &str)],
) -> anyhow::Result<TokenResponse> {
    let response = client
        .post(format!("{ISSUER}/oauth/token"))
        .form(form)
        .send()?
        .error_for_status()?;
    Ok(response.json()?)
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

fn wait_for_callback(listener: TcpListener) -> anyhow::Result<(String, String)> {
    let (mut stream, _) = listener.accept()?;
    let mut request_line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut request_line)?;
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("invalid OAuth callback request"))?;
    let url = Url::parse(&format!("http://localhost{path}"))?;
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    let code = query
        .get("code")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("callback missing code"))?;
    let state = query.get("state").cloned().unwrap_or_default();

    let body = "<!doctype html><meta charset=utf-8><title>Mothership</title>\
        <body style=\"font-family:system-ui;background:#0b0f14;color:#eaf1f8\">\
        <p>Codex authorized. You can close this tab and return to Mothership.</p>";
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    Ok((code, state))
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

fn random_b64url(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::getrandom(&mut buffer).expect("getrandom");
    URL_SAFE_NO_PAD.encode(buffer)
}

fn parse_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
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

fn fetch_models(
    client: &reqwest::blocking::Client,
    access_token: &str,
    account_id: Option<&str>,
) -> anyhow::Result<Vec<Model>> {
    let mut request = client
        .get(format!("{MODELS_ENDPOINT}?client_version={CLIENT_VERSION}"))
        .bearer_auth(access_token);
    if let Some(account_id) = account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let response = request.send()?.error_for_status()?;
    let payload: serde_json::Value = response.json()?;
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

// ---- chat (SSE) -----------------------------------------------------------

fn chat(
    client: &reqwest::blocking::Client,
    id: u64,
    model: &str,
    messages: Vec<ChatMessage>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let (access_token, account_id) = ensure_token(client)?;

    let input: Vec<serde_json::Value> = messages
        .iter()
        .filter(|message| !message.content.trim().is_empty())
        .map(|message| {
            let kind = if message.role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };
            json!({ "role": message.role, "content": [{ "type": kind, "text": message.content }] })
        })
        .collect();

    let body = json!({
        "model": model,
        "input": input,
        "stream": true,
        "store": false,
    });

    let mut request = client
        .post(RESPONSES_ENDPOINT)
        .bearer_auth(&access_token)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&body);
    if let Some(account_id) = account_id.as_deref() {
        request = request.header("ChatGPT-Account-Id", account_id);
    }

    let response = request.send()?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().unwrap_or_default();
        anyhow::bail!("Codex HTTP {status}: {}", text.chars().take(300).collect::<String>());
    }

    let reader = BufReader::new(response);
    for line in reader.lines() {
        let line = line?;
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let event: serde_json::Value = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(_) => continue,
        };
        let event_type = event.get("type").and_then(|value| value.as_str()).unwrap_or("");
        if matches!(event_type, "response.failed" | "error") {
            let message = event
                .get("message")
                .and_then(|value| value.as_str())
                .unwrap_or("provider error");
            anyhow::bail!("{message}");
        }
        if let Some(delta) = event.get("delta").and_then(|value| value.as_str()) {
            if !delta.is_empty() {
                emit(out, &Outbound::Delta { id, text: delta.to_string() })?;
            }
        }
        if event_type == "response.completed" {
            break;
        }
    }

    emit(out, &Outbound::Done { id })
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn emit(out: &mut impl Write, message: &Outbound) -> anyhow::Result<()> {
    let line = serde_json::to_string(message)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
