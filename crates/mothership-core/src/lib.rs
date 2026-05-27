pub mod auth;
pub mod chat;
pub mod connectors;
pub mod database;
pub mod error;
mod id;
pub mod ipc;
pub mod llm;
pub mod model;
pub mod run;
pub mod subprocess_gateway;

pub use chat::{
    ChatConversation, ChatMessage, ChatMessageRole, ChatMessageStatus, ChatRunContext,
    ChatRunEvent, ChatRunEventKind, ChatRunEventSink, ChatThreadSummary, NoopChatRunEventSink,
    SendChatMessageResult,
};
pub use connectors::{
    AuthProcessRegistry, ConnectorProviderSummary, ConnectorService, ConnectorSettingsSnapshot,
};
pub use database::Database;
pub use error::{MothershipError, Result};
pub use llm::{
    ConnectorModelManagementKind, ConnectorModelManagementSchema, ConnectorSettingsSchema,
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest, LlmChatMessage,
    LlmChatRole, LlmModel, LlmTransportKind, SelectedLlmModel,
};
pub use model::{ActivityEvent, DashboardMetric, DashboardSnapshot, SidecarStatus, WorkspaceItem};
pub use run::ChatRunService;
pub use subprocess_gateway::SubprocessChatGateway;
