//! OpenAI **Responses**-family transport.
//!
//! Restores (as shared, reusable code) the multi-transport behaviour the pre-pivot
//! core had and the self-contained Codex adapter dropped:
//! - **WebSocket-primary, then HTTP-SSE, then non-streaming HTTP-JSON** fallback;
//! - **retryable/committed classification** — a transport failure *before* the
//!   first answer token is retryable (fall to the next transport); *after* it is
//!   committed (propagate, never silently re-run);
//! - **structured event parsing** — parse by event `type`, so reasoning deltas
//!   are recognised and NOT leaked into the answer (the old naive "grab any
//!   `delta`" bug);
//! - **idle timeouts** on every streaming read.
//!
//! Provider-family-specific (the Responses wire shapes). A `/chat/completions`
//! provider like OpenRouter does not use this. Built on the SDK transport
//! primitives; the caller supplies the URLs, auth headers, and a persistent
//! [`WsSession`] slot it owns across turns.

use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use mothership_adapter_sdk::http;
use mothership_adapter_sdk::protocol::ChatMessage;
use mothership_adapter_sdk::sse;
use mothership_adapter_sdk::ws::WsSession;

/// Fast-fail bound on reaching the backend.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle gap allowed between SSE chunks before the stream is declared stalled.
pub const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// Idle gap allowed between WebSocket frames within a turn.
pub const WS_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the persistent WS may sit unused before the adapter closes it.
pub const WS_SESSION_IDLE: Duration = Duration::from_secs(60);
/// Overall bound on the non-streaming JSON fallback.
pub const JSON_TIMEOUT: Duration = Duration::from_secs(180);

/// Which transport produced the answer (so the caller can disable a WS tier that
/// keeps falling back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    WebSocket,
    Sse,
    Json,
}

/// A parsed Responses stream event.
enum Event {
    /// A chunk of the visible answer.
    OutputText(String),
    /// Reasoning/summary text — recognised but never emitted as answer.
    Reasoning,
    /// The turn finished successfully.
    Completed,
    /// The backend reported a failure.
    Failed(String),
    /// Anything else (created, in-progress, item added, …).
    Other,
}

/// Endpoint config for a Responses backend.
pub struct Endpoint {
    /// `https://…/responses`
    pub https_url: String,
    /// `wss://…/responses` (derived from `https_url`).
    pub wss_url: String,
}

impl Endpoint {
    /// Build from the HTTPS responses URL, deriving the `wss://` form.
    pub fn from_https(https_url: impl Into<String>) -> Result<Self> {
        let https_url = https_url.into();
        let wss_url = to_ws_url(&https_url)?;
        Ok(Self { https_url, wss_url })
    }
}

/// Run one chat turn against a Responses backend, trying WebSocket first (when a
/// session slot is provided and enabled), then SSE, then non-streaming JSON.
///
/// `ws` is the caller's persistent session slot: `Some(slot)` enables the WS
/// tier (the slot is lazily connected and reused across turns); `None` disables
/// it. Returns which transport produced the answer.
#[allow(clippy::too_many_arguments)]
pub async fn chat(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    auth_headers: &[(String, String)],
    model: &str,
    instructions: &str,
    messages: &[ChatMessage],
    ws: Option<&mut Option<WsSession>>,
    on_delta: &mut dyn FnMut(&str),
) -> Result<Transport> {
    let mut committed = false;

    // 1. WebSocket (primary), when enabled.
    if let Some(slot) = ws {
        if slot.is_none() {
            *slot = Some(WsSession::new(
                endpoint.wss_url.clone(),
                auth_headers.to_vec(),
                WS_SESSION_IDLE,
            ));
        }
        let session = slot.as_mut().expect("ws session present");
        match chat_ws(session, model, instructions, messages, &mut committed, on_delta).await {
            Ok(()) => return Ok(Transport::WebSocket),
            Err(error) => {
                if committed {
                    return Err(error); // already streaming answer — do not re-run
                }
                eprintln!("openai-responses: websocket tier failed, falling back: {error:#}");
                session.close().await;
            }
        }
    }

    // 2. HTTP-SSE.
    match chat_sse(client, endpoint, auth_headers, model, instructions, messages, &mut committed, on_delta).await {
        Ok(()) => return Ok(Transport::Sse),
        Err(error) => {
            if committed {
                return Err(error);
            }
            eprintln!("openai-responses: sse tier failed, falling back: {error:#}");
        }
    }

    // 3. Non-streaming HTTP-JSON (last resort; only reached pre-commit).
    chat_json(client, endpoint, auth_headers, model, instructions, messages, on_delta).await?;
    Ok(Transport::Json)
}

async fn chat_ws(
    session: &mut WsSession,
    model: &str,
    instructions: &str,
    messages: &[ChatMessage],
    committed: &mut bool,
    on_delta: &mut dyn FnMut(&str),
) -> Result<()> {
    let body = build_request(model, instructions, messages, true);
    session.send_text(body.to_string()).await?;
    loop {
        match session.next_text(WS_READ_IDLE_TIMEOUT).await? {
            Some(frame) => {
                let Some(value) = parse_frame(&frame) else {
                    continue;
                };
                match classify(&value) {
                    Event::OutputText(text) => {
                        *committed = true;
                        on_delta(&text);
                    }
                    Event::Completed => return Ok(()),
                    Event::Failed(message) => bail!("{message}"),
                    Event::Reasoning | Event::Other => {}
                }
            }
            None => {
                if *committed {
                    return Ok(());
                }
                bail!("websocket closed before any response");
            }
        }
    }
}

