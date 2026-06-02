//! Bridges a subprocess adapter to the core's chat-completion interface.
//!
//! [`SubprocessChatGateway`] implements [`LlmChatCompletionGateway`] by running
//! one chat turn against a provider adapter obtained from the shared
//! [`AdapterPool`] — a resident process when free, an ephemeral one when busy.
//! This is how the core chats through any process-based provider (a normal HTTP
//! adapter or one that drives an external CLI) without knowing which it is.

use std::sync::Arc;

use mothership_adapter_host::protocol::{ChatMessage, ToolCallResult};
use mothership_adapter_host::{AdapterEntry, ChatAdapterEvent, ToolCallHandler, ToolCallRequest};

use crate::adapter_pool::AdapterPool;
use crate::auth::FileCredentialVault;
use crate::llm::{
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest, LlmChatRole,
    LlmToolCallHandler, LlmToolCallRequest, LlmToolCallResult, LlmTransportKind,
};
use crate::{ChatCancellationToken, MothershipError, Result};

/// Chats with a provider implemented as a subprocess adapter. The adapter
/// process is reused across turns via the [`AdapterPool`]; spawning, the
/// credential-store side channel, `initialize`, and `set_settings` are all
/// handled by the pool. Settings — including secrets — live in the app's shared
/// credential `vault`, keyed by provider.
pub struct SubprocessChatGateway {
    pool: Arc<AdapterPool>,
    entry: AdapterEntry,
    vault: FileCredentialVault,
    run_id: Option<String>,
    tool_handler: Option<Arc<dyn LlmToolCallHandler>>,
}

impl SubprocessChatGateway {
    pub fn new(pool: Arc<AdapterPool>, entry: AdapterEntry, vault: FileCredentialVault) -> Self {
        Self {
            pool,
            entry,
            vault,
            run_id: None,
            tool_handler: None,
        }
    }

    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    pub fn with_tool_handler(mut self, handler: Arc<dyn LlmToolCallHandler>) -> Self {
        self.tool_handler = Some(handler);
        self
    }
}

impl LlmChatCompletionGateway for SubprocessChatGateway {
    fn complete_chat(
        &self,
        request: LlmChatCompletionRequest,
        cancellation: &ChatCancellationToken,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<String> {
        sink.transport_selected(LlmTransportKind::Subprocess);

        // Core owns the runtime prompt and tool catalog. The adapter receives
        // them as structured inputs and maps them to its provider-specific wire
        // format.
        let messages = request
            .messages
            .into_iter()
            .map(|message| {
                let role = match message.role {
                    LlmChatRole::User => "user",
                    LlmChatRole::Assistant => "assistant",
                };
                ChatMessage {
                    role: role.to_string(),
                    content: message.content,
                }
            })
            .collect::<Vec<_>>();

        let model_id = request.model_id;
        let prompt = request.prompt;
        let tools = request.tools;
        let cancellation = cancellation.clone();
        let run_id = self.run_id.clone();
        let tool_handler = self.tool_handler.as_ref().map(|handler| {
            Arc::new(SubprocessToolCallHandler {
                inner: Arc::clone(handler),
                run_id,
                cancellation: cancellation.clone(),
            }) as Arc<dyn ToolCallHandler>
        });
        self.pool
            .with(&self.entry, &self.vault, |adapter| {
                if let Some(tool_handler) = tool_handler {
                    adapter.chat_cancellable_with_tools(
                        &model_id,
                        prompt,
                        messages,
                        tools,
                        move || cancellation.is_cancelled(),
                        |event| match event {
                            ChatAdapterEvent::Delta(delta) => sink.delta(delta),
                            ChatAdapterEvent::ToolCall(tool_call_id) => {
                                sink.before_tool_call(tool_call_id)
                            }
                        },
                        tool_handler,
                    )
                } else {
                    adapter.chat_cancellable_with_prompt(
                        &model_id,
                        prompt,
                        messages,
                        move || cancellation.is_cancelled(),
                        |delta| sink.delta(delta),
                    )
                }
            })
            .map_err(|error| {
                MothershipError::InvalidRequest(format!("adapter chat failed: {error}"))
            })
    }
}

struct SubprocessToolCallHandler {
    inner: Arc<dyn LlmToolCallHandler>,
    run_id: Option<String>,
    cancellation: ChatCancellationToken,
}

impl ToolCallHandler for SubprocessToolCallHandler {
    fn handle_tool_call(&self, request: ToolCallRequest) -> ToolCallResult {
        let result = self
            .inner
            .handle_tool_call(self.llm_tool_call_request(request), &self.cancellation);
        ToolCallResult {
            ok: result.ok,
            content: model_facing_tool_content(result),
        }
    }

    fn handle_tool_calls(&self, requests: Vec<ToolCallRequest>) -> Vec<ToolCallResult> {
        self.inner
            .handle_tool_calls(
                requests
                    .into_iter()
                    .map(|request| self.llm_tool_call_request(request))
                    .collect(),
                &self.cancellation,
            )
            .into_iter()
            .map(|result| ToolCallResult {
                ok: result.ok,
                content: model_facing_tool_content(result),
            })
            .collect()
    }
}

impl SubprocessToolCallHandler {
    fn llm_tool_call_request(&self, request: ToolCallRequest) -> LlmToolCallRequest {
        LlmToolCallRequest {
            run_id: self.run_id.clone(),
            tool_call_id: request.tool_call_id,
            name: request.name,
            arguments: request.arguments,
        }
    }
}

fn model_facing_tool_content(result: LlmToolCallResult) -> String {
    if result.ok {
        return result.content;
    }

    let content = result.content.trim();
    if content.is_empty() {
        "Tool call failed.".to_string()
    } else {
        format!("Tool call failed:\n{content}")
    }
}
