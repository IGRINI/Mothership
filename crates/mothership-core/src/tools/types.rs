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
    pub yield_ms: Option<u64>,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub notify_on_complete: bool,
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
    Backgrounded,
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
    Backgrounded,
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

pub trait ToolBackgroundCompletionSink: Send + Sync {
    fn on_background_command_complete(
        &self,
        request: &ToolExecutionRequest,
        result: &ToolExecutionResult,
    );
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
        "commandIntent": command_intent(command),
        "status": result.status,
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

fn command_intent(command: &ToolCommand) -> &'static str {
    let program = program_name(&command.program);
    match program.as_str() {
        "git" => git_intent(&command.args),
        "ls" | "dir" => "list",
        "cat" | "type" => "read",
        "grep" | "findstr" => "search",
        "pwd" => "location",
        "npm" | "pnpm" | "yarn" | "bun" => package_manager_intent(&command.args),
        "cargo" => cargo_intent(&command.args),
        "powershell" | "pwsh" => shell_command_intent(&powershell_command_text(&command.args)),
        "cmd" => cmd_command_intent(&command.args),
        "bash" | "sh" => shell_script_arg_intent(&command.args),
        _ => "generic",
    }
}

fn git_intent(args: &[String]) -> &'static str {
    match args.first().map(|arg| arg.to_ascii_lowercase()) {
        Some(cmd) if cmd == "status" => "git_status",
        Some(cmd) if cmd == "diff" => "git_diff",
        Some(cmd) if cmd == "show" => "git_show",
        Some(cmd) if cmd == "log" => "git_log",
        Some(cmd) if cmd == "ls-files" => "list",
        _ => "generic",
    }
}

fn package_manager_intent(args: &[String]) -> &'static str {
    let words: Vec<String> = args.iter().map(|arg| arg.to_ascii_lowercase()).collect();
    if words.iter().any(|arg| arg == "test" || arg == "tests") {
        "test"
    } else if words.iter().any(|arg| arg == "build") {
        "build"
    } else if words.iter().any(|arg| arg == "install" || arg == "add") {
        "install"
    } else {
        "generic"
    }
}

fn cargo_intent(args: &[String]) -> &'static str {
    match args.first().map(|arg| arg.to_ascii_lowercase()) {
        Some(cmd) if cmd == "test" => "test",
        Some(cmd) if cmd == "build" => "build",
        Some(cmd) if cmd == "check" => "check",
        _ => "generic",
    }
}

fn shell_command_intent(command: &str) -> &'static str {
    let tokens = shell_words(command);
    let Some(first) = tokens.first().map(|token| token.to_ascii_lowercase()) else {
        return "generic";
    };
    match first.as_str() {
        "get-childitem" | "gci" | "ls" | "dir" => "list",
        "get-content" | "gc" | "cat" | "type" => "read",
        "select-string" | "sls" | "grep" | "findstr" => "search",
        "get-location" | "pwd" => "location",
        "git" => git_intent(&tokens[1..]),
        "npm" | "pnpm" | "yarn" | "bun" => package_manager_intent(&tokens[1..]),
        "cargo" => cargo_intent(&tokens[1..]),
        _ => "generic",
    }
}

fn powershell_command_text(args: &[String]) -> String {
    for (index, arg) in args.iter().enumerate() {
        if arg.eq_ignore_ascii_case("-command") || arg.eq_ignore_ascii_case("-c") {
            return args[index + 1..].join(" ");
        }
    }
    args.first().cloned().unwrap_or_default()
}

fn cmd_command_intent(args: &[String]) -> &'static str {
    if let Some(index) = args
        .iter()
        .position(|arg| arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("-c"))
    {
        shell_command_intent(&args[index + 1..].join(" "))
    } else {
        shell_command_intent(&args.join(" "))
    }
}

fn shell_script_arg_intent(args: &[String]) -> &'static str {
    if let Some(index) = args
        .iter()
        .position(|arg| arg == "-c" || arg == "-lc" || arg == "--command")
    {
        shell_command_intent(&args[index + 1..].join(" "))
    } else {
        shell_command_intent(&args.join(" "))
    }
}

fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in command.chars() {
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn program_name(program: &str) -> String {
    let mut name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim()
        .to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1"] {
        if name.ends_with(suffix) {
            name.truncate(name.len() - suffix.len());
            break;
        }
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed_result() -> ToolExecutionResult {
        ToolExecutionResult {
            tool_call_id: "tc".to_string(),
            status: ToolExecutionStatus::Completed,
            exit_code: Some(0),
            stdout_preview: String::new(),
            stderr_preview: String::new(),
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            stdout_bytes: 0,
            stderr_bytes: 0,
            truncated_for_display: false,
            truncated_for_agent: false,
            log_ref: None,
            message: None,
        }
    }

    fn payload_intent(program: &str, args: &[&str]) -> String {
        let command = ToolCommand::new(program, args.iter().copied());
        let (payload, _) = run_command_typed_payload(&command, &completed_result());
        payload["commandIntent"]
            .as_str()
            .expect("intent")
            .to_string()
    }

    #[test]
    fn run_command_payload_classifies_common_command_intents() {
        assert_eq!(payload_intent("git", &["diff"]), "git_diff");
        assert_eq!(
            payload_intent("git.exe", &["status", "--short"]),
            "git_status"
        );
        assert_eq!(payload_intent("npm", &["run", "build"]), "build");
        assert_eq!(payload_intent("cargo", &["check", "--workspace"]), "check");
        assert_eq!(
            payload_intent("powershell.exe", &["-Command", "Get-ChildItem src"]),
            "list"
        );
        assert_eq!(
            payload_intent("pwsh", &["-c", "Get-Content src/main.ts"]),
            "read"
        );
        assert_eq!(payload_intent("cmd.exe", &["/c", "dir src"]), "list");
    }
}
