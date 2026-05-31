//! Bridges a subprocess adapter to the core's chat-completion interface.
//!
//! [`SubprocessChatGateway`] implements [`LlmChatCompletionGateway`] by running
//! one chat turn against a provider adapter obtained from the shared
//! [`AdapterPool`] — a resident process when free, an ephemeral one when busy.
//! This is how the core chats through any process-based provider (a normal HTTP
//! adapter or one that drives an external CLI) without knowing which it is.

use std::sync::Arc;

use mothership_adapter_host::protocol::{ChatMessage, ToolCallResult};
use mothership_adapter_host::{AdapterEntry, ToolCallHandler, ToolCallRequest};

use crate::adapter_pool::AdapterPool;
use crate::auth::FileCredentialVault;
use crate::llm::{
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest, LlmChatRole,
    LlmToolCallHandler, LlmToolCallRequest, LlmTransportKind,
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

        // The uniform chat shape: optional leading system message, then the
        // conversation. The adapter maps this to whatever its provider expects.
        let mut messages = Vec::new();
        if !request.system_prompt.trim().is_empty() {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: request.system_prompt,
            });
        }
        for message in request.messages {
            let role = match message.role {
                LlmChatRole::User => "user",
                LlmChatRole::Assistant => "assistant",
            };
            messages.push(ChatMessage {
                role: role.to_string(),
                content: message.content,
            });
        }

        let model_id = request.model_id;
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
                        messages,
                        move || cancellation.is_cancelled(),
                        |delta| sink.delta(delta),
                        tool_handler,
                    )
                } else {
                    adapter.chat_cancellable(
                        &model_id,
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
        let result = self.inner.handle_tool_call(
            LlmToolCallRequest {
                run_id: self.run_id.clone(),
                tool_call_id: request.tool_call_id,
                name: request.name,
                arguments: request.arguments,
            },
            &self.cancellation,
        );
        ToolCallResult {
            ok: result.ok,
            content: result.content,
        }
    }
}
