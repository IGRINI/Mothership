//! Bridges a subprocess adapter to the core's chat-completion interface.
//!
//! [`SubprocessChatGateway`] implements [`LlmChatRoundGateway`] by running
//! one provider model round against an adapter obtained from the shared
//! [`AdapterPool`] — a resident process when free, an ephemeral one when busy.
//! This is how the core chats through any process-based provider (a normal HTTP
//! adapter or one that drives an external CLI) without knowing which it is.

use std::sync::Arc;

use mothership_adapter_host::protocol::{ChatMessage, ToolCallResponse, ToolCallResult};
use mothership_adapter_host::AdapterEntry;

use crate::adapter_pool::AdapterPool;
use crate::auth::FileCredentialVault;
use crate::llm::{
    LlmChatCompletionEventSink, LlmChatRole, LlmChatRound, LlmChatRoundGateway,
    LlmChatRoundRequest, LlmToolCallRequest, LlmTransportKind,
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
}

impl SubprocessChatGateway {
    pub fn new(pool: Arc<AdapterPool>, entry: AdapterEntry, vault: FileCredentialVault) -> Self {
        Self {
            pool,
            entry,
            vault,
            run_id: None,
        }
    }

    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }
}

impl LlmChatRoundGateway for SubprocessChatGateway {
    fn complete_round(
        &self,
        request: LlmChatRoundRequest,
        cancellation: &ChatCancellationToken,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<LlmChatRound> {
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
        let extra_messages = request
            .extra_messages
            .into_iter()
            .map(chat_message_from_llm)
            .collect::<Vec<_>>();
        let tool_results = request
            .tool_results
            .into_iter()
            .map(|result| ToolCallResponse {
                tool_call_id: result.tool_call_id,
                result: ToolCallResult {
                    ok: result.result.ok,
                    content: model_facing_tool_content(result.result),
                },
            })
            .collect::<Vec<_>>();

        let model_id = request.model_id;
        let reasoning = request.reasoning;
        let prompt = request.prompt;
        let runtime_context = request.runtime_context;
        let tools = request.tools;
        let state = request.state;
        let cancellation = cancellation.clone();
        let run_id = self.run_id.clone();
        self.pool
            .with(&self.entry, &self.vault, |adapter| {
                adapter.chat_round_cancellable(
                    &model_id,
                    reasoning,
                    prompt,
                    runtime_context,
                    messages,
                    tools,
                    state,
                    tool_results,
                    extra_messages,
                    move || cancellation.is_cancelled(),
                    |delta| sink.delta(delta),
                )
            })
            .map(|round| LlmChatRound {
                text: round.text,
                state: round.state,
                tool_calls: round
                    .tool_calls
                    .into_iter()
                    .map(|call| LlmToolCallRequest {
                        run_id: run_id.clone(),
                        tool_call_id: call.tool_call_id,
                        name: call.name,
                        arguments: call.arguments,
                    })
                    .collect(),
            })
            .map_err(|error| {
                MothershipError::InvalidRequest(format!("adapter chat failed: {error}"))
            })
    }
}

fn chat_message_from_llm(message: crate::LlmChatMessage) -> ChatMessage {
    let role = match message.role {
        LlmChatRole::User => "user",
        LlmChatRole::Assistant => "assistant",
    };
    ChatMessage {
        role: role.to_string(),
        content: message.content,
    }
}

fn model_facing_tool_content(result: crate::LlmToolCallResult) -> String {
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
