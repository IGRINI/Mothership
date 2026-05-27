//! Bridges a subprocess adapter to the core's chat-completion interface.
//!
//! [`SubprocessChatGateway`] implements [`LlmChatCompletionGateway`] by spawning
//! an adapter process (see `mothership-adapter-host`) and streaming its reply.
//! This is how the core chats through any process-based provider — a normal HTTP
//! adapter or one that drives an external CLI — without knowing which it is.

use std::path::PathBuf;

use mothership_adapter_host::protocol::ChatMessage;
use mothership_adapter_host::Adapter;

use crate::auth::FileCredentialVault;
use crate::llm::{
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest, LlmChatRole,
    LlmTransportKind,
};
use crate::{MothershipError, Result};

/// Chats with a provider implemented as a subprocess adapter. Each call spawns a
/// fresh adapter process, runs one turn, and drops it (the process is killed on
/// drop). Pooling / resident instances can come later if startup cost matters.
///
/// Settings — including secrets like API keys and OAuth tokens — live in the
/// app's shared credential `vault`, keyed by `provider_id`. The host loads them
/// and pushes them to the adapter via `set_settings` on each spawn, and persists
/// anything the adapter hands back (an OAuth token it minted/refreshed) into the
/// same shared vault. Nothing secret is stored next to the adapter on disk.
pub struct SubprocessChatGateway {
    program: PathBuf,
    provider_id: String,
    vault: FileCredentialVault,
}

impl SubprocessChatGateway {
    pub fn new(
        program: impl Into<PathBuf>,
        provider_id: impl Into<String>,
        vault: FileCredentialVault,
    ) -> Self {
        Self {
            program: program.into(),
            provider_id: provider_id.into(),
            vault,
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

        // Persist anything the adapter pushes back (e.g. an OAuth token it just
        // minted or refreshed) into the SAME shared vault, merged so the keys it
        // didn't send survive. Wired before any exchange so it can fire anytime.
        let vault = self.vault.clone();
        let provider_id = self.provider_id.clone();
        adapter.set_store_secret_handler(move |values| {
            if let Err(error) = vault.merge_adapter_settings(&provider_id, values) {
                eprintln!("failed to persist adapter secret for {provider_id}: {error}");
            }
        });

        adapter.initialize().map_err(|error| {
            MothershipError::InvalidRequest(format!("adapter initialize failed: {error}"))
        })?;

        let settings = self
            .vault
            .load_adapter_settings(&self.provider_id)
            .map_err(|error| {
                MothershipError::InvalidRequest(format!("load adapter settings failed: {error}"))
            })?;
        if !settings.is_empty() {
            adapter.set_settings(settings).map_err(|error| {
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
