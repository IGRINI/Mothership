pub mod adapter_pool;
pub mod agentic;
pub mod auth;
pub mod changes;
pub mod chat;
pub mod connectors;
pub mod database;
pub mod error;
mod id;
pub mod ipc;
pub mod llm;
pub mod model;
pub mod personalization;
pub mod project;
pub mod prompt;
pub mod provider_runtime;
pub mod run;
pub mod subprocess_gateway;
pub mod tools;

pub use adapter_pool::AdapterPool;
pub use agentic::{
    AgenticLoopPolicy, DEFAULT_FALLBACK_MESSAGE, DEFAULT_FINAL_SYNTHESIS_PROMPT,
    DEFAULT_MAX_AGENTIC_ROUNDS,
};
pub use changes::{
    sha256_hex as change_blob_sha256_hex, CaptureFileSystem, ChangeConflict, ChangeContext,
    ChangeEventSink, ChangeFileDiff, ChangeFileSummary, ChangeOp, ChangeRecorder, ChangeSetEvent,
    ChangeSetEventKind, ChangeSetStatus, ChangeSetSummary, ChangesService, ConflictReason,
    FileBlobStore, NoopChangeEventSink, RevertOutcome, RevertStatus, SnapshotBlobStore,
};
pub use chat::{
    ActiveRunSummary, ChatCancellationToken, ChatConversation, ChatMessage, ChatMessagePart,
    ChatMessagePartKind, ChatMessageRole, ChatMessageStatus, ChatRunCancellationResult,
    ChatRunContext, ChatRunContextSpec, ChatRunEvent, ChatRunEventKind, ChatRunEventSink,
    ChatThreadSummary, ChatUpdatedEvent, NoopChatRunEventSink, SendChatMessageResult,
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
    FeatureRoute, LlmChatCompletionEventSink, LlmChatCompletionRequest, LlmChatMessage,
    LlmChatRole, LlmChatRound, LlmChatRoundGateway, LlmChatRoundRequest, LlmModel,
    LlmToolCallHandler, LlmToolCallRequest, LlmToolCallResponse, LlmToolCallResult,
    LlmTransportKind, ProviderRequestDraft, ProviderRequestModifier, ProviderRequestPipeline,
    SelectedLlmModel,
};
pub use model::{ActivityEvent, DashboardMetric, DashboardSnapshot, SidecarStatus, WorkspaceItem};
pub use mothership_adapter_host::protocol::{
    AudioTranscriptionResult, ProviderMediaBlob, ReasoningCapabilities, ReasoningConfig,
    ReasoningEffort, ReasoningOption, ReasoningSummary, ToolDescriptor,
};
pub use personalization::{ModelInstruction, PersonalizationSettings, ProviderInstruction};
pub use project::{ProjectSnapshot, ProjectSummary};
pub use prompt::{PromptPreview, PromptPreviewSection};
pub use provider_runtime::{ProviderRuntimeHealth, ProviderRuntimeManager, ProviderRuntimeStatus};
pub use run::{chat_prompt_preview, schedule_cancel_fallback, ChatRunRegistry, ChatRunService};
pub use subprocess_gateway::SubprocessChatGateway;
pub use tools::{
    apply_patch as run_apply_patch_tool, canonical_catalog_bytes, canonical_json,
    catalog_fingerprint, check_write_content_precondition as check_write_file_content_precondition,
    classify as classify_file_tool, default_credential_guard, edit_file as run_edit_file_tool,
    file_permission_action_for_mode, image_generate_tool_descriptor,
    preview_diff as file_tool_preview_diff, read_file as run_read_file_tool, redact_event,
    run_command_typed_payload, search_text as run_search_text_tool, tool_batch_plan,
    validate_args_shallow as validate_file_tool_args_shallow, write_file as run_write_file_tool,
    write_file_with_limit as run_write_file_tool_with_limit,
    write_file_with_limit_and_observation as run_write_file_tool_with_limit_and_observation,
    ApprovalPreview, BackendOutcome, CatalogDrift, CatalogPin, ConservativeCommandPermissionPolicy,
    CredentialGuard, FileMetadata, FileSystem, FileTool, FileToolCapability, FileToolError,
    FileToolOutcome, FileToolOutputStore, FileToolSpill, ModeAwareCommandPermissionPolicy,
    NoopCredentialGuard, NoopToolExecutionEventSink, PathError, PatternCredentialGuard,
    PendingToolApprovalGate, RedactingOutputStore, ResourceLease, ResourceRequest,
    SpawnedToolProcess, StaticToolApprovalGate, StdFileSystem, ToolApprovalAnswer,
    ToolApprovalDecision, ToolApprovalGate, ToolApprovalMode, ToolApprovalModeStore, ToolArtifact,
    ToolArtifactRange, ToolBackend, ToolBatchPlan, ToolCallContext, ToolCancellationToken,
    ToolCapability, ToolCommand, ToolConcurrency, ToolDecision, ToolExecutionAccepted,
    ToolExecutionCancellationResult, ToolExecutionEvent, ToolExecutionEventKind,
    ToolExecutionEventSink, ToolExecutionRecord, ToolExecutionRegistry, ToolExecutionRequest,
    ToolExecutionResult, ToolExecutionStatus, ToolExecutor, ToolKind, ToolOrchestrator,
    ToolOutputContext, ToolOutputPolicy, ToolOutputStore, ToolOutputStream, ToolPermissionAction,
    ToolPermissionEvaluation, ToolPermissionPolicy, ToolPolicySettings, ToolPolicyStore,
    ToolProcessExit, ToolProcessSandbox, ToolProcessSpec, ToolRepeatBlock, ToolRepeatGuard,
    ToolRepeatGuardConfig, ToolResourceLimits, ToolSupervisor, UserAwareCommandPermissionPolicy,
    Workspace, APPLY_PATCH_TOOL_NAME, DEFAULT_MAX_WRITE_FILE_BYTES, EDIT_FILE_TOOL_NAME,
    IMAGE_GENERATE_TOOL_NAME, MAX_TOOL_EVENT_BYTES, READ_FILE_TOOL_NAME, RUN_COMMAND_TOOL_NAME,
    SEARCH_TEXT_TOOL_NAME, WRITE_FILE_TOOL_NAME,
};
