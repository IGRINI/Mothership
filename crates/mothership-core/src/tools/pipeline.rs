//! Unified tool-call orchestration vocabulary and contract.
//!
//! Every tool call — whether it spawns a process or runs a typed in-process
//! file/search handler — moves through one lifecycle:
//!
//! ```text
//! classified → permission_requested → approved | denied
//!            → started → (output / updated)* → completed | failed | cancelled
//! ```
//!
//! The phases map onto [`ToolExecutionEventKind`](super::types::ToolExecutionEventKind).
//! Two executor families realize the `started → … → terminal` step differently —
//! a process is spawned and drained by [`ToolSupervisor`](super::ToolSupervisor);
//! a typed tool calls a pure Core handler — but both share the same identity
//! ([`ToolCallContext`]), approval gate, cancellation token, bounded-payload /
//! spill rules, and event stream.
//!
//! This module owns the *vocabulary* ([`ToolKind`]), the per-call *context*
//! ([`ToolCallContext`]), and the *contract* ([`ToolExecutor`]). Concrete
//! executors live in the composition root (the sidecar), where the process
//! sandbox, async runtime, approval gate, and project context are assembled.
//!
//! Non-goals (deliberate): there is no single executor that handles both process
//! and in-process tools, and [`ToolSupervisor`](super::ToolSupervisor) is left
//! intact as the process executor. The trait is the seam; the executors keep
//! their distinct machinery.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use super::cancellation::ToolCancellationToken;
use super::catalog::{
    APPLY_PATCH_TOOL_NAME, EDIT_FILE_TOOL_NAME, IMAGE_GENERATE_TOOL_NAME, READ_FILE_TOOL_NAME,
    RUN_COMMAND_TOOL_NAME, SEARCH_TEXT_TOOL_NAME, WRITE_FILE_TOOL_NAME,
};
use super::file_tools::FileTool;
use crate::{ChatCancellationToken, LlmToolCallResult};

/// The typed identity of an executable tool the runtime stores and renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ToolKind {
    /// `run_command` — spawn a sandboxed OS process.
    RunCommand,
    /// `read_file` — read a workspace file (bounded, read-only).
    ReadFile,
    /// `write_file` — overwrite/create a workspace file.
    WriteFile,
    /// `edit_file` — content-addressed string replacement.
    EditFile,
    /// `apply_patch` — multi-file V4A patch (all-or-none).
    ApplyPatch,
    /// `search_text` — read-only content search of the workspace.
    SearchText,
    /// `image_generate` — provider-routed media generation.
    ImageGenerate,
}

impl ToolKind {
    /// Resolve a known wire tool name to its kind.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            RUN_COMMAND_TOOL_NAME => Self::RunCommand,
            READ_FILE_TOOL_NAME => Self::ReadFile,
            WRITE_FILE_TOOL_NAME => Self::WriteFile,
            EDIT_FILE_TOOL_NAME => Self::EditFile,
            APPLY_PATCH_TOOL_NAME => Self::ApplyPatch,
            SEARCH_TEXT_TOOL_NAME => Self::SearchText,
            IMAGE_GENERATE_TOOL_NAME => Self::ImageGenerate,
            _ => return None,
        })
    }

    /// Resolve a model-submitted tool name to an executable kind.
    pub fn from_routable_name(name: &str) -> Option<Self> {
        Self::from_name(name)
    }

    /// True when Core should accept new calls for this tool kind.
    pub fn is_routable(&self) -> bool {
        true
    }

    /// The canonical wire tool name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RunCommand => RUN_COMMAND_TOOL_NAME,
            Self::ReadFile => READ_FILE_TOOL_NAME,
            Self::WriteFile => WRITE_FILE_TOOL_NAME,
            Self::EditFile => EDIT_FILE_TOOL_NAME,
            Self::ApplyPatch => APPLY_PATCH_TOOL_NAME,
            Self::SearchText => SEARCH_TEXT_TOOL_NAME,
            Self::ImageGenerate => IMAGE_GENERATE_TOOL_NAME,
        }
    }

    /// True for the process executor (`run_command`).
    pub fn is_process(&self) -> bool {
        matches!(self, Self::RunCommand)
    }

    /// True for tools that mutate the filesystem (write / edit / patch). These
    /// always route through the approval gate when policy asks.
    pub fn is_mutating(&self) -> bool {
        matches!(self, Self::WriteFile | Self::EditFile | Self::ApplyPatch)
    }

    /// True for currently routable read-only typed tools (read / search). `run_command`
    /// is intentionally excluded — its read-only-ness is argument-dependent and
    /// decided by the scheduler, not the kind.
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadFile | Self::SearchText)
    }

    /// True if executing the tool requires an active project (workspace root) to
    /// resolve and contain paths.
    pub fn requires_project(&self) -> bool {
        matches!(
            self,
            Self::ReadFile | Self::WriteFile | Self::EditFile | Self::ApplyPatch | Self::SearchText
        )
    }

    /// The corresponding typed file-tool handler, or `None` for process and
    /// provider-service tools.
    pub fn file_tool(&self) -> Option<FileTool> {
        Some(match self {
            Self::RunCommand => return None,
            Self::ReadFile => FileTool::Read,
            Self::WriteFile => FileTool::Write,
            Self::EditFile => FileTool::Edit,
            Self::ApplyPatch => FileTool::ApplyPatch,
            Self::SearchText => FileTool::SearchText,
            Self::ImageGenerate => return None,
        })
    }
}

