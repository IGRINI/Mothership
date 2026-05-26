pub mod auth;
pub mod chat;
pub mod database;
pub mod error;
mod id;
pub mod llm;
pub mod model;

pub use chat::{
    ChatConversation, ChatMessage, ChatMessageRole, ChatMessageStatus, ChatRunContext,
    ChatRunEvent, ChatRunEventKind, ChatRunEventSink, ChatThreadSummary, NoopChatRunEventSink,
    SendChatMessageResult,
};
pub use database::Database;
pub use error::{MothershipError, Result};
pub use llm::{
    chat_system_prompt, connector_settings_schema, default_llm_model, find_llm_model,
    ConnectorModelManagementKind, ConnectorModelManagementSchema, ConnectorSettingsSchema,
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest, LlmChatMessage,
    LlmChatRole, LlmModel, LlmModelCatalogCache, LlmModelCatalogRepository, LlmModelCatalogService,
    LlmTransportKind, OpenAiCodexChatCompletionGateway, SelectedLlmModel,
    StaticLlmConnectorRegistry,
};
pub use model::{ActivityEvent, DashboardMetric, DashboardSnapshot, SidecarStatus, WorkspaceItem};
