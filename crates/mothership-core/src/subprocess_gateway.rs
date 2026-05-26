//! Bridges a subprocess adapter to the core's chat-completion interface.
//!
//! [`SubprocessChatGateway`] implements [`LlmChatCompletionGateway`] by spawning
//! an adapter process (see `mothership-adapter-host`) and streaming its reply.
//! This is how the core chats through any process-based provider — a normal HTTP
//! adapter or one that drives an external CLI — without knowing which it is.

use std::collections::BTreeMap;
use std::path::PathBuf;

use mothership_adapter_host::protocol::ChatMessage;
use mothership_adapter_host::Adapter;

use crate::llm::{
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest, LlmChatRole,
    LlmTransportKind,
};
use crate::{MothershipError, Result};

/// Chats with a provider implemented as a subprocess adapter. Each call spawns a
/// fresh adapter process, runs one turn, and drops it (the process is killed on
/// drop). Pooling / resident instances can come later if startup cost matters.
pub struct SubprocessChatGateway {
    program: PathBuf,
    settings: BTreeMap<String, String>,
}

impl SubprocessChatGateway {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            settings: BTreeMap::new(),
        }
    }

    /// Same, but with the settings the host pushes to the adapter (api key, base
    /// url, user model list, …) right after initialization.
    pub fn with_settings(program: impl Into<PathBuf>, settings: BTreeMap<String, String>) -> Self {
        Self {
            program: program.into(),
            settings,
        }
    }
}

impl LlmChatCompletionGateway for SubprocessChatGateway {
    fn complete_chat(
        &self,
        request: LlmChatCompletionRequest,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<String> {
        let mut adapter = Adapter::spawn(&self.program).map_err(|error| {
            MothershipError::InvalidRequest(format!("failed to start adapter: {error}"))
        })?;
        adapter.initialize().map_err(|error| {
            MothershipError::InvalidRequest(format!("adapter initialize failed: {error}"))
        })?;
        if !self.settings.is_empty() {
            adapter.set_settings(self.settings.clone()).map_err(|error| {
                MothershipError::InvalidRequest(format!("adapter set_settings failed: {error}"))
            })?;
        }

        sink.transport_selected(LlmTransportKind::Subprocess);

        // The uniform chat shape: system prompt (if any) as a leading system
        // message, then the conversation. The adapter maps this to whatever its
        // provider expects.
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

        adapter
            .chat(&request.model_id, messages, |delta| sink.delta(delta))
            .map_err(|error| {
                MothershipError::InvalidRequest(format!("adapter chat failed: {error}"))
            })
    }
}
