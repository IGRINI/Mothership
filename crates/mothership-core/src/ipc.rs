//! Wire protocol between the desktop host (thin client) and the long-running
//! sidecar that owns Core: newline-delimited JSON, one frame per line.
//!
//! The relationship is multiplexed and long-lived — many requests and several
//! event streams share one stdio pipe — so unlike the single-conversation
//! adapter protocol it keeps three things distinct:
//!
//! - **Response** — exactly one per request `id`, terminal (success or [`CoreError`]).
//! - **Event** — zero or more streamed updates, carrying their own domain
//!   identity (chat id / run id), not the transport request id.
//! - **Notification** — server-initiated, no id (e.g. shutting down).
//!
//! Handshake: the sidecar emits [`ServerFrame::Hello`]; the host replies with
//! [`ClientFrame::Initialize`]; the sidecar opens the database, runs migrations,
//! then emits [`ServerFrame::Ready`]. The host sends no ordinary request before
//! `Ready`, and refuses a mismatched [`PROTOCOL_VERSION`] major.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::connectors::{
    AdapterSettingPatchValue, ConnectorSettingsEvent, ConnectorSettingsSnapshot,
};
use crate::{
    ChatConversation, ChatRunCancellationResult, ChatRunEvent, ChatThreadSummary, ChatUpdatedEvent,
    DashboardSnapshot, MothershipError, ProjectSnapshot, ReasoningConfig, SendChatMessageResult,
    SidecarStatus, ToolApprovalAnswer, ToolExecutionAccepted, ToolExecutionCancellationResult,
    ToolExecutionEvent, ToolExecutionRequest,
};

/// Bump the major when a change isn't backward compatible. The host refuses a
/// sidecar whose version disagrees, rather than mis-parsing frames.
pub const PROTOCOL_VERSION: u32 = 1;

/// Host -> sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Sent once after `Hello`: where the database (and its sibling
    /// `plugins/` + `auth/` dirs) live. The sidecar opens it and replies `Ready`.
    Initialize { db_path: PathBuf },
    /// A correlated request. `id` is transport-local and only used to match the
    /// single `Response`/`Error`; streaming output is correlated by domain id.
    Request { id: u64, request: CoreRequest },
    /// Ask the sidecar to wind down (best effort; the host also hard-kills via
    /// the OS as a fallback).
    Shutdown,
}

/// Sidecar -> host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerFrame {
    /// First frame the sidecar ever writes; advertises its protocol version.
    Hello {
        protocol_version: u32,
        sidecar_version: String,
    },
    /// The database is open and migrated; ordinary requests may now be sent.
    Ready,
    /// Terminal success for request `id`.
    Response { id: u64, result: CoreResponse },
    /// Terminal failure for request `id`.
    Error { id: u64, error: CoreError },
    /// A streamed, domain-correlated update (not tied to a request id).
    Event { event: CoreEvent },
    /// Server-initiated, id-less.
    Notification { notification: Notification },
    /// Forward-compat: an older host parses an unknown future frame as this and
    /// ignores it, rather than failing the whole stream.
    #[serde(other)]
    Unknown,
}

