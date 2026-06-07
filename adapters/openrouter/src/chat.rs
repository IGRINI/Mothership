use std::time::Duration;

use anyhow::Context as _;
use mothership_adapter_sdk::http;
use mothership_adapter_sdk::protocol::{
    ReasoningConfig, ToolCallInvocation, ToolCallResponse, ToolDescriptor,
};
use mothership_adapter_sdk::sse;
use mothership_adapter_sdk::tools::parse_tool_arguments;
use mothership_adapter_sdk::{ChatRequest, ChatRoundOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::models::openrouter_fast_service_tier_for_model;
use crate::settings::{auth_headers, OpenRouterSettings};

const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_SSE_LINE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRouterRoundState {
    conversation: Vec<Value>,
}

fn conversation_from_request(request: &ChatRequest) -> anyhow::Result<Vec<Value>> {
    match &request.state {
        Some(state) => Ok(
            serde_json::from_value::<OpenRouterRoundState>(state.clone())
                .context("decode OpenRouter continuation state")?
                .conversation,
        ),
        None => {
            let mut conversation = Vec::new();
            let instructions = request.prompt.rendered_text();
            if !instructions.trim().is_empty() {
                conversation.push(json!({ "role": "system", "content": instructions }));
            }
            conversation.extend(
                request
                    .messages
                    .iter()
                    .map(|message| json!({ "role": message.role, "content": message.content })),
            );
            Ok(conversation)
        }
    }
}

pub(crate) async fn stream_chat(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
    request: &ChatRequest,
    sink: &mut mothership_adapter_sdk::ChatSink,
) -> anyhow::Result<ChatRoundOutcome> {
    if settings.api_key().is_empty() {
        anyhow::bail!("missing OpenRouter API key (set it in adapter settings)");
    }
    let url = format!(
        "{}/chat/completions",
        settings.base_url().trim_end_matches('/')
    );
    if !request.tool_results.is_empty() && request.state.is_none() {
        anyhow::bail!("OpenRouter continuation with tool results requires adapter state");
    }

    let mut conversation = conversation_from_request(request)?;
    for tool_result in &request.tool_results {
        conversation.push(tool_result_message(tool_result));
    }
    for message in &request.extra_messages {
        conversation.push(json!({ "role": message.role, "content": message.content }));
    }

    let round = stream_chat_once(
        client,
        settings,
        &url,
        &request.model,
        &conversation,
        sink,
        &request.tools,
        request.reasoning.as_ref(),
        openrouter_fast_service_tier_for_model(&request.model, request.fast_mode),
    )
    .await?;

    if round.tool_calls.is_empty() {
        if !round.assistant_content.trim().is_empty() {
            conversation.push(json!({
                "role": "assistant",
                "content": round.assistant_content,
            }));
        }
    } else {
        conversation.push(assistant_tool_call_message(
            &round.tool_calls,
            &round.assistant_content,
        ));
    }

    Ok(ChatRoundOutcome {
        state: Some(serde_json::to_value(OpenRouterRoundState { conversation })?),
        tool_calls: round
            .tool_calls
            .into_iter()
            .map(|call| ToolCallInvocation {
                tool_call_id: call.id,
                name: call.name,
                arguments: parse_tool_arguments(&call.arguments),
            })
            .collect(),
    })
}

async fn stream_chat_once(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
    url: &str,
    model: &str,
    messages: &[Value],
    sink: &mut mothership_adapter_sdk::ChatSink,
    tools: &[ToolDescriptor],
    reasoning: Option<&ReasoningConfig>,
    service_tier: Option<&str>,
) -> anyhow::Result<ChatRound> {
    let body = openrouter_chat_body(model, messages, tools, reasoning, service_tier);

    let api_key = settings.api_key();
    let headers = auth_headers(api_key);
    let redacted_values = [api_key];
    let request = http::post_stream_redacted(
        client,
        url,
        &headers,
        &body,
        http::DEFAULT_ERROR_BODY_TIMEOUT,
        http::DEFAULT_MAX_ERROR_BODY_CHARS,
        &redacted_values,
    );
    let response = tokio::select! {
        result = request => result.context("send OpenRouter chat request")?,
        _ = sink.cancelled() => return Ok(ChatRound::default()),
    };

    read_sse_stream(response, sink).await
}

fn openrouter_chat_body(
    model: &str,
    messages: &[Value],
    tools: &[ToolDescriptor],
    reasoning: Option<&ReasoningConfig>,
    service_tier: Option<&str>,
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
    });
    if !tools.is_empty() {
        if let Some(object) = body.as_object_mut() {
            object.insert(
                "tools".to_string(),
                Value::Array(tools.iter().map(chat_completions_tool_schema).collect()),
            );
            object.insert("tool_choice".to_string(), json!("auto"));
            object.insert("parallel_tool_calls".to_string(), json!(true));
        }
    }
    if let Some(reasoning) = reasoning.and_then(openrouter_reasoning_value) {
        if let Some(object) = body.as_object_mut() {
            object.insert("reasoning".to_string(), reasoning);
        }
    }
    if let Some(service_tier) = service_tier.filter(|tier| !tier.trim().is_empty()) {
        if let Some(object) = body.as_object_mut() {
            object.insert(
                "service_tier".to_string(),
                Value::String(service_tier.to_string()),
            );
        }
    }
    body
}

fn openrouter_reasoning_value(reasoning: &ReasoningConfig) -> Option<Value> {
    if reasoning.is_empty() {
        return None;
    }

    let mut object = serde_json::Map::new();
    if let Some(effort) = reasoning.effort {
        object.insert(
            "effort".to_string(),
            Value::String(effort.as_wire_str().to_string()),
        );
    }
    if let Some(budget_tokens) = reasoning.budget_tokens {
        object.insert("max_tokens".to_string(), json!(budget_tokens));
    }

    (!object.is_empty()).then(|| Value::Object(object))
}

