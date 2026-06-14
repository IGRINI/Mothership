use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{llm::LlmChatMessage, tools::ToolExecutionRecord};
use mothership_adapter_host::protocol::ReasoningConfig;

// `optional_fields`: every `#[serde(default)] Option<_>` field deserializes fine
// when absent, and the thin clients (incl. the preview mocks) build partial
// summaries — so emit them as `field?: T | null`, matching the wire's
// deserialize tolerance. TS-only; no serde/runtime change.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, optional_fields = nullable)]
pub struct ChatThreadSummary {
    pub id: String,
    #[serde(default)]
    pub project_id: Option<String>,
    pub title: String,
    pub preview: String,
    pub message_count: i64,
    /// Execution model for THIS chat's future runs. `None` => use the global
    /// default-for-new-chats. Distinct from `ChatMessage.provider_id/model_id`,
    /// which is the immutable attribution of an already-produced answer.
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    /// Per-chat tool approval mode (`manual`/`auto_safe`/`yolo`). `None` => the
    /// runtime default (manual). Seeded into the runtime store when a run starts.
    #[serde(default)]
    pub approval_mode: Option<String>,
    /// Per-chat reasoning option id for future runs. `None` => the model's
    /// recommended default.
    #[serde(default)]
    pub reasoning: Option<String>,
    /// Per-chat fast-mode preference for future runs. `None`/`false` => standard
    /// provider speed/cost behavior.
    #[serde(default)]
    pub fast_mode: Option<bool>,
    /// The chat's unsent composer text, so it survives reopening. `None`/empty =>
    /// nothing typed yet.
    #[serde(default)]
    pub draft: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Pushed when a chat's metadata changes out of band (e.g. its model was set,
/// possibly on another client). Wire event `chat_updated`; the host forwards it
/// to the webview as `chat-updated`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChatUpdatedEvent {
    pub chat: ChatThreadSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, optional_fields = nullable)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ChatMessageRole {
    Assistant,
    User,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ChatMessageStatus {
    Complete,
    Cancelled,
    Failed,
    Sending,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChatConversation {
    pub chat: ChatThreadSummary,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tool_executions: Vec<ToolExecutionRecord>,
    #[serde(default)]
    pub message_parts: Vec<ChatMessagePart>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChatMessagePart {
    pub id: i64,
    pub chat_id: String,
    pub message_id: String,
    pub kind: ChatMessagePartKind,
    pub text: Option<String>,
    pub tool_call_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ChatMessagePartKind {
    Text,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SendChatMessageResult {
    pub run_id: String,
    pub chat: ChatThreadSummary,
    pub user_message: ChatMessage,
    pub assistant_message: ChatMessage,
    /// Messages removed from the conversation before this run was created.
    ///
    /// Send/continue normally leave this empty. Edit/retry use it so thin
    /// clients can reconcile local chat state before applying stream events.
    #[serde(default)]
    pub removed_message_ids: Vec<String>,
    #[serde(skip)]
    pub context: ChatRunContextSpec,
}

#[derive(Debug, Clone, Default)]
pub struct ChatRunContextSpec {
    pub include_failed_assistant_message_id: Option<String>,
    pub reasoning: Option<ReasoningConfig>,
    pub fast_mode: bool,
    pub runtime_messages: Vec<LlmChatMessage>,
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

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChatRunCancellationResult {
    pub run_id: String,
    pub accepted: bool,
}

/// A chat run currently executing, as reported by `list_active_runs`. Carries
/// the display fields (chat title, project name) resolved at registration so a
/// freshly-connected client can render agent activity without extra lookups.
/// One entry per top-level run — provider-internal subagents are never listed.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, optional_fields = nullable)]
pub struct ActiveRunSummary {
    pub run_id: String,
    pub chat_id: String,
    pub chat_title: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub project_name: Option<String>,
    pub message_id: String,
    pub provider_id: String,
    pub model_id: String,
    /// Unix epoch milliseconds when the run registered (provider resolved).
    pub started_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
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
    #[serde(default)]
    pub removed_message_ids: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
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
