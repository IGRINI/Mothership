//! Generic OAuth helpers for provider adapters.
//!
//! Provider adapters still own provider-specific endpoints, client ids, scopes,
//! token request/response shapes, and account-claim interpretation. This module
//! only provides reusable OAuth mechanics.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCallback {
    pub code: String,
    pub state: String,
}

pub fn pkce_pair(verifier_bytes: usize) -> PkcePair {
    let verifier = random_b64url(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkcePair {
        verifier,
        challenge,
    }
}

pub fn random_state(bytes: usize) -> String {
    random_b64url(bytes)
}

pub async fn wait_for_localhost_callback(
    listener: &TcpListener,
    accept_timeout: Duration,
    read_timeout: Duration,
    success_body: &str,
) -> Result<OAuthCallback> {
    let (stream, _) = tokio::time::timeout(accept_timeout, listener.accept())
        .await
        .map_err(|_| anyhow!("timed out waiting for browser OAuth callback"))?
        .context("accept OAuth callback")?;
    let (read_half, mut write_half) = stream.into_split();
    let mut request_line = String::new();
    tokio::time::timeout(
        read_timeout,
        BufReader::new(read_half).read_line(&mut request_line),
    )
    .await
    .map_err(|_| anyhow!("timed out reading OAuth callback request"))??;

    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("invalid OAuth callback request"))?;
    let url = Url::parse(&format!("http://localhost{path}"))?;
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    let code = query
        .get("code")
        .cloned()
        .ok_or_else(|| anyhow!("callback missing code"))?;
    let state = query.get("state").cloned().unwrap_or_default();

    let body = success_body;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = write_half.write_all(response.as_bytes()).await;

    Ok(OAuthCallback { code, state })
}

pub fn parse_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn random_b64url(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::getrandom(&mut buffer).expect("getrandom");
    URL_SAFE_NO_PAD.encode(buffer)
}

#[cfg(test)]
mod tests {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde_json::json;

    use super::*;

    #[test]
    fn pkce_pair_uses_s256_challenge() {
        let pair = pkce_pair(32);
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pair.verifier.as_bytes()));

        assert_eq!(pair.challenge, expected);
        assert!(!pair.verifier.contains('='));
    }

    #[test]
    fn parses_jwt_claims_payload() {
        let header = URL_SAFE_NO_PAD.encode(b"{}");
        let payload = URL_SAFE_NO_PAD.encode(json!({ "sub": "user_1" }).to_string());
        let token = format!("{header}.{payload}.signature");

        let claims = parse_jwt_claims(&token).expect("claims");

        assert_eq!(claims["sub"], "user_1");
    }
}
