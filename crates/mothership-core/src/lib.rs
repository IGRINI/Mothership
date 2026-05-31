pub mod adapter_pool;
pub mod auth;
pub mod chat;
pub mod connectors;
pub mod database;
pub mod error;
mod id;
pub mod ipc;
pub mod llm;
pub mod model;
pub mod provider_runtime;
pub mod run;
pub mod subprocess_gateway;

pub use adapter_pool::AdapterPool;
pub use chat::{
    ChatCancellationToken, ChatConversation, ChatMessage, ChatMessageRole, ChatMessageStatus,
    ChatRunCancellationResult, ChatRunContext, ChatRunEvent, ChatRunEventKind, ChatRunEventSink,
    ChatThreadSummary, NoopChatRunEventSink, SendChatMessageResult,
};
pub use connectors::{
    trusted_built_in_adapter_sha256, AdapterSettingPatchValue, AuthProcessRegistry,
    ConnectorManager, ConnectorProviderSummary, ConnectorRefreshStatus, ConnectorService,
    ConnectorSettingsEvent, ConnectorSettingsEventKind, ConnectorSettingsSnapshot,
};
pub use database::Database;
pub use error::{MothershipError, Result};
pub use llm::{
    ConnectorModelManagementKind, ConnectorModelManagementSchema, ConnectorSettingsSchema,
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest, LlmChatMessage,
    LlmChatRole, LlmModel, LlmTransportKind, SelectedLlmModel,
};
pub use model::{ActivityEvent, DashboardMetric, DashboardSnapshot, SidecarStatus, WorkspaceItem};
pub use provider_runtime::ProviderRuntimeManager;
pub use run::{schedule_cancel_fallback, ChatRunRegistry, ChatRunService};
pub use subprocess_gateway::SubprocessChatGateway;