async fn read_sse_stream(
    response: reqwest::Response,
    sink: &mut mothership_adapter_sdk::ChatSink,
) -> anyhow::Result<ChatRound> {
    let mut tool_calls = Vec::<StreamingToolCall>::new();
    let mut assistant_content = String::new();

    sse::read_sse_cancellable(
        response,
        STREAM_IDLE_TIMEOUT,
        MAX_SSE_LINE_BYTES,
        sink.cancellation_token(),
        |payload| {
            if handle_sse_payload(payload, sink, &mut tool_calls, &mut assistant_content)? {
                return Ok(false);
            }
            Ok(true)
        },
    )
    .await?;

    if sink.is_cancelled() {
        return Ok(ChatRound::default());
    }

    Ok(ChatRound {
        assistant_content,
        tool_calls: finish_tool_calls(tool_calls),
    })
}

fn handle_sse_payload(
    data: &str,
    sink: &mothership_adapter_sdk::ChatSink,
    tool_calls: &mut Vec<StreamingToolCall>,
    assistant_content: &mut String,
) -> anyhow::Result<bool> {
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(true);
    }
    if let Ok(value) = serde_json::from_str::<Value>(data) {
        let delta = &value["choices"][0]["delta"];
        if let Some(text) = delta["content"].as_str() {
            if !text.is_empty() {
                sink.delta(text);
                assistant_content.push_str(text);
            }
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                merge_tool_call_delta(tool_calls, call);
            }
        }
    }
    Ok(false)
}

#[derive(Debug, Default)]
struct ChatRound {
    assistant_content: String,
    tool_calls: Vec<PendingToolCall>,
}

#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Default)]
struct StreamingToolCall {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn merge_tool_call_delta(tool_calls: &mut Vec<StreamingToolCall>, call: &Value) {
    let index = call
        .get("index")
        .and_then(Value::as_u64)
        .map(|index| index as usize)
        .unwrap_or(tool_calls.len());

    let position = tool_calls
        .iter()
        .position(|existing| existing.index == index)
        .unwrap_or_else(|| {
            tool_calls.push(StreamingToolCall {
                index,
                ..StreamingToolCall::default()
            });
            tool_calls.len() - 1
        });
    let pending = &mut tool_calls[position];

    if let Some(id) = call.get("id").and_then(Value::as_str) {
        if !id.is_empty() {
            pending.id = Some(id.to_string());
        }
    }
    if let Some(function) = call.get("function") {
        if let Some(name) = function.get("name").and_then(Value::as_str) {
            if !name.is_empty() {
                let current = pending.name.get_or_insert_with(String::new);
                current.push_str(name);
            }
        }
        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
            pending.arguments.push_str(arguments);
        }
    }
}

fn finish_tool_calls(tool_calls: Vec<StreamingToolCall>) -> Vec<PendingToolCall> {
    tool_calls
        .into_iter()
        .filter_map(|call| {
            let name = call.name?;
            if name.trim().is_empty() {
                return None;
            }
            Some(PendingToolCall {
                id: call
                    .id
                    .unwrap_or_else(|| format!("openrouter_tool_call_{}", call.index)),
                name,
                arguments: call.arguments,
            })
        })
        .collect()
}

fn assistant_tool_call_message(tool_calls: &[PendingToolCall], assistant_content: &str) -> Value {
    let content = if assistant_content.trim().is_empty() {
        Value::Null
    } else {
        Value::String(assistant_content.to_string())
    };

    json!({
        "role": "assistant",
        "content": content,
        "tool_calls": tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments,
                    },
                })
            })
            .collect::<Vec<_>>(),
    })
}

fn tool_result_message(tool_result: &ToolCallResponse) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": tool_result.tool_call_id,
        "content": tool_result.result.content,
    })
}

fn chat_completions_tool_schema(tool: &ToolDescriptor) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters.clone(),
            "strict": tool.strict,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mothership_adapter_sdk::protocol::ReasoningEffort;

    #[test]
    fn assistant_tool_call_message_preserves_visible_progress_text() {
        let message = assistant_tool_call_message(
            &[PendingToolCall {
                id: "call_1".to_string(),
                name: "run_command".to_string(),
                arguments: "{\"program\":\"git\"}".to_string(),
            }],
            "I will inspect the repository first.",
        );

        assert_eq!(message["role"], "assistant");
        assert_eq!(message["content"], "I will inspect the repository first.");
        assert_eq!(message["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn assistant_tool_call_message_uses_null_content_without_progress_text() {
        let message = assistant_tool_call_message(
            &[PendingToolCall {
                id: "call_1".to_string(),
                name: "run_command".to_string(),
                arguments: "{}".to_string(),
            }],
            "",
        );

        assert!(message["content"].is_null());
    }

    #[test]
    fn openrouter_reasoning_body_maps_effort_and_budget() {
        let body = openrouter_reasoning_value(&ReasoningConfig {
            effort: Some(ReasoningEffort::Low),
            budget_tokens: Some(2048),
            summary: None,
        })
        .expect("reasoning body");

        assert_eq!(body["effort"], "low");
        assert_eq!(body["max_tokens"], 2048);
    }

    #[test]
    fn openrouter_chat_body_includes_fast_service_tier_without_provider_sort() {
        let body = openrouter_chat_body("openai/gpt-5.5", &[], &[], None, Some("priority"));

        assert_eq!(body["service_tier"], "priority");
        assert!(body.get("provider").is_none());
    }
}