impl From<FileTool> for ToolKind {
    fn from(tool: FileTool) -> Self {
        match tool {
            FileTool::Read => Self::ReadFile,
            FileTool::Write => Self::WriteFile,
            FileTool::Edit => Self::EditFile,
            FileTool::ApplyPatch => Self::ApplyPatch,
            FileTool::SearchText => Self::SearchText,
        }
    }
}

/// Per-call context threaded through the lifecycle. Holds only call-scoped data
/// (identity, classification, arguments, cancellation); the stable dependencies
/// (sandbox, runtime, sink, approval gate, project, filesystem) live on the
/// concrete [`ToolExecutor`]. Cheap to copy — every field is a reference or a
/// `Copy` scalar.
#[derive(Clone, Copy)]
pub struct ToolCallContext<'a> {
    /// Unique id of this tool invocation (the approval/cancellation key).
    pub tool_call_id: &'a str,
    /// The assistant run this call belongs to, if any.
    pub run_id: Option<&'a str>,
    /// The wire tool name as sent by the model.
    pub tool_name: &'a str,
    /// The typed kind resolved from `tool_name`.
    pub kind: ToolKind,
    /// The raw tool arguments (already JSON-validated at the protocol edge).
    pub arguments: &'a Value,
    /// Per-call cancellation slot, kept in sync with the chat token by the
    /// dispatcher so a cancelled chat denies a pending approval and unblocks
    /// execution.
    pub cancellation: &'a ToolCancellationToken,
    /// The chat-wide cancellation token (cancels every in-flight call).
    pub chat_cancellation: &'a ChatCancellationToken,
}

/// The contract every tool executor implements. The dispatcher owns the shared
/// scaffolding (registry slot, chat-cancellation watcher, kind routing); the
/// executor owns the tool-family-specific work and emits the lifecycle events.
///
/// `execute` returns the model-facing [`LlmToolCallResult`]; lifecycle events
/// (permission / started / completed / …) are emitted through the executor's own
/// event sink as a side effect, exactly as before.
pub trait ToolExecutor: Send + Sync {
    /// Run one tool call to a terminal result.
    fn execute(&self, ctx: ToolCallContext<'_>) -> LlmToolCallResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::default_tool_catalog;

    #[test]
    fn kind_round_trips_through_name() {
        for kind in [
            ToolKind::RunCommand,
            ToolKind::ReadFile,
            ToolKind::WriteFile,
            ToolKind::EditFile,
            ToolKind::ApplyPatch,
            ToolKind::SearchText,
            ToolKind::ImageGenerate,
        ] {
            assert_eq!(ToolKind::from_name(kind.as_str()), Some(kind));
            assert_eq!(ToolKind::from_routable_name(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn every_catalog_tool_has_a_kind() {
        // The pipeline must be able to route every tool the catalog advertises.
        for tool in default_tool_catalog() {
            assert!(
                ToolKind::from_name(&tool.name).is_some(),
                "catalog tool `{}` has no ToolKind",
                tool.name
            );
        }
    }

    #[test]
    fn unknown_name_has_no_kind() {
        assert_eq!(ToolKind::from_name("definitely_not_a_tool"), None);
    }

    #[test]
    fn classification_helpers_partition_kinds() {
        assert!(ToolKind::RunCommand.is_process());
        assert!(!ToolKind::ReadFile.is_process());

        assert!(ToolKind::WriteFile.is_mutating());
        assert!(ToolKind::EditFile.is_mutating());
        assert!(ToolKind::ApplyPatch.is_mutating());
        assert!(!ToolKind::ReadFile.is_mutating());
        assert!(!ToolKind::RunCommand.is_mutating());
        assert!(!ToolKind::ImageGenerate.is_mutating());

        assert!(ToolKind::ReadFile.is_read_only());
        assert!(ToolKind::SearchText.is_read_only());
        assert!(!ToolKind::RunCommand.is_read_only());
        assert!(!ToolKind::ImageGenerate.is_read_only());

        // Every file/search tool requires a project; run_command does not.
        assert!(!ToolKind::RunCommand.requires_project());
        assert!(!ToolKind::ImageGenerate.requires_project());
        for kind in [
            ToolKind::ReadFile,
            ToolKind::WriteFile,
            ToolKind::EditFile,
            ToolKind::ApplyPatch,
            ToolKind::SearchText,
        ] {
            assert!(kind.requires_project());
        }
    }

    #[test]
    fn file_tool_conversion_is_bijective_for_non_command_kinds() {
        for tool in [
            FileTool::Read,
            FileTool::Write,
            FileTool::Edit,
            FileTool::ApplyPatch,
            FileTool::SearchText,
        ] {
            let kind = ToolKind::from(tool);
            assert_eq!(kind.file_tool(), Some(tool));
        }
        assert_eq!(ToolKind::RunCommand.file_tool(), None);
        assert_eq!(ToolKind::ImageGenerate.file_tool(), None);
    }
}
