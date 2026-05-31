use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCommand {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl ToolCommand {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionRequest {
    pub tool_call_id: String,
    pub run_id: Option<String>,
    pub project_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub command: ToolCommand,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub output_policy: ToolOutputPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionAccepted {
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionCancellationResult {
    pub tool_call_id: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolApprovalAnswer {
    pub tool_call_id: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutputPolicy {
    pub memory_preview_bytes: usize,
    pub ui_stream_bytes_per_sec: usize,
    pub agent_tail_bytes: usize,
    pub spill_to_file: bool,
}

impl Default for ToolOutputPolicy {
    fn default() -> Self {
        Self {
            memory_preview_bytes: 256 * 1024,
            ui_stream_bytes_per_sec: 128 * 1024,
            agent_tail_bytes: 64 * 1024,
            spill_to_file: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionStatus {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    PermissionDenied,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionResult {
    pub tool_call_id: String,
    pub status: ToolExecutionStatus,
    pub exit_code: Option<i32>,
    pub stdout_preview: String,
    pub stderr_preview: String,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub truncated_for_display: bool,
    pub truncated_for_agent: bool,
    pub log_ref: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionEventKind {
    Queued,
    PermissionRequested,
    PermissionDenied,
    WaitingForResource,
    Started,
    Output,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionEvent {
    pub tool_call_id: String,
    pub run_id: Option<String>,
    pub project_id: Option<String>,
    pub command: Option<ToolCommand>,
    pub kind: ToolExecutionEventKind,
    pub stream: Option<ToolOutputStream>,
    pub chunk: Option<String>,
    pub message: Option<String>,
    pub result: Option<ToolExecutionResult>,
}

pub trait ToolExecutionEventSink: Send + Sync {
    fn emit(&self, event: ToolExecutionEvent);
}

#[derive(Debug, Default)]
pub struct NoopToolExecutionEventSink;

impl ToolExecutionEventSink for NoopToolExecutionEventSink {
    fn emit(&self, _event: ToolExecutionEvent) {}
}

pub(crate) fn event(
    request: &ToolExecutionRequest,
    kind: ToolExecutionEventKind,
) -> ToolExecutionEvent {
    ToolExecutionEvent {
        tool_call_id: request.tool_call_id.clone(),
        run_id: request.run_id.clone(),
        project_id: request.project_id.clone(),
        command: Some(request.command.clone()),
        kind,
        stream: None,
        chunk: None,
        message: None,
        result: None,
    }
}
