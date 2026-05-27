//! HTTP transport primitives shared by adapters: a client builder with a connect
//! timeout, a non-streaming JSON POST, and a streaming POST that hands a response
//! to [`crate::sse::read_sse`]. These are the HTTP/SSE tiers of a provider's
//! transport fallback chain.

use std::time::Duration;

use anyhow::{bail, Context, Result};

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
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("HTTP {status}: {}", truncate(&text, 300));
    }
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
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("HTTP {status}: {}", truncate(&text, 300));
    }
    Ok(response)
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
