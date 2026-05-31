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
const MAX_TOOL_ROUNDS: usize = 8;

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

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub output: String,
}

#[async_trait::async_trait]
pub trait ToolDispatcher: Send + Sync {
    async fn dispatch(&self, call: ToolCall) -> Result<ToolOutput>;
}

#[derive(Debug, Clone)]
struct RawToolCall {
    item_id: Option<String>,
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    calls: Vec<PartialToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    item_id: Option<String>,
    output_index: usize,
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Debug, Default)]
struct RoundOutput {
    tool_calls: Vec<RawToolCall>,
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
/// `ws` is the caller's persistent session (it owns the slot and the WS-specific
/// headers): `Some(session)` enables the WS tier (connected lazily, reused across
/// turns); `None` disables it. Returns which transport produced the answer.
#[allow(clippy::too_many_arguments)]
pub async fn chat(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    auth_headers: &[(String, String)],
    model: &str,
    instructions: &str,
    messages: &[ChatMessage],
    ws: Option<&mut WsSession>,
    on_delta: &mut (dyn FnMut(&str) + Send),
) -> Result<Transport> {
    chat_with_optional_tools(
        client,
        endpoint,
        auth_headers,
        model,
        instructions,
        messages,
        ws,
        on_delta,
        &[],
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn chat_with_tools(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    auth_headers: &[(String, String)],
    model: &str,
    instructions: &str,
    messages: &[ChatMessage],
    ws: Option<&mut WsSession>,
    on_delta: &mut (dyn FnMut(&str) + Send),
    tools: &[Value],
    dispatcher: &dyn ToolDispatcher,
) -> Result<Transport> {
    chat_with_optional_tools(
        client,
        endpoint,
        auth_headers,
        model,
        instructions,
        messages,
        ws,
        on_delta,
        tools,
        Some(dispatcher),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn chat_with_optional_tools(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    auth_headers: &[(String, String)],
    model: &str,
    instructions: &str,
    messages: &[ChatMessage],
    mut ws: Option<&mut WsSession>,
    on_delta: &mut (dyn FnMut(&str) + Send),
    tools: &[Value],
    dispatcher: Option<&dyn ToolDispatcher>,
) -> Result<Transport> {
    let mut input = build_input(messages);

    for _ in 0..MAX_TOOL_ROUNDS {
        let (transport, round) = chat_round(
            client,
            endpoint,
            auth_headers,
            model,
            instructions,
            &input,
            ws.as_deref_mut(),
            on_delta,
            tools,
        )
        .await?;

        if round.tool_calls.is_empty() {
            return Ok(transport);
        }

        let Some(dispatcher) = dispatcher else {
            bail!("model requested a tool call, but no tool dispatcher is configured");
        };

        for call in round.tool_calls {
            let output = dispatcher.dispatch(tool_call_for_dispatch(&call)).await?;
            input.push(function_call_input_item(&call));
            input.push(json!({
                "type": "function_call_output",
                "call_id": call.call_id,
                "output": output.output,
            }));
        }
    }

    bail!("responses tool loop exceeded {MAX_TOOL_ROUNDS} rounds")
}

#[allow(clippy::too_many_arguments)]
async fn chat_round(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    auth_headers: &[(String, String)],
    model: &str,
    instructions: &str,
    input: &[Value],
    ws: Option<&mut WsSession>,
    on_delta: &mut (dyn FnMut(&str) + Send),
    tools: &[Value],
) -> Result<(Transport, RoundOutput)> {
    let mut committed = false;

    // 1. WebSocket (primary), when enabled.
    if let Some(session) = ws {
        match chat_ws(
            session,
            model,
            instructions,
            input,
            tools,
            &mut committed,
            on_delta,
        )
        .await
        {
            Ok(round) => return Ok((Transport::WebSocket, round)),
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
    match chat_sse(
        client,
        endpoint,
        auth_headers,
        model,
        instructions,
        input,
        tools,
        &mut committed,
        on_delta,
    )
    .await
    {
        Ok(round) => return Ok((Transport::Sse, round)),
        Err(error) => {
            if committed {
                return Err(error);
            }
            eprintln!("openai-responses: sse tier failed, falling back: {error:#}");
        }
    }

    // 3. Non-streaming HTTP-JSON (last resort; only reached pre-commit).
    let round = chat_json(
        client,
        endpoint,
        auth_headers,
        model,
        instructions,
        input,
        tools,
        on_delta,
    )
    .await?;
    Ok((Transport::Json, round))
}

async fn chat_ws(
    session: &mut WsSession,
    model: &str,
    instructions: &str,
    input: &[Value],
    tools: &[Value],
    committed: &mut bool,
    on_delta: &mut (dyn FnMut(&str) + Send),
) -> Result<RoundOutput> {
    let body = build_request_from_input(model, instructions, input, true, tools);
    session.send_text(body.to_string()).await?;
    let mut accumulator = ToolCallAccumulator::default();
    loop {
        match session.next_text(WS_READ_IDLE_TIMEOUT).await? {
            Some(frame) => {
                let Some(value) = parse_frame(&frame) else {
                    continue;
                };
                match handle_stream_event(&value, &mut accumulator, committed, on_delta)? {
                    Event::OutputText(text) => {
                        *committed = true;
                        on_delta(&text);
                    }
                    Event::Completed => {
                        return Ok(RoundOutput {
                            tool_calls: accumulator.finish(),
                        })
                    }
                    Event::Failed(message) => bail!("{message}"),
                    Event::Reasoning | Event::Other => {}
                }
            }
            None => {
                if *committed {
                    return Ok(RoundOutput {
                        tool_calls: accumulator.finish(),
                    });
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
    input: &[Value],
    tools: &[Value],
    committed: &mut bool,
    on_delta: &mut (dyn FnMut(&str) + Send),
) -> Result<RoundOutput> {
    let body = build_request_from_input(model, instructions, input, true, tools);
    let response = http::post_stream(client, &endpoint.https_url, auth_headers, &body).await?;
    let mut accumulator = ToolCallAccumulator::default();
    sse::read_sse(response, SSE_IDLE_TIMEOUT, |payload| {
        if payload == "[DONE]" {
            return Ok(false);
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return Ok(true);
        };
        match handle_stream_event(&value, &mut accumulator, committed, on_delta)? {
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
    .await?;
    Ok(RoundOutput {
        tool_calls: accumulator.finish(),
    })
}

async fn chat_json(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    auth_headers: &[(String, String)],
    model: &str,
    instructions: &str,
    input: &[Value],
    tools: &[Value],
    on_delta: &mut (dyn FnMut(&str) + Send),
) -> Result<RoundOutput> {
    let body = build_request_from_input(model, instructions, input, false, tools);
    let value = http::post_json(
        client,
        &endpoint.https_url,
        auth_headers,
        &body,
        JSON_TIMEOUT,
    )
    .await?;
    let text = extract_output_text(&value);
    let tool_calls = extract_tool_calls(&value);
    if text.is_empty() && tool_calls.is_empty() {
        bail!("empty response from provider");
    }
    on_delta(&text);
    Ok(RoundOutput { tool_calls })
}

/// Build the Responses request body. System messages feed `instructions`
/// (already resolved by the caller); the rest become `input` items.
pub fn build_request(
    model: &str,
    instructions: &str,
    messages: &[ChatMessage],
    stream: bool,
) -> Value {
    build_request_from_input(model, instructions, &build_input(messages), stream, &[])
}

fn build_input(messages: &[ChatMessage]) -> Vec<Value> {
    messages
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
        .collect()
}

fn build_request_from_input(
    model: &str,
    instructions: &str,
    input: &[Value],
    stream: bool,
    tools: &[Value],
) -> Value {
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "stream": stream,
        "store": false,
    });
    if !tools.is_empty() {
        if let Some(object) = body.as_object_mut() {
            object.insert("tools".to_string(), Value::Array(tools.to_vec()));
            object.insert("tool_choice".to_string(), Value::String("auto".to_string()));
            object.insert("parallel_tool_calls".to_string(), Value::Bool(true));
        }
    }
    body
}

fn handle_stream_event(
    value: &Value,
    accumulator: &mut ToolCallAccumulator,
    committed: &mut bool,
    _on_delta: &mut (dyn FnMut(&str) + Send),
) -> Result<Event> {
    let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "response.output_item.added" => {
            if let Some(item) = value.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    *committed = true;
                    accumulator.merge_item(item, output_index(value));
                }
            }
            Ok(Event::Other)
        }
        "response.function_call_arguments.delta" => {
            *committed = true;
            accumulator.merge_arguments_delta(
                output_index(value),
                value.get("item_id").and_then(Value::as_str),
                value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            Ok(Event::Other)
        }
        "response.function_call_arguments.done" => {
            *committed = true;
            if let Some(item) = value.get("item") {
                accumulator.merge_item(item, output_index(value));
            } else {
                accumulator.merge_top_level_done(value);
            }
            Ok(Event::Other)
        }
        "response.output_item.done" => {
            if let Some(item) = value.get("item") {
                match item.get("type").and_then(Value::as_str) {
                    Some("function_call") => {
                        *committed = true;
                        accumulator.merge_item(item, output_index(value));
                    }
                    _ => {}
                }
            }
            Ok(Event::Other)
        }
        _ => Ok(classify(value)),
    }
}

fn output_index(value: &Value) -> usize {
    value
        .get("output_index")
        .and_then(Value::as_u64)
        .map(|index| index as usize)
        .unwrap_or(0)
}

impl ToolCallAccumulator {
    fn merge_item(&mut self, item: &Value, output_index: usize) {
        let call = self.find_or_create(output_index, item.get("id").and_then(Value::as_str));
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                call.item_id = Some(id.to_string());
            }
        }
        if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
            if !call_id.is_empty() {
                call.call_id = Some(call_id.to_string());
            }
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            if !name.is_empty() {
                call.name = Some(name.to_string());
            }
        }
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
            call.arguments = arguments.to_string();
        }
    }

    fn merge_top_level_done(&mut self, value: &Value) {
        let call = self.find_or_create(
            output_index(value),
            value.get("item_id").and_then(Value::as_str),
        );
        if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
            call.arguments = arguments.to_string();
        }
        if let Some(call_id) = value.get("call_id").and_then(Value::as_str) {
            call.call_id = Some(call_id.to_string());
        }
        if let Some(name) = value.get("name").and_then(Value::as_str) {
            call.name = Some(name.to_string());
        }
    }

    fn merge_arguments_delta(&mut self, output_index: usize, item_id: Option<&str>, delta: &str) {
        let call = self.find_or_create(output_index, item_id);
        call.arguments.push_str(delta);
    }

    fn finish(mut self) -> Vec<RawToolCall> {
        self.calls.sort_by_key(|call| call.output_index);
        self.calls
            .into_iter()
            .filter_map(|call| {
                let name = call.name?;
                if name.trim().is_empty() {
                    return None;
                }
                Some(RawToolCall {
                    item_id: call.item_id,
                    call_id: call
                        .call_id
                        .unwrap_or_else(|| format!("responses_call_{}", call.output_index)),
                    name,
                    arguments: call.arguments,
                })
            })
            .collect()
    }

    fn find_or_create(
        &mut self,
        output_index: usize,
        item_id: Option<&str>,
    ) -> &mut PartialToolCall {
        if let Some(item_id) = item_id {
            if let Some(position) = self
                .calls
                .iter()
                .position(|call| call.item_id.as_deref() == Some(item_id))
            {
                return &mut self.calls[position];
            }
        }
        if let Some(position) = self
            .calls
            .iter()
            .position(|call| call.output_index == output_index)
        {
            return &mut self.calls[position];
        }
        self.calls.push(PartialToolCall {
            output_index,
            item_id: item_id.map(ToOwned::to_owned),
            ..PartialToolCall::default()
        });
        self.calls.last_mut().expect("tool call just inserted")
    }
}

fn tool_call_for_dispatch(call: &RawToolCall) -> ToolCall {
    ToolCall {
        call_id: call.call_id.clone(),
        name: call.name.clone(),
        arguments: serde_json::from_str(&call.arguments)
            .unwrap_or_else(|_| json!({ "rawArguments": call.arguments.clone() })),
    }
}

fn function_call_input_item(call: &RawToolCall) -> Value {
    let mut item = json!({
        "type": "function_call",
        "call_id": call.call_id.as_str(),
        "name": call.name.as_str(),
        "arguments": call.arguments.as_str(),
    });
    if let (Some(object), Some(item_id)) = (item.as_object_mut(), call.item_id.as_deref()) {
        object.insert("id".to_string(), Value::String(item_id.to_string()));
    }
    item
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
    let payload = trimmed
        .strip_prefix("data:")
        .map(str::trim)
        .unwrap_or(trimmed);
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

fn extract_tool_calls(value: &Value) -> Vec<RawToolCall> {
    value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, item)| {
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return None;
            }
            let name = item.get("name").and_then(Value::as_str)?.to_string();
            if name.trim().is_empty() {
                return None;
            }
            Some(RawToolCall {
                item_id: item
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                call_id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| format!("responses_call_{index}")),
                name,
                arguments: item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
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
        let reasoning =
            classify(&json!({"type":"response.reasoning_summary_text.delta","delta":"think"}));
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
        let endpoint =
            Endpoint::from_https("https://chatgpt.com/backend-api/codex/responses").unwrap();
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
    fn accumulates_streaming_function_call_arguments() {
        let mut accumulator = ToolCallAccumulator::default();
        let mut committed = false;
        let mut on_delta = |_text: &str| {};

        handle_stream_event(
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "fc_1",
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "run_command",
                    "arguments": ""
                }
            }),
            &mut accumulator,
            &mut committed,
            &mut on_delta,
        )
        .unwrap();
        handle_stream_event(
            &json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_1",
                "delta": "{\"program\":\"git\"}"
            }),
            &mut accumulator,
            &mut committed,
            &mut on_delta,
        )
        .unwrap();

        let calls = accumulator.finish();
        assert!(committed);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "call_1");
        assert_eq!(calls[0].name, "run_command");
        assert_eq!(calls[0].arguments, "{\"program\":\"git\"}");
    }

    #[test]
    fn extracts_non_streaming_function_call() {
        let body = json!({
            "output": [{
                "id": "fc_1",
                "type": "function_call",
                "call_id": "call_1",
                "name": "run_command",
                "arguments": "{\"program\":\"git\"}"
            }]
        });

        let calls = extract_tool_calls(&body);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "call_1");
        assert_eq!(calls[0].name, "run_command");
    }

    #[test]
    fn parse_frame_tolerates_data_prefix() {
        assert!(parse_frame("data: {\"type\":\"x\"}").is_some());
        assert!(parse_frame("[DONE]").is_none());
        assert!(parse_frame("not json").is_none());
    }
}
