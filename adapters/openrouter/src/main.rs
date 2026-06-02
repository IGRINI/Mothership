//! OpenRouter provider adapter.
//!
//! Provider-specific logic only: settings, user-defined model list, and the
//! OpenAI-compatible streaming HTTP call. The shared adapter SDK owns stdio
//! framing, request dispatch, and cooperative chat cancellation.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context as _;
use mothership_adapter_sdk::agentic::AgenticTurnPolicy;
use mothership_adapter_sdk::http;
use mothership_adapter_sdk::protocol::{
    AuthKind, AuthStatus, Model, ModelManagement, SettingsField, SettingsFieldKind, ToolDescriptor,
};
use mothership_adapter_sdk::sse;
use mothership_adapter_sdk::tools::{dispatch_tool_calls, ProviderToolCall, ProviderToolResult};
use mothership_adapter_sdk::{ChatRequest, Context, ProviderAdapter};
use serde_json::{json, Value};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_SSE_LINE_BYTES: usize = 256 * 1024;
const MAX_ERROR_BODY_CHARS: usize = 300;
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct OpenRouterSettings {
    api_key: String,
    base_url: String,
    models: String,
}

struct OpenRouterAdapter {
    client: reqwest::Client,
    settings: OpenRouterSettings,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    mothership_adapter_sdk::run(OpenRouterAdapter {
        client: http_client()?,
        settings: OpenRouterSettings::default(),
    })
    .await
}

#[async_trait::async_trait]
impl ProviderAdapter for OpenRouterAdapter {
    fn identity(&self) -> (String, String) {
        ("openrouter".to_string(), "OpenRouter".to_string())
    }

    fn settings_schema(&self) -> Vec<SettingsField> {
        vec![
            SettingsField {
                key: "api_key".to_string(),
                label: "OpenRouter API key".to_string(),
                kind: SettingsFieldKind::Secret,
                required: true,
            },
            SettingsField {
                key: "base_url".to_string(),
                label: "Base URL (optional)".to_string(),
                kind: SettingsFieldKind::Text,
                required: false,
            },
            SettingsField {
                key: "models".to_string(),
                label: "Models".to_string(),
                kind: SettingsFieldKind::StringList,
                required: false,
            },
        ]
    }

    fn auth_schema(&self) -> AuthKind {
        AuthKind::ApiKey {
            label: "OpenRouter API key".to_string(),
        }
    }

    fn auth_status(&self) -> AuthStatus {
        if self.settings.api_key.trim().is_empty() {
            AuthStatus::missing("OpenRouter API key is not configured")
        } else {
            AuthStatus::configured("OpenRouter API key is configured")
        }
    }

    async fn set_settings(&mut self, values: BTreeMap<String, String>) -> anyhow::Result<()> {
        self.settings.api_key = values.get("api_key").cloned().unwrap_or_default();
        self.settings.base_url = values.get("base_url").cloned().unwrap_or_default();
        self.settings.models = values.get("models").cloned().unwrap_or_default();
        Ok(())
    }

    async fn models(&mut self, _ctx: &Context) -> anyhow::Result<(ModelManagement, Vec<Model>)> {
        Ok((
            ModelManagement::UserDefined,
            parse_models(&self.settings.models),
        ))
    }

    async fn chat(
        &mut self,
        request: ChatRequest,
        _ctx: &Context,
        sink: &mut mothership_adapter_sdk::ChatSink,
    ) -> anyhow::Result<()> {
        stream_chat(&self.client, &self.settings, &request, sink).await
    }
}

fn parse_models(spec: &str) -> Vec<Model> {
    spec.split(['\n', ','])
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .enumerate()
        .map(|(index, id)| Model {
            id: id.to_string(),
            label: id.to_string(),
            recommended: index == 0,
        })
        .collect()
}

async fn stream_chat(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
    request: &ChatRequest,
    sink: &mut mothership_adapter_sdk::ChatSink,
) -> anyhow::Result<()> {
    if settings.api_key.trim().is_empty() {
        anyhow::bail!("missing OpenRouter API key (set it in adapter settings)");
    }
    let base = if settings.base_url.trim().is_empty() {
        DEFAULT_BASE_URL
    } else {
        settings.base_url.trim()
    };
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
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
    let agentic_policy = AgenticTurnPolicy::default();

    for _ in 0..agentic_policy.max_turns() {
        let round = stream_chat_once(
            client,
            settings,
            &url,
            &request.model,
            &conversation,
            sink,
            &request.tools,
        )
        .await?;
        if round.tool_calls.is_empty() {
            return Ok(());
        }

        let tool_results = dispatch_openrouter_tool_calls(round.tool_calls.clone(), sink).await?;
        if sink.is_cancelled() {
            return Ok(());
        }
        conversation.push(assistant_tool_call_message(
            &round.tool_calls,
            &round.assistant_content,
        ));
        for tool_result in tool_results {
            conversation.push(tool_result_message(tool_result));
        }
    }

    conversation.push(json!({
        "role": "user",
        "content": agentic_policy.final_synthesis_prompt(),
    }));
    match stream_chat_once(
        client,
        settings,
        &url,
        &request.model,
        &conversation,
        sink,
        &[],
    )
    .await
    {
        Ok(round) if !round.tool_calls.is_empty() => {
            sink.delta(agentic_policy.fallback_message());
            eprintln!(
                "openrouter-adapter: final no-tool synthesis unexpectedly returned {} tool call(s)",
                round.tool_calls.len()
            );
            Ok(())
        }
        Ok(round) if round.assistant_content.trim().is_empty() => {
            sink.delta(agentic_policy.fallback_message());
            Ok(())
        }
        Ok(_round) => Ok(()),
        Err(error) => {
            sink.delta(agentic_policy.fallback_message());
            eprintln!(
                "openrouter-adapter: final no-tool synthesis after {} agentic turns failed: {error:#}",
                agentic_policy.max_turns()
            );
            Ok(())
        }
    }
}

async fn stream_chat_once(
    client: &reqwest::Client,
    settings: &OpenRouterSettings,
    url: &str,
    model: &str,
    messages: &[Value],
    sink: &mut mothership_adapter_sdk::ChatSink,
    tools: &[ToolDescriptor],
) -> anyhow::Result<ChatRound> {
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

    let api_key = settings.api_key.trim();
    let headers = auth_headers(api_key);
    let redacted_values = [api_key];
    let request = http::post_stream_redacted(
        client,
        url,
        &headers,
        &body,
        ERROR_BODY_TIMEOUT,
        MAX_ERROR_BODY_CHARS,
        &redacted_values,
    );
    let response = tokio::select! {
        result = request => result.context("send OpenRouter chat request")?,
        _ = sink.cancelled() => return Ok(ChatRound::default()),
    };

    read_sse_stream(response, sink).await
}

fn auth_headers(api_key: &str) -> Vec<(String, String)> {
    vec![("Authorization".to_string(), format!("Bearer {api_key}"))]
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .context("build OpenRouter HTTP client")
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

async fn dispatch_openrouter_tool_calls(
    tool_calls: Vec<PendingToolCall>,
    sink: &mothership_adapter_sdk::ChatSink,
) -> anyhow::Result<Vec<ProviderToolResult>> {
    dispatch_tool_calls(
        tool_calls.into_iter().map(|call| {
            ProviderToolCall::from_raw_json_arguments(call.id, call.name, &call.arguments)
        }),
        sink,
    )
    .await
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

fn tool_result_message(tool_result: ProviderToolResult) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": tool_result.call.id,
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
}
