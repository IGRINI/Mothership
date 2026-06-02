use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use serde::{Deserialize, Serialize};

use crate::tools::ToolExecutionRecord;
use mothership_adapter_host::protocol::ReasoningConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatThreadSummary {
    pub id: String,
    #[serde(default)]
    pub project_id: Option<String>,
    pub title: String,
    pub preview: String,
    pub message_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub id: String,
    pub chat_id: String,
    pub position: i64,
    pub role: ChatMessageRole,
    pub content: String,
    pub status: ChatMessageStatus,
    pub created_at: String,
    #[serde(default)]
    pub error: Option<String>,
    /// For assistant messages, the provider/model that produced this reply (so
    /// the UI can attribute it to the right adapter + model). `None` for user
    /// messages and for messages written before attribution was recorded.
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChatMessageRole {
    Assistant,
    User,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChatMessageStatus {
    Complete,
    Cancelled,
    Failed,
    Sending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatConversation {
    pub chat: ChatThreadSummary,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tool_executions: Vec<ToolExecutionRecord>,
    #[serde(default)]
    pub message_parts: Vec<ChatMessagePart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessagePart {
    pub id: i64,
    pub chat_id: String,
    pub message_id: String,
    pub kind: ChatMessagePartKind,
    pub text: Option<String>,
    pub tool_call_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChatMessagePartKind {
    Text,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendChatMessageResult {
    pub run_id: String,
    pub chat: ChatThreadSummary,
    pub user_message: ChatMessage,
    pub assistant_message: ChatMessage,
    #[serde(skip)]
    pub context: ChatRunContextSpec,
}

#[derive(Debug, Clone, Default)]
pub struct ChatRunContextSpec {
    pub include_failed_assistant_message_id: Option<String>,
    pub reasoning: Option<ReasoningConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRunContext {
    pub run_id: String,
    pub chat_id: String,
    pub assistant_message_id: String,
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRunCancellationResult {
    pub run_id: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRunEvent {
    pub run_id: String,
    pub chat_id: String,
    pub message_id: String,
    pub kind: ChatRunEventKind,
    pub delta: Option<String>,
    pub message: Option<ChatMessage>,
    pub chat: Option<ChatThreadSummary>,
    pub transport: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChatRunEventKind {
    Started,
    TransportSelected,
    Delta,
    ToolCall,
    Cancelled,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Default)]
pub struct ChatCancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl ChatCancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

pub trait ChatRunEventSink: Send {
    fn emit(&mut self, event: ChatRunEvent);
}

pub struct NoopChatRunEventSink;

impl ChatRunEventSink for NoopChatRunEventSink {
    fn emit(&mut self, _event: ChatRunEvent) {}
}
