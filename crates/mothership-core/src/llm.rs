//! Provider-agnostic LLM types.
//!
//! The core knows nothing about any specific provider. Providers are subprocess
//! adapters (see `mothership-adapter-host` + the `adapters/` crates) that own
//! their own transport, auth, and model catalog. This module only defines the
//! generic value types that cross the core↔adapter boundary plus the two traits
//! the chat path is built on. Model listing and chat both flow through adapters
//! (see [`crate::SubprocessChatGateway`]); the core holds no provider code.

use serde::{Deserialize, Serialize};

use crate::ChatCancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmModel {
    pub provider_id: String,
    pub provider_label: String,
    pub id: String,
    pub label: String,
    pub family: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub recommended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SelectedLlmModel {
    pub provider_id: String,
    pub model_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorSettingsSchema {
    pub model_management: ConnectorModelManagementSchema,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorModelManagementSchema {
    pub kind: ConnectorModelManagementKind,
    pub title: String,
    pub description: String,
    pub add_model_label: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorModelManagementKind {
    FixedCatalog,
    RemoteCatalog,
    EditableList,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmChatCompletionRequest {
    pub provider_id: String,
    pub model_id: String,
    pub system_prompt: String,
    pub messages: Vec<LlmChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmChatMessage {
    pub role: LlmChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LlmChatRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LlmTransportKind {
    WebSocket,
    HttpSse,
    HttpJson,
    Subprocess,
}

impl LlmTransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::HttpSse => "http_sse",
            Self::HttpJson => "http_json",
            Self::Subprocess => "subprocess",
        }
    }
}

pub trait LlmChatCompletionEventSink {
    fn transport_selected(&mut self, transport: LlmTransportKind);

    fn delta(&mut self, delta: &str);
}

pub trait LlmChatCompletionGateway: Send + Sync {
    fn complete_chat(
        &self,
        request: LlmChatCompletionRequest,
        cancellation: &ChatCancellationToken,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<String, crate::MothershipError>;
}
