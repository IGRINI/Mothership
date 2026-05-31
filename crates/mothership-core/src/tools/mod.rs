//! Agent tool execution runtime.
//!
//! Core owns the product contract: tool requests, approval decisions, resource
//! leases, output policy, events, and final results. Platform-specific process
//! spawning is injected through [`ToolProcessSandbox`] by the sidecar composition
//! root, so Core does not know about Windows Job Objects, process groups, or any
//! other OS detail.

mod cancellation;
mod output;
mod permissions;
mod process;
mod registry;
mod resources;
mod supervisor;
mod types;

pub use cancellation::ToolCancellationToken;
pub use output::{FileToolOutputStore, ToolOutputStore};
pub use permissions::{
    ConservativeCommandPermissionPolicy, PendingToolApprovalGate, StaticToolApprovalGate,
    ToolApprovalDecision, ToolApprovalGate, ToolPermissionAction, ToolPermissionEvaluation,
    ToolPermissionPolicy,
};
pub use process::{SpawnedToolProcess, ToolProcessExit, ToolProcessSandbox, ToolProcessSpec};
pub use registry::ToolExecutionRegistry;
pub use resources::ToolResourceLimits;
pub use supervisor::ToolSupervisor;
pub use types::{
    NoopToolExecutionEventSink, ToolApprovalAnswer, ToolCommand, ToolExecutionAccepted,
    ToolExecutionCancellationResult, ToolExecutionEvent, ToolExecutionEventKind,
    ToolExecutionEventSink, ToolExecutionRecord, ToolExecutionRequest, ToolExecutionResult,
    ToolExecutionStatus, ToolOutputPolicy, ToolOutputStream,
};
