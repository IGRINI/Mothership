//! Agent tool execution runtime.
//!
//! Core owns the product contract: tool requests, approval decisions, resource
//! leases, output policy, events, and final results. Platform-specific process
//! spawning is injected through [`ToolProcessSandbox`] by the sidecar composition
//! root, so Core does not know about Windows Job Objects, process groups, or any
//! other OS detail.

mod cancellation;
mod catalog;
mod credential_guard;
mod file_edit;
mod file_tools;
mod filesystem;
mod orchestrator;
mod output;
mod patch;
mod permissions;
mod pipeline;
mod process;
mod registry;
mod repeat_guard;
mod resources;
mod scheduler;
mod search;
mod supervisor;
mod types;

pub use cancellation::ToolCancellationToken;
pub use catalog::{
    canonical_catalog_bytes, canonical_json, catalog_fingerprint, default_tool_catalog,
    CatalogDrift, CatalogPin, APPLY_PATCH_TOOL_ID, APPLY_PATCH_TOOL_NAME, EDIT_FILE_TOOL_ID,
    EDIT_FILE_TOOL_NAME, LIST_FILES_TOOL_ID, LIST_FILES_TOOL_NAME, READ_FILE_TOOL_ID,
    READ_FILE_TOOL_NAME, RUN_COMMAND_TOOL_ID, RUN_COMMAND_TOOL_NAME, SEARCH_TEXT_TOOL_ID,
    SEARCH_TEXT_TOOL_NAME, WRITE_FILE_TOOL_ID, WRITE_FILE_TOOL_NAME,
};
pub use credential_guard::{
    default_credential_guard, redact_event, CredentialGuard, NoopCredentialGuard,
    PatternCredentialGuard, RedactingOutputStore,
};
pub use file_tools::{
    apply_patch, check_write_content_precondition, classify, edit_file, preview_diff, read_file,
    validate_args_shallow, write_file, write_file_with_limit,
    write_file_with_limit_and_observation, FileTool, FileToolCapability, FileToolError,
    FileToolOutcome, FileToolSpill, DEFAULT_MAX_WRITE_FILE_BYTES, MAX_TOOL_EVENT_BYTES,
};
pub use filesystem::{FileMetadata, FileSystem, PathError, StdFileSystem, Workspace};
pub use orchestrator::{
    ApprovalPreview, BackendOutcome, ResourceLease, ResourceRequest, ToolBackend, ToolCapability,
    ToolDecision, ToolOrchestrator,
};
pub use output::{FileToolOutputStore, ToolOutputStore};
pub use permissions::{
    command_permission_for_mode, file_permission_action_for_mode,
    ConservativeCommandPermissionPolicy, ModeAwareCommandPermissionPolicy, PendingToolApprovalGate,
    StaticToolApprovalGate, ToolApprovalDecision, ToolApprovalGate, ToolApprovalMode,
    ToolApprovalModeStore, ToolPermissionAction, ToolPermissionEvaluation, ToolPermissionPolicy,
    ToolPolicySettings, ToolPolicyStore, UserAwareCommandPermissionPolicy,
};
pub use pipeline::{ToolCallContext, ToolExecutor, ToolKind};
pub use process::{SpawnedToolProcess, ToolProcessExit, ToolProcessSandbox, ToolProcessSpec};
pub use registry::ToolExecutionRegistry;
pub use repeat_guard::{ToolRepeatBlock, ToolRepeatGuard, ToolRepeatGuardConfig};
pub use resources::ToolResourceLimits;
pub use scheduler::{tool_batch_plan, tool_concurrency, ToolBatchPlan, ToolConcurrency};
pub use search::{classify_list_files, classify_search_text, list_files, search_text};
pub use supervisor::ToolSupervisor;
pub use types::{
    run_command_typed_payload, NoopToolExecutionEventSink, ToolApprovalAnswer, ToolArtifact,
    ToolArtifactRange, ToolCommand, ToolExecutionAccepted, ToolExecutionCancellationResult,
    ToolExecutionEvent, ToolExecutionEventKind, ToolExecutionEventSink, ToolExecutionRecord,
    ToolExecutionRequest, ToolExecutionResult, ToolExecutionStatus, ToolOutputPolicy,
    ToolOutputStream,
};
