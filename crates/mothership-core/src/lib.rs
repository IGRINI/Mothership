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
pub mod project;
pub mod prompt;
pub mod provider_runtime;
pub mod run;
pub mod subprocess_gateway;
pub mod tools;

pub use adapter_pool::AdapterPool;
pub use chat::{
    ChatCancellationToken, ChatConversation, ChatMessage, ChatMessagePart, ChatMessagePartKind,
    ChatMessageRole, ChatMessageStatus, ChatRunCancellationResult, ChatRunContext,
    ChatRunContextSpec, ChatRunEvent, ChatRunEventKind, ChatRunEventSink, ChatThreadSummary,
    NoopChatRunEventSink, SendChatMessageResult,
};
pub use connectors::{
    trusted_built_in_adapter_sha256, AdapterSettingPatchValue, AuthProcessRegistry,
    ConnectorManager, ConnectorProviderSummary, ConnectorRefreshStatus, ConnectorSettingsEvent,
    ConnectorSettingsEventKind, ConnectorSettingsSnapshot,
};
pub use database::Database;
pub use error::{MothershipError, Result};
pub use llm::{
    ConnectorModelManagementKind, ConnectorModelManagementSchema, ConnectorSettingsSchema,
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest, LlmChatMessage,
    LlmChatRole, LlmModel, LlmToolCallHandler, LlmToolCallRequest, LlmToolCallResult,
    LlmTransportKind, ProviderRequestDraft, ProviderRequestModifier, ProviderRequestPipeline,
    SelectedLlmModel,
};
pub use model::{ActivityEvent, DashboardMetric, DashboardSnapshot, SidecarStatus, WorkspaceItem};
pub use project::{ProjectSnapshot, ProjectSummary};
pub use provider_runtime::{ProviderRuntimeHealth, ProviderRuntimeManager, ProviderRuntimeStatus};
pub use run::{schedule_cancel_fallback, ChatRunRegistry, ChatRunService};
pub use subprocess_gateway::SubprocessChatGateway;
pub use tools::{
    tool_batch_plan, ConservativeCommandPermissionPolicy, FileToolOutputStore,
    NoopToolExecutionEventSink, PendingToolApprovalGate, SpawnedToolProcess,
    StaticToolApprovalGate, ToolApprovalAnswer, ToolApprovalDecision, ToolApprovalGate,
    ToolBatchPlan, ToolCancellationToken, ToolCommand, ToolConcurrency, ToolExecutionAccepted,
    ToolExecutionCancellationResult, ToolExecutionEvent, ToolExecutionEventKind,
    ToolExecutionEventSink, ToolExecutionRecord, ToolExecutionRegistry, ToolExecutionRequest,
    ToolExecutionResult, ToolExecutionStatus, ToolOutputPolicy, ToolOutputStore, ToolOutputStream,
    ToolPermissionAction, ToolPermissionEvaluation, ToolPermissionPolicy, ToolProcessExit,
    ToolProcessSandbox, ToolProcessSpec, ToolRepeatBlock, ToolRepeatGuard, ToolRepeatGuardConfig,
    ToolResourceLimits, ToolSupervisor, RUN_COMMAND_TOOL_NAME,
};
