use anyhow::{bail, Result};
use mothership_adapter_sdk::protocol::{PromptBundle, ToolCallInvocation, ToolDescriptor};
use mothership_adapter_sdk::ws::WsSession;
use mothership_adapter_sdk::{ChatRequest, ChatRoundOutcome};
use mothership_openai_responses as responses;
use serde_json::json;

use crate::auth;
use crate::models::codex_fast_service_tier_for_model;

pub(crate) const RESPONSES_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
/// Codex Responses-over-WebSocket beta opt-in (matches the real Codex CLI).
pub(crate) const WS_BETA_HEADER: &str = "responses_websockets=2026-02-06";

pub(crate) async fn run_chat_round(
    client: &reqwest::Client,
    endpoint: &responses::Endpoint,
    ws: &mut Option<WsSession>,
    ws_disabled: &mut bool,
    access_token: &str,
    account_id: Option<&str>,
    request: ChatRequest,
    sink: &mut mothership_adapter_sdk::ChatSink,
) -> Result<ChatRoundOutcome> {
    let headers = auth::auth_headers(access_token, account_id);
    let instructions = provider_instructions(&request.prompt)?;
    if !request.tool_results.is_empty() && request.state.is_none() {
        bail!("Codex continuation with tool results requires adapter state");
    }

    let had_ws = !*ws_disabled;
    if had_ws && ws.is_none() {
        *ws = Some(WsSession::new(
            endpoint.wss_url.clone(),
            auth::ws_headers(access_token, account_id),
            responses::WS_SESSION_IDLE,
        ));
    }

    let cancellation = sink.cancellation_token();
    let mut on_delta = |text: &str| sink.delta(text);
    let tools = request
        .tools
        .iter()
        .map(responses_tool_schema)
        .collect::<Vec<_>>();
    let tool_outputs = request
        .tool_results
        .into_iter()
        .map(|result| responses::ToolCallOutput {
            call_id: result.tool_call_id,
            output: result.result.content,
        })
        .collect::<Vec<_>>();
    let ws_arg = if had_ws { ws.as_mut() } else { None };
    let service_tier = codex_fast_service_tier_for_model(&request.model, request.fast_mode);
    let (transport, round) = tokio::select! {
        result = responses::chat_round_with_state(
            client,
            endpoint,
            &headers,
            &request.model,
            &instructions,
            &request.messages,
            request.reasoning.as_ref(),
            request.state,
            tool_outputs,
            &request.extra_messages,
            ws_arg,
            &mut on_delta,
            &tools,
            service_tier,
        ) => result?,
        _ = cancellation.cancelled() => {
            if let Some(session) = ws.as_mut() {
                session.close().await;
            }
            return Ok(ChatRoundOutcome::default());
        }
    };

    // If WS was enabled but the answer came over a fallback, the WS tier is
    // unavailable on this backend/session; stop paying its connect cost.
    if had_ws && transport != responses::Transport::WebSocket {
        eprintln!(
            "codex-adapter: websocket unavailable, served via {transport:?}; disabling WS for this session"
        );
        *ws = None;
        *ws_disabled = true;
    }

    Ok(ChatRoundOutcome {
        state: Some(round.state),
        tool_calls: round
            .tool_calls
            .into_iter()
            .map(|call| ToolCallInvocation {
                tool_call_id: call.call_id,
                name: call.name,
                arguments: call.arguments,
            })
            .collect(),
    })
}

fn provider_instructions(prompt: &PromptBundle) -> Result<String> {
    let instructions = prompt.rendered_text();
    if instructions.trim().is_empty() {
        bail!("core prompt bundle rendered empty; refusing to send empty Codex instructions")
    } else {
        Ok(instructions)
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