async fn chat_sse(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    auth_headers: &[(String, String)],
    model: &str,
    instructions: &str,
    messages: &[ChatMessage],
    committed: &mut bool,
    on_delta: &mut dyn FnMut(&str),
) -> Result<()> {
    let body = build_request(model, instructions, messages, true);
    let response = http::post_stream(client, &endpoint.https_url, auth_headers, &body).await?;
    sse::read_sse(response, SSE_IDLE_TIMEOUT, |payload| {
        if payload == "[DONE]" {
            return Ok(false);
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return Ok(true);
        };
        match classify(&value) {
            Event::OutputText(text) => {
                *committed = true;
                on_delta(&text);
                Ok(true)
            }
            Event::Completed => Ok(false),
            Event::Failed(message) => bail!("{message}"),
            Event::Reasoning | Event::Other => Ok(true),
        }
    })
    .await
}

async fn chat_json(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    auth_headers: &[(String, String)],
    model: &str,
    instructions: &str,
    messages: &[ChatMessage],
    on_delta: &mut dyn FnMut(&str),
) -> Result<()> {
    let body = build_request(model, instructions, messages, false);
    let value = http::post_json(
        client,
        &endpoint.https_url,
        auth_headers,
        &body,
        JSON_TIMEOUT,
    )
    .await?;
    let text = extract_output_text(&value);
    if text.is_empty() {
        bail!("empty response from provider");
    }
    on_delta(&text);
    Ok(())
}

/// Build the Responses request body. System messages feed `instructions`
/// (already resolved by the caller); the rest become `input` items.
pub fn build_request(
    model: &str,
    instructions: &str,
    messages: &[ChatMessage],
    stream: bool,
) -> Value {
    let input: Vec<Value> = messages
        .iter()
        .filter(|message| message.role != "system")
        .filter(|message| !message.content.trim().is_empty())
        .map(|message| {
            let kind = if message.role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };
            json!({
                "role": message.role,
                "content": [{ "type": kind, "text": message.content }],
            })
        })
        .collect();

    json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "stream": stream,
        "store": false,
    })
}

/// Parse a Responses event by its `type` — the structured parsing that keeps
/// reasoning out of the answer.
fn classify(value: &Value) -> Event {
    let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "response.output_text.delta" => value
            .get("delta")
            .and_then(Value::as_str)
            .filter(|delta| !delta.is_empty())
            .map(|delta| Event::OutputText(delta.to_string()))
            .unwrap_or(Event::Other),
        "response.reasoning_summary_text.delta"
        | "response.reasoning_text.delta"
        | "response.reasoning_summary.delta" => Event::Reasoning,
        "response.completed" => Event::Completed,
        "response.failed" | "error" => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| {
                    value
                        .get("response")
                        .and_then(|r| r.get("error"))
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                })
                .or_else(|| {
                    value
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("provider error")
                .to_string();
            Event::Failed(message)
        }
        _ => Event::Other,
    }
}

/// Some WS frames may carry an SSE-style `data:` prefix; tolerate both and parse
/// the JSON event. Returns `None` for non-JSON / control frames.
fn parse_frame(frame: &str) -> Option<Value> {
    let trimmed = frame.trim();
    let payload = trimmed.strip_prefix("data:").map(str::trim).unwrap_or(trimmed);
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    serde_json::from_str(payload).ok()
}

/// Pull the visible answer text out of a non-streaming Responses body
/// (`output[].content[].text` where `type == "output_text"`).
fn extract_output_text(value: &Value) -> String {
    let mut text = String::new();
    if let Some(output) = value.get("output").and_then(Value::as_array) {
        for item in output {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        if let Some(chunk) = part.get("text").and_then(Value::as_str) {
                            text.push_str(chunk);
                        }
                    }
                }
            }
        }
    }
    // Fallback: some shapes expose a flat `output_text`.
    if text.is_empty() {
        if let Some(flat) = value.get("output_text").and_then(Value::as_str) {
            text.push_str(flat);
        }
    }
    text
}

/// Derive the `wss://` URL from an `https://` (or `http://`) responses URL.
fn to_ws_url(https_url: &str) -> Result<String> {
    let mut parsed = url::Url::parse(https_url)?;
    let scheme = match parsed.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => bail!("unexpected responses URL scheme: {other}"),
    };
    parsed
        .set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("failed to set ws scheme"))?;
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_output_vs_reasoning() {
        let out = classify(&json!({"type":"response.output_text.delta","delta":"hi"}));
        assert!(matches!(out, Event::OutputText(t) if t == "hi"));
        let reasoning = classify(&json!({"type":"response.reasoning_summary_text.delta","delta":"think"}));
        assert!(matches!(reasoning, Event::Reasoning));
    }

    #[test]
    fn classifies_completion_and_failure() {
        assert!(matches!(
            classify(&json!({"type":"response.completed"})),
            Event::Completed
        ));
        let failed = classify(&json!({"type":"response.failed","message":"boom"}));
        assert!(matches!(failed, Event::Failed(m) if m == "boom"));
    }

    #[test]
    fn derives_wss_url() {
        let endpoint = Endpoint::from_https("https://chatgpt.com/backend-api/codex/responses").unwrap();
        assert_eq!(
            endpoint.wss_url,
            "wss://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn extracts_non_streaming_text() {
        let body = json!({
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "hello world" }]
            }]
        });
        assert_eq!(extract_output_text(&body), "hello world");
    }

    #[test]
    fn parse_frame_tolerates_data_prefix() {
        assert!(parse_frame("data: {\"type\":\"x\"}").is_some());
        assert!(parse_frame("[DONE]").is_none());
        assert!(parse_frame("not json").is_none());
    }
}
