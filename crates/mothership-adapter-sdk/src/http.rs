//! HTTP transport primitives shared by adapters: a client builder with a connect
//! timeout, a non-streaming JSON POST, and a streaming POST that hands a response
//! to [`crate::sse::read_sse`]. These are the HTTP/SSE tiers of a provider's
//! transport fallback chain.

use std::time::Duration;

use anyhow::{bail, Context, Result};

const DEFAULT_ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_ERROR_BODY_CHARS: usize = 300;

/// A reqwest client with a bounded connect timeout (so "server unreachable"
/// fails fast) and no overall timeout (streaming responses run long).
pub fn client(connect_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .build()
        .unwrap_or_default()
}

fn apply_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder
}

/// Non-streaming JSON POST (the HTTP-JSON fallback tier): sends `body`, returns
/// the parsed response, errors on non-2xx.
pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value> {
    let response = apply_headers(client.post(url).timeout(timeout).json(body), headers)
        .send()
        .await
        .context("http json post")?;
    let response = ensure_success_redacted(
        response,
        DEFAULT_ERROR_BODY_TIMEOUT,
        DEFAULT_MAX_ERROR_BODY_CHARS,
        headers,
        &[],
    )
    .await?;
    let text = response.text().await.unwrap_or_default();
    serde_json::from_str(&text).context("parse json response body")
}

/// Streaming POST (the SSE tier): returns the response for
/// [`crate::sse::read_sse`]. Errors on non-2xx before streaming starts.
pub async fn post_stream(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &serde_json::Value,
) -> Result<reqwest::Response> {
    post_stream_redacted(
        client,
        url,
        headers,
        body,
        DEFAULT_ERROR_BODY_TIMEOUT,
        DEFAULT_MAX_ERROR_BODY_CHARS,
        &[],
    )
    .await
}

/// Streaming POST with explicit provider-error redaction. Use this when an
/// adapter has provider secrets that may be echoed in an HTTP error body.
pub async fn post_stream_redacted(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &serde_json::Value,
    error_body_timeout: Duration,
    max_error_body_chars: usize,
    redacted_values: &[&str],
) -> Result<reqwest::Response> {
    let response = apply_headers(
        client
            .post(url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(body),
        headers,
    )
    .send()
    .await
    .context("http stream post")?;
    ensure_success_redacted(
        response,
        error_body_timeout,
        max_error_body_chars,
        headers,
        redacted_values,
    )
    .await
}

pub async fn ensure_success_redacted(
    response: reqwest::Response,
    error_body_timeout: Duration,
    max_error_body_chars: usize,
    headers: &[(String, String)],
    redacted_values: &[&str],
) -> Result<reqwest::Response> {
    let secrets = provider_error_redactions(headers, redacted_values);
    ensure_success(response, error_body_timeout, max_error_body_chars, &secrets).await
}

pub async fn ensure_success(
    response: reqwest::Response,
    error_body_timeout: Duration,
    max_error_body_chars: usize,
    redacted_values: &[String],
) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = read_error_body(response, error_body_timeout).await;
    bail!(
        "HTTP {status}: {}",
        sanitize_provider_error(&body, redacted_values, max_error_body_chars)
    );
}

pub fn sanitize_provider_error(text: &str, redacted_values: &[String], max_chars: usize) -> String {
    let mut redacted = text.to_string();
    for value in redacted_values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        redacted = redacted.replace(value, "[redacted]");
        if let Some(token) = value.strip_prefix("Bearer ") {
            redacted = redacted.replace(token, "[redacted]");
        }
    }

    let redacted = redacted
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
        .join("\n");

    let truncated = redacted.chars().take(max_chars).collect::<String>();
    if truncated.trim().is_empty() {
        "[empty response body]".to_string()
    } else {
        truncated
    }
}

async fn read_error_body(response: reqwest::Response, timeout: Duration) -> String {
    match tokio::time::timeout(timeout, response.text()).await {
        Ok(Ok(text)) => text,
        Ok(Err(_)) | Err(_) => String::new(),
    }
}

fn header_secret_values(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            name == "authorization" || name.contains("api-key") || name.contains("api_key")
        })
        .map(|(_, value)| value.clone())
        .collect()
}

pub fn provider_error_redactions(
    headers: &[(String, String)],
    redacted_values: &[&str],
) -> Vec<String> {
    let mut secrets = header_secret_values(headers);
    secrets.extend(
        redacted_values
            .iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
    );
    secrets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_error_sanitizer_redacts_secrets_and_sensitive_lines() {
        let message = sanitize_provider_error(
            "Authorization: Bearer sk-secret\nbody sk-secret\nsafe line",
            &["sk-secret".to_string()],
            300,
        );

        assert!(!message.contains("sk-secret"));
        assert!(message.contains("[redacted sensitive provider error line]"));
        assert!(message.contains("body [redacted]"));
        assert!(message.contains("safe line"));
    }

    #[test]
    fn provider_error_sanitizer_truncates_after_redaction() {
        let message = sanitize_provider_error("abcdef", &[], 3);

        assert_eq!(message, "abc");
    }
}
