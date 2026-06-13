use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use super::pipeline::ToolKind;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
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

// `#[ts(optional_fields)]` emits every `Option<_>` field as `field?: T | null`
// (optional key, still nullable), matching both serde's `#[serde(default)]`
// deserialize tolerance and the thin client, which builds this request and only
// sets a subset of fields (`chat_id` / `workspace_root` are host-only and never
// sent by the UI). Does not affect serde/runtime behavior.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, optional_fields = nullable)]
pub struct ToolExecutionRequest {
    pub tool_call_id: String,
    pub run_id: Option<String>,
    /// The chat whose approval mode governs this call, when the call runs on
    /// behalf of a chat run. `None` (e.g. the protocol-level `run_command`)
    /// falls back to the approval-mode store's default.
    #[serde(default)]
    pub chat_id: Option<String>,
    pub project_id: Option<String>,
    /// The canonical workspace root governing this call, when known. Used by the
    /// manual-mode read-only argument screen to test whether a command argument
    /// references a path outside the workspace. `None` (e.g. a protocol-level
    /// `run_command` from a client that does not send it) falls back to
    /// screening against `cwd` alone.
    #[serde(default)]
    pub workspace_root: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub command: ToolCommand,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub output_policy: ToolOutputPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ToolExecutionAccepted {
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ToolExecutionCancellationResult {
    pub tool_call_id: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ToolApprovalAnswer {
    pub tool_call_id: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ToolOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ToolExecutionStatus {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    PermissionDenied,
    LoopBlocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Default, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ToolExecutionEventKind {
    #[default]
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
    LoopBlocked,
}

/// A durable artifact produced by a tool call — a diff, a captured output
/// stream, search results. The full blob lives in the output store / on disk and
/// is referenced by `log_ref`; only bounded metadata and a short preview are
/// carried inline so the database and event stream never hold multi-megabyte
/// payloads.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ToolArtifact {
    /// Stable id within the tool call (e.g. "diff", "stdout", "results").
    pub artifact_id: String,
    /// What kind of artifact this is (diff / output / results / …).
    pub kind: String,
    /// MIME-ish content type ("text/x-diff", "text/plain", "application/json").
    pub content_type: String,
    /// A bounded, already-truncated preview of the content.
    pub preview: String,
    /// Durable reference to the full blob (output-store logRef or file path).
    pub log_ref: Option<String>,
    /// Total size of the full artifact in bytes.
    pub size_bytes: u64,
    /// SHA-256 of the full artifact, when known.
    pub sha256: Option<String>,
    /// True if `preview` is a truncated prefix of the full artifact.
    pub truncated: bool,
}

/// A newline-aligned slice of a tool's persisted output artifact, served lazily
/// to the UI. Lets the UI page the full blob on demand without ever pulling the
/// whole thing into memory or history — and always from the *snapshot* the tool
/// produced, never the (possibly changed) live file.
///
/// Offsets are byte positions in the underlying blob **file** (so a bounded read
/// can `seek` straight to them); `content` is that window with the writer's
/// stream-header lines stripped. Callers page by echoing `next_offset` back as
/// the next `offset` — they never compute offsets in the stripped space.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ToolArtifactRange {
    /// The decoded text for the requested range, with stream headers stripped.
    pub content: String,
    /// Byte offset into the blob FILE where this slice starts (a line boundary).
    pub offset: u64,
    /// Blob-file byte offset to pass back as the next `offset` to continue
    /// paging, or `None` at EOF.
    pub next_offset: Option<u64>,
    /// Total size of the blob FILE in bytes (an upper bound on visible content;
    /// it still includes the writer's stream-header bytes).
    pub total_bytes: u64,
    /// True when this slice reaches the end of the artifact.
    pub eof: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
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
    /// Typed kind of the tool, for typed storage + UI routing. `None` for legacy
    /// emitters not yet updated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_kind: Option<ToolKind>,
    /// Semantic, tool-specific payload (paths, counts, statuses, sha — never huge
    /// content). Powers typed storage and UI semantic cards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// Workspace-relative paths this call touched (for the call summary).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touched_paths: Vec<String>,
    /// Durable artifacts (diff/output/results) — referenced, not inlined.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ToolArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ToolExecutionRecord {
    pub tool_call_id: String,
    pub run_id: Option<String>,
    pub chat_id: String,
    pub message_id: String,
    pub project_id: Option<String>,
    pub command: Option<ToolCommand>,
    pub kind: ToolExecutionEventKind,
    pub message: Option<String>,
    pub output: String,
    pub result: Option<ToolExecutionResult>,
    pub created_at: String,
    pub updated_at: String,
    /// Typed kind of the tool (from typed storage), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_kind: Option<ToolKind>,
    /// Latest semantic payload for the call (from typed storage), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// Durable artifacts for the call (from typed storage).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ToolArtifact>,
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
        // This helper builds events for the process executor; the typed payload +
        // output artifact are synthesized from `command` + `result` at the storage
        // layer, so the supervisor stays untouched.
        tool_kind: Some(ToolKind::RunCommand),
        payload: None,
        touched_paths: Vec::new(),
        artifacts: Vec::new(),
    }
}

/// The typed `run_command` payload (program/args/exit/output) + an output
/// artifact, derived from a command and its result. Shared by the command
/// backend (so the live event carries it and the UI shows a semantic card) and
/// the storage-layer synthesis (so a reload produces an identical card).
pub fn run_command_typed_payload(
    command: &ToolCommand,
    result: &ToolExecutionResult,
) -> (Value, Vec<ToolArtifact>) {
    let payload = serde_json::json!({
        "program": command.program,
        "args": command.args,
        "exitCode": result.exit_code,
        "stdoutPreview": result.stdout_preview,
        "stderrPreview": result.stderr_preview,
        "stdoutBytes": result.stdout_bytes,
        "stderrBytes": result.stderr_bytes,
        "truncated": result.truncated_for_display,
        "logRef": result.log_ref,
    });
    let mut artifacts = Vec::new();
    if result.stdout_bytes > 0 || result.log_ref.is_some() {
        artifacts.push(ToolArtifact {
            artifact_id: "stdout".to_string(),
            kind: "output".to_string(),
            content_type: "text/plain".to_string(),
            preview: result.stdout_preview.clone(),
            log_ref: result.log_ref.clone(),
            size_bytes: result.stdout_bytes as u64,
            sha256: None,
            truncated: result.truncated_for_display,
        });
    }
    (payload, artifacts)
}
