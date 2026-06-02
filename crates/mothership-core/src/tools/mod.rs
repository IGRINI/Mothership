//! Agent tool execution runtime.
//!
//! Core owns the product contract: tool requests, approval decisions, resource
//! leases, output policy, events, and final results. Platform-specific process
//! spawning is injected through [`ToolProcessSandbox`] by the sidecar composition
//! root, so Core does not know about Windows Job Objects, process groups, or any
//! other OS detail.

mod cancellation;
mod catalog;
mod file_edit;
mod file_tools;
mod filesystem;
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
    default_tool_catalog, APPLY_PATCH_TOOL_ID, APPLY_PATCH_TOOL_NAME, EDIT_FILE_TOOL_ID,
    EDIT_FILE_TOOL_NAME, LIST_FILES_TOOL_ID, LIST_FILES_TOOL_NAME, READ_FILE_TOOL_ID,
    READ_FILE_TOOL_NAME, RUN_COMMAND_TOOL_ID, RUN_COMMAND_TOOL_NAME, SEARCH_TEXT_TOOL_ID,
    SEARCH_TEXT_TOOL_NAME, WRITE_FILE_TOOL_ID, WRITE_FILE_TOOL_NAME,
};
pub use file_tools::{
    apply_patch, classify, edit_file, preview_diff, read_file, write_file, FileTool,
    FileToolCapability, FileToolError, FileToolOutcome, FileToolSpill, MAX_TOOL_EVENT_BYTES,
};
pub use search::{
    classify_list_files, classify_search_text, list_files, search_text,
};
pub use filesystem::{FileMetadata, FileSystem, PathError, StdFileSystem, Workspace};
pub use output::{FileToolOutputStore, ToolOutputStore};
pub use permissions::{
    ConservativeCommandPermissionPolicy, PendingToolApprovalGate, StaticToolApprovalGate,
    ToolApprovalDecision, ToolApprovalGate, ToolPermissionAction, ToolPermissionEvaluation,
    ToolPermissionPolicy,
};
pub use pipeline::{ToolCallContext, ToolExecutor, ToolKind};
pub use process::{SpawnedToolProcess, ToolProcessExit, ToolProcessSandbox, ToolProcessSpec};
pub use registry::ToolExecutionRegistry;
pub use repeat_guard::{ToolRepeatBlock, ToolRepeatGuard, ToolRepeatGuardConfig};
pub use resources::ToolResourceLimits;
pub use scheduler::{tool_batch_plan, tool_concurrency, ToolBatchPlan, ToolConcurrency};
pub use supervisor::ToolSupervisor;
pub use types::{
    NoopToolExecutionEventSink, ToolApprovalAnswer, ToolArtifact, ToolCommand,
    ToolExecutionAccepted, ToolExecutionCancellationResult, ToolExecutionEvent,
    ToolExecutionEventKind, ToolExecutionEventSink, ToolExecutionRecord, ToolExecutionRequest,
    ToolExecutionResult, ToolExecutionStatus, ToolOutputPolicy, ToolOutputStream,
};
