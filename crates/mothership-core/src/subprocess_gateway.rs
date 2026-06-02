//! Bridges a subprocess adapter to the core's chat-completion interface.
//!
//! [`SubprocessChatGateway`] implements [`LlmChatRoundGateway`] by running
//! one provider model round against an adapter obtained from the shared
//! [`AdapterPool`] — a resident process when free, an ephemeral one when busy.
//! This is how the core chats through any process-based provider (a normal HTTP
//! adapter or one that drives an external CLI) without knowing which it is.

use std::cell::RefCell;
use std::sync::Arc;

use mothership_adapter_host::protocol::{ChatMessage, ToolCallResponse, ToolCallResult};
use mothership_adapter_host::AdapterEntry;

use crate::adapter_pool::AdapterPool;
use crate::auth::FileCredentialVault;
use crate::llm::{
    LlmChatCompletionEventSink, LlmChatRole, LlmChatRound, LlmChatRoundGateway,
    LlmChatRoundRequest, LlmToolCallHandler, LlmToolCallRequest, LlmTransportKind,
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

impl LlmChatRoundGateway for SubprocessChatGateway {
    fn complete_round(
        &self,
        request: LlmChatRoundRequest,
        cancellation: &ChatCancellationToken,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<LlmChatRound> {
        sink.transport_selected(LlmTransportKind::Subprocess);
        let sink = RefCell::new(sink);

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
        let cancellation_for_watcher = cancellation.clone();
        let cancellation_for_tools = cancellation.clone();
        let run_id = self.run_id.clone();
        let tool_run_id = run_id.clone();
        let tool_handler = self.tool_handler.clone();
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
                    move || cancellation_for_watcher.is_cancelled(),
                    |call| {
                        sink.borrow_mut().before_tool_call(&call.tool_call_id);
                        execute_mid_turn_tool(
                            tool_handler.as_deref(),
                            tool_run_id.clone(),
                            call,
                            &cancellation_for_tools,
                        )
                    },
                    |delta| sink.borrow_mut().delta(delta),
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

fn execute_mid_turn_tool(
    handler: Option<&dyn LlmToolCallHandler>,
    run_id: Option<String>,
    call: mothership_adapter_host::protocol::ToolCallInvocation,
    cancellation: &ChatCancellationToken,
) -> ToolCallResult {
    let Some(handler) = handler else {
        return ToolCallResult {
            ok: false,
            content: "Mothership Core tool handler is not enabled for this run".to_string(),
        };
    };

    let result = handler.handle_tool_call(
        LlmToolCallRequest {
            run_id,
            tool_call_id: call.tool_call_id,
            name: call.name,
            arguments: call.arguments,
        },
        cancellation,
    );

    ToolCallResult {
        ok: result.ok,
        content: model_facing_tool_content(result),
    }
}