/// The operations the host can ask Core to perform. Flat and domain-named;
/// adding a future op (e.g. a run/tool call) is purely additive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CoreRequest {
    DashboardSnapshot,
    AppendActivityEvent {
        message: String,
    },
    ListChats {
        project_id: Option<String>,
        limit: Option<i64>,
    },
    CreateChat {
        project_id: String,
    },
    GetChat {
        chat_id: String,
        limit: Option<i64>,
    },
    SendChatMessage {
        chat_id: Option<String>,
        project_id: Option<String>,
        content: String,
        #[serde(default)]
        reasoning: Option<ReasoningConfig>,
    },
    /// Edit an existing user message, remove everything after it in that chat,
    /// and start a fresh assistant run from the edited prompt.
    EditChatUserMessage {
        chat_id: String,
        message_id: String,
        content: String,
    },
    /// Create a new chat by copying the visible conversation history through
    /// the selected assistant message.
    BranchChatFromMessage {
        chat_id: String,
        message_id: String,
    },
    /// Re-run the last failed assistant message in a chat without deleting the
    /// failed partial answer. Like `SendChatMessage`, it streams `Event::ChatRun`s.
    RetryChatMessage {
        chat_id: String,
    },
    /// Continue from the last failed assistant message by appending an explicit
    /// continuation prompt and starting a fresh assistant run.
    ContinueChatMessage {
        chat_id: String,
    },
    CancelChatRun {
        run_id: String,
    },
    RunToolCommand {
        request: ToolExecutionRequest,
    },
    ApproveToolExecution {
        tool_call_id: String,
        approved: bool,
        reason: Option<String>,
    },
    CancelToolExecution {
        tool_call_id: String,
    },
    ConnectorSettings,
    SetSelectedModel {
        provider_id: String,
        model_id: String,
    },
    SetChatModel {
        chat_id: String,
        provider_id: String,
        model_id: String,
    },
    SaveAdapterSettings {
        provider_id: String,
        values: BTreeMap<String, AdapterSettingPatchValue>,
    },
    /// Run the adapter's own auth flow (e.g. browser OAuth). Long-running: the
    /// single `Response` lands when the flow finishes or is cancelled.
    Authenticate {
        provider_id: String,
    },
    /// Cancel an in-flight `Authenticate` for this provider (its domain id).
    /// Idempotent — unknown/finished providers are a no-op.
    CancelAuthenticate {
        provider_id: String,
    },
    Logout {
        provider_id: String,
    },
    ListProjects,
    OpenProject {
        path: String,
    },
    SetActiveProject {
        project_id: String,
    },
    SidecarStatus,
}

/// Terminal success payloads, one per [`CoreRequest`] shape.
// Adjacently tagged (`{"ok": "...", "data": ...}`) rather than internally
// tagged: an internally-tagged enum cannot serialize a newtype variant whose
// payload is a sequence (e.g. `ChatList(Vec<_>)`) — there's nowhere to put the
// tag on a JSON array. Adjacent tagging handles every variant shape.
// These are wire DTOs exchanged at human-interaction rates and consumed once per
// request; the size spread between variants doesn't justify boxing each payload.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ok", content = "data", rename_all = "snake_case")]
pub enum CoreResponse {
    Dashboard(DashboardSnapshot),
    ChatList(Vec<ChatThreadSummary>),
    Chat(ChatConversation),
    /// Updated chat summary returned by `set_chat_model`.
    ChatSummary(ChatThreadSummary),
    /// The synchronous half of sending a message: the persisted user + assistant
    /// placeholder. The streamed completion follows as `Event::ChatRun`s.
    ChatMessageStarted(SendChatMessageResult),
    ChatRunCancellation(ChatRunCancellationResult),
    ToolExecutionAccepted(ToolExecutionAccepted),
    ToolApproval(ToolApprovalAnswer),
    ToolExecutionCancellation(ToolExecutionCancellationResult),
    ConnectorSettings(ConnectorSettingsSnapshot),
    ProjectSnapshot(ProjectSnapshot),
    SidecarStatus(SidecarStatus),
}

/// Streamed, domain-correlated updates. Each variant carries its own identity
/// (e.g. `ChatRunEvent` has chat/run/message ids), so the host routes by domain
/// scope, never by transport request id.
// The size spread is inherent to having a zero-size `Unknown` fallback next to a
// real payload; boxing the payload to satisfy the lint buys nothing here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CoreEvent {
    ChatRun(ChatRunEvent),
    ToolExecution(ToolExecutionEvent),
    ConnectorSettings(ConnectorSettingsEvent),
    ChatUpdated(ChatUpdatedEvent),
    #[serde(other)]
    Unknown,
}

/// Server-initiated, id-less messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "notification", rename_all = "snake_case")]
pub enum Notification {
    ShuttingDown,
    #[serde(other)]
    Unknown,
}

/// A typed error for a failed request. `retryable` tells the host whether
/// re-issuing might succeed (e.g. after a sidecar restart).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl CoreError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }

    /// The sidecar went away (crash / EOF) with this request still pending.
    /// Retryable once a fresh sidecar is up.
    pub fn sidecar_lost() -> Self {
        Self::new(
            "sidecar_lost",
            "the core sidecar is unavailable; retry shortly",
            true,
        )
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl From<MothershipError> for CoreError {
    fn from(error: MothershipError) -> Self {
        let code = match &error {
            MothershipError::InvalidRequest(_) => "invalid_request",
            _ => "internal",
        };
        Self::new(code, error.to_string(), false)
    }
}
