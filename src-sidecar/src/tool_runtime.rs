use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use mothership_core::{
    classify_file_tool, file_tool_preview_diff, run_apply_patch_tool, run_edit_file_tool,
    run_read_file_tool, run_write_file_tool, tool_batch_plan, ChatCancellationToken, FileTool,
    FileToolOutcome,
    FileToolSpill, LlmToolCallHandler, LlmToolCallRequest, LlmToolCallResult, MothershipError,
    PendingToolApprovalGate, Result, SpawnedToolProcess, StdFileSystem, ToolApprovalDecision,
    ToolBatchPlan, ToolCancellationToken, ToolCommand, ToolExecutionEvent, ToolExecutionEventKind,
    ToolExecutionEventSink, ToolExecutionRegistry, ToolExecutionRequest, ToolExecutionResult,
    ToolExecutionStatus, ToolOutputPolicy, ToolOutputStore, ToolPermissionAction, ToolProcessExit,
    ToolProcessSandbox, ToolProcessSpec, ToolSupervisor, Workspace, MAX_TOOL_EVENT_BYTES,
    RUN_COMMAND_TOOL_NAME,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncRead;

const DEFAULT_TOOL_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const MAX_TOOL_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const CHAT_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_TOOL_RESULT_CHARS: usize = 160 * 1024;

pub struct ProcessSandboxToolAdapter {
    inner: Arc<dyn process_sandbox::ProcessSandbox>,
}

impl ProcessSandboxToolAdapter {
    pub fn new(inner: Arc<dyn process_sandbox::ProcessSandbox>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl ToolProcessSandbox for ProcessSandboxToolAdapter {
    async fn spawn(&self, spec: ToolProcessSpec) -> Result<Box<dyn SpawnedToolProcess>> {
        let spec = platform_process_spec(spec);
        let process = self
            .inner
            .spawn(process_sandbox::ToolSpec {
                program: spec.program,
                args: spec.args,
                cwd: spec.cwd,
                env: spec.env,
                timeout: spec.timeout,
            })
            .await
            .map_err(|error| MothershipError::Runtime(error.to_string()))?;

        Ok(Box::new(SpawnedProcessAdapter { inner: process }))
    }
}

#[cfg(windows)]
fn platform_process_spec(spec: ToolProcessSpec) -> ToolProcessSpec {
    let program = spec
        .program
        .to_string_lossy()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let args = spec
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect::<Vec<_>>();

    match program.as_str() {
        "pwd" => powershell_spec(
            spec,
            "Get-Location | Select-Object -ExpandProperty Path",
            Vec::new(),
        ),
        "ls" | "dir" => powershell_spec(
            spec,
            "if ($args.Count -eq 0) { Get-ChildItem -Force } else { Get-ChildItem -Force -LiteralPath $args }",
            args,
        ),
        "cat" | "type" => powershell_spec(
            spec,
            "if ($args.Count -eq 0) { Write-Error 'file path is required'; exit 2 } Get-Content -Raw -LiteralPath $args",
            args,
        ),
        "grep" => powershell_spec(
            spec,
            "if ($args.Count -lt 1) { Write-Error 'pattern is required'; exit 2 } $pattern = $args[0]; $paths = if ($args.Count -gt 1) { $args[1..($args.Count - 1)] } else { @('.') }; Select-String -Pattern $pattern -Path $paths",
            args,
        ),
        _ => spec,
    }
}

#[cfg(not(windows))]
fn platform_process_spec(spec: ToolProcessSpec) -> ToolProcessSpec {
    spec
}

#[cfg(windows)]
fn powershell_spec(
    mut spec: ToolProcessSpec,
    script: &str,
    script_args: Vec<String>,
) -> ToolProcessSpec {
    let script = format!(
        "$__mothershipEncoding = [System.Text.UTF8Encoding]::new($false); \
         [Console]::InputEncoding = $__mothershipEncoding; \
         [Console]::OutputEncoding = $__mothershipEncoding; \
         $OutputEncoding = $__mothershipEncoding; \
         {script}"
    );
    let mut args = vec![
        OsString::from("-NoProfile"),
        OsString::from("-NonInteractive"),
        OsString::from("-Command"),
        OsString::from(script),
    ];
    args.extend(script_args.into_iter().map(OsString::from));
    spec.program = OsString::from("powershell.exe");
    spec.args = args;
    spec
}

#[derive(Clone)]
pub struct SidecarLlmToolHandler {
    supervisor: Arc<ToolSupervisor>,
    registry: Arc<ToolExecutionRegistry>,
    runtime: Arc<tokio::runtime::Runtime>,
    sink: Arc<dyn ToolExecutionEventSink>,
    project: Option<ToolProjectContext>,
    approvals: Arc<PendingToolApprovalGate>,
    file_system: StdFileSystem,
    output_store: Option<Arc<dyn ToolOutputStore>>,
}

#[derive(Clone)]
struct ToolProjectContext {
    id: String,
    root: PathBuf,
}

impl SidecarLlmToolHandler {
    pub fn new(
        supervisor: Arc<ToolSupervisor>,
        registry: Arc<ToolExecutionRegistry>,
        runtime: Arc<tokio::runtime::Runtime>,
        sink: Arc<dyn ToolExecutionEventSink>,
        project: Option<(String, PathBuf)>,
        approvals: Arc<PendingToolApprovalGate>,
        output_store: Option<Arc<dyn ToolOutputStore>>,
    ) -> Self {
        Self {
            supervisor,
            registry,
            runtime,
            sink,
            project: project.map(|(id, root)| ToolProjectContext { id, root }),
            approvals,
            file_system: StdFileSystem::new(),
            output_store,
        }
    }
}

impl LlmToolCallHandler for SidecarLlmToolHandler {
    fn handle_tool_call(
        &self,
        request: LlmToolCallRequest,
        chat_cancellation: &ChatCancellationToken,
    ) -> LlmToolCallResult {
        match request.name.as_str() {
            RUN_COMMAND_TOOL_NAME => self.run_command(request, chat_cancellation),
            other => match FileTool::from_name(other) {
                Some(tool) => self.run_file_tool(tool, request, chat_cancellation),
                None => LlmToolCallResult {
                    ok: false,
                    content: format!("unsupported tool `{other}`"),
                },
            },
        }
    }

    fn handle_tool_calls(
        &self,
        requests: Vec<LlmToolCallRequest>,
        chat_cancellation: &ChatCancellationToken,
    ) -> Vec<LlmToolCallResult> {
        match tool_batch_plan(&requests) {
            ToolBatchPlan::Sequential => requests
                .into_iter()
                .map(|request| self.handle_tool_call(request, chat_cancellation))
                .collect(),
            ToolBatchPlan::Parallel => {
                let handles = requests
                    .into_iter()
                    .enumerate()
                    .map(|(index, request)| {
                        let handler = self.clone();
                        let cancellation = chat_cancellation.clone();
                        thread::spawn(move || {
                            (index, handler.handle_tool_call(request, &cancellation))
                        })
                    })
                    .collect::<Vec<_>>();
                let mut results = handles
                    .into_iter()
                    .map(|handle| match handle.join() {
                        Ok(result) => result,
                        Err(_) => (
                            usize::MAX,
                            LlmToolCallResult {
                                ok: false,
                                content: "tool worker thread panicked".to_string(),
                            },
                        ),
                    })
                    .collect::<Vec<_>>();
                results.sort_by_key(|(index, _)| *index);
                results.into_iter().map(|(_, result)| result).collect()
            }
        }
    }
}

impl SidecarLlmToolHandler {
    fn run_command(
        &self,
        request: LlmToolCallRequest,
        chat_cancellation: &ChatCancellationToken,
    ) -> LlmToolCallResult {
        let tool_request = match tool_execution_request(request, self.project.as_ref()) {
            Ok(request) => request,
            Err(error) => {
                return LlmToolCallResult {
                    ok: false,
                    content: error,
                };
            }
        };

        let cancellation = ToolCancellationToken::default();
        if chat_cancellation.is_cancelled() {
            cancellation.cancel();
        }
        if !self
            .registry
            .register(&tool_request.tool_call_id, cancellation.clone())
        {
            return LlmToolCallResult {
                ok: false,
                content: format!("tool call already active: {}", tool_request.tool_call_id),
            };
        }

        let finished = Arc::new(AtomicBool::new(false));
        let watcher_finished = Arc::clone(&finished);
        let watcher_cancellation = cancellation.clone();
        let chat_cancellation = chat_cancellation.clone();
        let watcher = thread::spawn(move || {
            while !watcher_finished.load(Ordering::SeqCst) {
                if chat_cancellation.is_cancelled() {
                    watcher_cancellation.cancel();
                    return;
                }
                thread::sleep(CHAT_CANCEL_POLL_INTERVAL);
            }
        });

        let result = self.runtime.block_on(self.supervisor.run_command(
            tool_request.clone(),
            cancellation,
            Arc::clone(&self.sink),
        ));
        finished.store(true, Ordering::SeqCst);
        let _ = watcher.join();
        self.registry.finish(&tool_request.tool_call_id);

        match result {
            Ok(result) => LlmToolCallResult {
                ok: result.status == ToolExecutionStatus::Completed,
                content: format_tool_result(&result),
            },
            Err(error) => LlmToolCallResult {
                ok: false,
                content: format!("tool supervisor failed: {error}"),
            },
        }
    }
}

impl SidecarLlmToolHandler {
    /// Execute one of the typed file tools (`read_file` / `write_file` /
    /// `edit_file` / `apply_patch`). Mirrors `run_command`'s cross-cuts —
    /// per-tool registry slot, chat-cancellation honoring, and the same
    /// `PermissionRequested` → approval-gate → `PermissionDenied` flow — but the
    /// "execute" step calls a pure Core handler instead of spawning a process.
    /// Synthesizes a [`ToolExecutionResult`] (text in the stdout fields,
    /// `command: None`) so existing event/persistence plumbing works unchanged.
    fn run_file_tool(
        &self,
        tool: FileTool,
        request: LlmToolCallRequest,
        chat_cancellation: &ChatCancellationToken,
    ) -> LlmToolCallResult {
        let tool_call_id = request.tool_call_id.clone();
        let run_id = request.run_id.clone();
        let arguments = request.arguments.clone();

        // File tools require a known project root to resolve + contain paths.
        let Some(project) = self.project.clone() else {
            return LlmToolCallResult {
                ok: false,
                content: "file tools require an active project; none is associated with this chat"
                    .to_string(),
            };
        };
        let project_id = Some(project.id.clone());

        let workspace = match Workspace::new(&project.root) {
            Ok(workspace) => workspace,
            Err(error) => {
                return LlmToolCallResult {
                    ok: false,
                    content: format!("project root is unavailable: {error}"),
                };
            }
        };

        // Per-tool-call cancellation slot, kept in sync with the chat token so a
        // cancelled chat denies a pending approval and short-circuits execution.
        let cancellation = ToolCancellationToken::default();
        if chat_cancellation.is_cancelled() {
            cancellation.cancel();
        }
        if !self.registry.register(&tool_call_id, cancellation.clone()) {
            return LlmToolCallResult {
                ok: false,
                content: format!("tool call already active: {tool_call_id}"),
            };
        }

        // Watch the chat token like `run_command` does, so a chat cancelled while
        // the tool is awaiting approval (or running) flips the per-call token and
        // unblocks `request_decision`.
        let finished = Arc::new(AtomicBool::new(false));
        let watcher_finished = Arc::clone(&finished);
        let watcher_cancellation = cancellation.clone();
        let watcher_chat_cancellation = chat_cancellation.clone();
        let watcher = thread::spawn(move || {
            while !watcher_finished.load(Ordering::SeqCst) {
                if watcher_chat_cancellation.is_cancelled() {
                    watcher_cancellation.cancel();
                    return;
                }
                thread::sleep(CHAT_CANCEL_POLL_INTERVAL);
            }
        });

        let result = self.run_file_tool_inner(
            tool,
            &arguments,
            &workspace,
            &tool_call_id,
            &run_id,
            &project_id,
            &cancellation,
            chat_cancellation,
        );

        finished.store(true, Ordering::SeqCst);
        let _ = watcher.join();
        self.registry.finish(&tool_call_id);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn run_file_tool_inner(
        &self,
        tool: FileTool,
        arguments: &Value,
        workspace: &Workspace,
        tool_call_id: &str,
        run_id: &Option<String>,
        project_id: &Option<String>,
        cancellation: &ToolCancellationToken,
        chat_cancellation: &ChatCancellationToken,
    ) -> LlmToolCallResult {
        // Classify capability (intent + touched paths + allow/ask/deny).
        let capability = match classify_file_tool(tool, arguments, workspace) {
            Ok(capability) => capability,
            Err(error) => {
                return LlmToolCallResult {
                    ok: false,
                    content: error.to_string(),
                };
            }
        };

        match capability.action {
            ToolPermissionAction::Deny => {
                let result = synthesized_result(
                    tool_call_id,
                    ToolExecutionStatus::PermissionDenied,
                    String::new(),
                    Some(capability.summary.clone()),
                );
                self.emit_file_event(
                    tool_call_id,
                    run_id,
                    project_id,
                    ToolExecutionEventKind::PermissionDenied,
                    Some(capability.summary.clone()),
                    Some(result),
                );
                return LlmToolCallResult {
                    ok: false,
                    content: capability.summary,
                };
            }
            ToolPermissionAction::Ask => {
                // Surface the approval request (with a diff/summary preview) and
                // block on the same gate the UI drives via `decide`.
                let preview = self.build_preview(tool, arguments, workspace, &capability.summary);
                self.emit_file_event(
                    tool_call_id,
                    run_id,
                    project_id,
                    ToolExecutionEventKind::PermissionRequested,
                    Some(preview),
                    None,
                );
                let decision = self
                    .runtime
                    .block_on(self.approvals.request_decision(tool_call_id, cancellation));
                if let ToolApprovalDecision::Denied { reason } = decision {
                    let result = synthesized_result(
                        tool_call_id,
                        ToolExecutionStatus::PermissionDenied,
                        String::new(),
                        Some(reason.clone()),
                    );
                    self.emit_file_event(
                        tool_call_id,
                        run_id,
                        project_id,
                        ToolExecutionEventKind::PermissionDenied,
                        Some(reason.clone()),
                        Some(result),
                    );
                    return LlmToolCallResult {
                        ok: false,
                        content: format!("tool call denied: {reason}"),
                    };
                }
            }
            ToolPermissionAction::Allow => {}
        }

        // A chat cancelled during approval (or before execution) is reported as
        // a cancellation rather than running the side effect.
        if cancellation.is_cancelled() || chat_cancellation.is_cancelled() {
            let result = synthesized_result(
                tool_call_id,
                ToolExecutionStatus::Cancelled,
                String::new(),
                Some("tool call was cancelled".to_string()),
            );
            self.emit_file_event(
                tool_call_id,
                run_id,
                project_id,
                ToolExecutionEventKind::Cancelled,
                Some("tool call was cancelled".to_string()),
                Some(result),
            );
            return LlmToolCallResult {
                ok: false,
                content: "tool call was cancelled".to_string(),
            };
        }

        self.emit_file_event(
            tool_call_id,
            run_id,
            project_id,
            ToolExecutionEventKind::Started,
            Some(capability.summary.clone()),
            None,
        );

        // Execute the pure handler through the injected filesystem port.
        let spill = self.output_store.as_ref().map(|store| AsyncOutputStoreSpill {
            store: Arc::clone(store),
            runtime: Arc::clone(&self.runtime),
        });
        let outcome = match tool {
            FileTool::Read => run_read_file_tool(
                arguments,
                workspace,
                &self.file_system,
                tool_call_id,
                spill.as_ref().map(|spill| spill as &dyn FileToolSpill),
            ),
            FileTool::Write => run_write_file_tool(arguments, workspace, &self.file_system),
            FileTool::Edit => run_edit_file_tool(arguments, workspace, &self.file_system),
            FileTool::ApplyPatch => run_apply_patch_tool(arguments, workspace, &self.file_system),
        };

        match outcome {
            Ok(outcome) => {
                let status = if outcome.ok {
                    ToolExecutionStatus::Completed
                } else {
                    ToolExecutionStatus::Failed
                };
                // The model gets the full (160 KiB-capped) text. The event/DB
                // payload is bounded separately to MAX_TOOL_EVENT_BYTES so a huge
                // overwrite's diff cannot push megabytes into persistence/UI; the
                // full diff is spilled to the output store (when available) and a
                // logRef is attached to the stored result instead.
                let model_response = truncate_for_model(file_outcome_text(&outcome));
                let bounded = bound_event_payload(
                    &outcome,
                    tool_call_id,
                    spill.as_ref().map(|spill| spill as &dyn FileToolSpill),
                );
                let mut result = synthesized_result(
                    tool_call_id,
                    status,
                    bounded.result_text,
                    bounded.diff.clone(),
                );
                result.log_ref = bounded.log_ref;
                result.truncated_for_display = bounded.truncated;
                let kind = if outcome.ok {
                    ToolExecutionEventKind::Completed
                } else {
                    ToolExecutionEventKind::Failed
                };
                self.emit_file_event(tool_call_id, run_id, project_id, kind, bounded.diff, Some(result));
                LlmToolCallResult {
                    ok: outcome.ok,
                    content: model_response,
                }
            }
            Err(error) => {
                let message = error.to_string();
                let result = synthesized_result(
                    tool_call_id,
                    ToolExecutionStatus::Failed,
                    String::new(),
                    Some(message.clone()),
                );
                self.emit_file_event(
                    tool_call_id,
                    run_id,
                    project_id,
                    ToolExecutionEventKind::Failed,
                    Some(message.clone()),
                    Some(result),
                );
                LlmToolCallResult {
                    ok: false,
                    content: message,
                }
            }
        }
    }

    /// Build the approval-card message: the capability summary, with a
    /// side-effect-free diff preview appended for a mutating tool so the human
    /// (or remote approver) sees what will change before approving. The diff is
    /// computed by Core via a dry run (no writes); when it cannot be previewed
    /// (file missing, edit would not match, patch would not apply) only the
    /// summary is shown and the handler will report the precise failure.
    ///
    /// The preview is bounded to [`MAX_TOOL_EVENT_BYTES`] before it is returned:
    /// it flows into the `PermissionRequested` event message, which is persisted
    /// and pushed to the UI, so an enormous diff (e.g. overwriting a multi-MB
    /// file) must not push megabytes through the event store. When the diff
    /// portion overflows the budget it is truncated on a char boundary with a
    /// marker noting the full size (the full change still applies on approval).
    fn build_preview(
        &self,
        tool: FileTool,
        arguments: &Value,
        workspace: &Workspace,
        summary: &str,
    ) -> String {
        match file_tool_preview_diff(tool, arguments, workspace, &self.file_system) {
            Some(diff) if !diff.is_empty() => bound_preview(summary, &diff),
            _ => summary.to_string(),
        }
    }

    fn emit_file_event(
        &self,
        tool_call_id: &str,
        run_id: &Option<String>,
        project_id: &Option<String>,
        kind: ToolExecutionEventKind,
        message: Option<String>,
        result: Option<ToolExecutionResult>,
    ) {
        self.sink.emit(ToolExecutionEvent {
            tool_call_id: tool_call_id.to_string(),
            run_id: run_id.clone(),
            project_id: project_id.clone(),
            command: None,
            kind,
            stream: None,
            chunk: None,
            message,
            result,
        });
    }
}

/// Bridge the async [`ToolOutputStore`] to the synchronous [`FileToolSpill`] the
/// `read_file` handler expects: open a writer, append the full content as one
/// stdout chunk, and finish to obtain the durable reference.
struct AsyncOutputStoreSpill {
    store: Arc<dyn ToolOutputStore>,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl FileToolSpill for AsyncOutputStoreSpill {
    fn spill(&self, tool_call_id: &str, content: &str) -> std::io::Result<String> {
        let store = Arc::clone(&self.store);
        let tool_call_id = tool_call_id.to_string();
        let content = content.to_string();
        self.runtime.block_on(async move {
            let mut writer = store
                .open(&tool_call_id)
                .await
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            writer
                .append(
                    mothership_core::ToolOutputStream::Stdout,
                    content.as_bytes(),
                )
                .await
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            writer
                .finish()
                .await
                .map_err(|error| std::io::Error::other(error.to_string()))
        })
    }
}

/// Synthesize a [`ToolExecutionResult`] for a file tool: the model-facing text
/// goes in `stdout_preview`/`stdout_tail`, there is no exit code, and `command`
/// is `None` (set by the event builder). Reuses the existing result/event/persist
/// path so file tools render through the same machinery as `run_command`.
fn synthesized_result(
    tool_call_id: &str,
    status: ToolExecutionStatus,
    stdout_text: String,
    message: Option<String>,
) -> ToolExecutionResult {
    ToolExecutionResult {
        tool_call_id: tool_call_id.to_string(),
        status,
        exit_code: None,
        stdout_preview: stdout_text.clone(),
        stderr_preview: String::new(),
        stdout_tail: stdout_text,
        stderr_tail: String::new(),
        stdout_bytes: 0,
        stderr_bytes: 0,
        truncated_for_display: false,
        truncated_for_agent: false,
        log_ref: None,
        message,
    }
}

/// The text returned to the model for a file-tool outcome: the handler's
/// `model_text`, with the diff appended for a mutating success so the model sees
/// what changed.
fn file_outcome_text(outcome: &FileToolOutcome) -> String {
    match (&outcome.diff, outcome.ok) {
        (Some(diff), true) if !diff.is_empty() => {
            format!("{}\n\n{}", outcome.model_text, diff)
        }
        _ => outcome.model_text.clone(),
    }
}

/// What goes into the persisted [`ToolExecutionResult`] / event for a file tool,
/// after bounding to [`MAX_TOOL_EVENT_BYTES`] so the database/UI never receive a
/// multi-megabyte diff. The full diff is written to the output store (when one is
/// available) and referenced via `log_ref`.
struct BoundedEventPayload {
    /// Bounded text for the result's stdout fields (summary + bounded diff).
    result_text: String,
    /// Bounded diff for the event message, or `None` when there is no diff.
    diff: Option<String>,
    /// Reference to the spilled full diff, when it was spilled.
    log_ref: Option<String>,
    /// Whether anything was truncated relative to the full payload.
    truncated: bool,
}

/// Bound the event/DB payload for a file-tool outcome. The model-facing response
/// is bounded separately by the caller; this only shapes what is stored and
/// streamed to the UI. When the diff exceeds the budget it is truncated with a
/// marker and (if a spill sink is present) the full diff is persisted and a
/// `logRef` is returned so the full content stays retrievable.
fn bound_event_payload(
    outcome: &FileToolOutcome,
    tool_call_id: &str,
    spill: Option<&dyn FileToolSpill>,
) -> BoundedEventPayload {
    let full_diff = outcome
        .diff
        .as_deref()
        .filter(|diff| !diff.is_empty() && outcome.ok);

    let Some(full_diff) = full_diff else {
        // No diff to bound; the summary alone is already small.
        return BoundedEventPayload {
            result_text: outcome.model_text.clone(),
            diff: None,
            log_ref: None,
            truncated: false,
        };
    };

    if full_diff.len() <= MAX_TOOL_EVENT_BYTES {
        return BoundedEventPayload {
            result_text: format!("{}\n\n{}", outcome.model_text, full_diff),
            diff: Some(full_diff.to_string()),
            log_ref: None,
            truncated: false,
        };
    }

    // Oversized diff: spill the full diff (best effort) and keep only a bounded
    // prefix in the event/result.
    let log_ref = spill.and_then(|spill| spill.spill(tool_call_id, full_diff).ok());
    let mut bounded_diff = truncate_on_char_boundary(full_diff, MAX_TOOL_EVENT_BYTES);
    bounded_diff.push_str("\n... diff truncated for storage ...\n");
    if let Some(log_ref) = &log_ref {
        bounded_diff.push_str(&format!("full diff: {log_ref}\n"));
    }
    BoundedEventPayload {
        result_text: format!("{}\n\n{}", outcome.model_text, bounded_diff),
        diff: Some(bounded_diff),
        log_ref,
        truncated: true,
    }
}

/// Take the longest prefix of `text` not exceeding `max_bytes` that ends on a
/// char boundary, so the truncated diff is always valid UTF-8.
fn truncate_on_char_boundary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Compose an approval-preview message (`{summary}\n\n{diff}`) and bound the diff
/// portion to [`MAX_TOOL_EVENT_BYTES`]. The `PermissionRequested` event message
/// is persisted and rendered, so an oversized preview diff must be truncated
/// before it leaves Core's preview path. The (small) summary is always kept; only
/// the diff is truncated, on a char boundary, with a marker stating how much was
/// shown of the full size so the approver knows the full change still applies.
fn bound_preview(summary: &str, diff: &str) -> String {
    if diff.len() <= MAX_TOOL_EVENT_BYTES {
        return format!("{summary}\n\n{diff}");
    }
    let full = diff.len();
    let mut bounded = truncate_on_char_boundary(diff, MAX_TOOL_EVENT_BYTES);
    let shown = bounded.len();
    bounded.push_str(&format!(
        "\n[preview truncated: showing {shown} of {full} bytes — approve to apply the full change]"
    ));
    format!("{summary}\n\n{bounded}")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunCommandArguments {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default, alias = "timeout_ms")]
    timeout_ms: Option<u64>,
}

fn tool_execution_request(
    request: LlmToolCallRequest,
    project: Option<&ToolProjectContext>,
) -> std::result::Result<ToolExecutionRequest, String> {
    let arguments = serde_json::from_value::<RunCommandArguments>(request.arguments)
        .map_err(|error| format!("invalid run_command arguments: {error}"))?;
    let program = arguments.program.trim();
    if program.is_empty() {
        return Err("run_command.program cannot be empty".to_string());
    }
    let timeout_ms = arguments
        .timeout_ms
        .unwrap_or(DEFAULT_TOOL_TIMEOUT_MS)
        .min(MAX_TOOL_TIMEOUT_MS);
    let cwd = resolve_tool_cwd(arguments.cwd, project)?;
    let project_id = project
        .map(|project| project.id.clone())
        .or_else(|| cwd.as_ref().map(|cwd| cwd.display().to_string()));

    Ok(ToolExecutionRequest {
        tool_call_id: request.tool_call_id,
        run_id: request.run_id,
        project_id,
        cwd,
        command: ToolCommand {
            program: program.to_string(),
            args: arguments.args,
            env: Default::default(),
        },
        timeout_ms: Some(timeout_ms),
        output_policy: ToolOutputPolicy::default(),
    })
}

fn resolve_tool_cwd(
    requested_cwd: Option<PathBuf>,
    project: Option<&ToolProjectContext>,
) -> std::result::Result<Option<PathBuf>, String> {
    let Some(project) = project else {
        return Ok(requested_cwd);
    };

    let root = canonicalize_existing_directory(&project.root, "project root")?;
    let cwd = match requested_cwd {
        Some(cwd) if cwd.is_absolute() => cwd,
        Some(cwd) => root.join(cwd),
        None => root.clone(),
    };
    let cwd = canonicalize_existing_directory(&cwd, "run_command.cwd")?;

    if !path_within(&cwd, &root) {
        return Err(format!(
            "run_command.cwd must stay inside the active project root: {}",
            root.display()
        ));
    }

    Ok(Some(cwd))
}

fn canonicalize_existing_directory(
    path: &Path,
    label: &str,
) -> std::result::Result<PathBuf, String> {
    let canonical =
        fs::canonicalize(path).map_err(|error| format!("{label} is not available: {error}"))?;
    if !canonical.is_dir() {
        return Err(format!("{label} must be a directory"));
    }
    Ok(canonical)
}

fn path_within(path: &Path, root: &Path) -> bool {
    if path == root || path.starts_with(root) {
        return true;
    }

    let path = normalize_for_compare(path);
    let root = normalize_for_compare(root);
    path == root
        || path
            .strip_prefix(&root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn normalize_for_compare(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");

    #[cfg(windows)]
    {
        return normalized.to_ascii_lowercase();
    }

    #[cfg(not(windows))]
    {
        normalized
    }
}

fn format_tool_result(result: &ToolExecutionResult) -> String {
    let mut content = String::new();
    content.push_str(&format!("status: {}\n", status_name(result.status)));
    if let Some(exit_code) = result.exit_code {
        content.push_str(&format!("exit_code: {exit_code}\n"));
    }
    if let Some(message) = result.message.as_deref() {
        if !message.trim().is_empty() {
            content.push_str(&format!("message: {message}\n"));
        }
    }
    if !result.stdout_tail.is_empty() {
        content.push_str("\nstdout_tail:\n");
        content.push_str(&result.stdout_tail);
        content.push('\n');
    }
    if !result.stderr_tail.is_empty() {
        content.push_str("\nstderr_tail:\n");
        content.push_str(&result.stderr_tail);
        content.push('\n');
    }
    if let Some(log_ref) = result.log_ref.as_deref() {
        content.push_str(&format!("\nfull_output_log: {log_ref}\n"));
    }
    if result.truncated_for_agent {
        content.push_str(
            "\noutput_note: output was truncated for the model; use full_output_log if needed\n",
        );
    }
    truncate_for_model(content)
}

fn status_name(status: ToolExecutionStatus) -> &'static str {
    match status {
        ToolExecutionStatus::Completed => "completed",
        ToolExecutionStatus::Failed => "failed",
        ToolExecutionStatus::Cancelled => "cancelled",
        ToolExecutionStatus::TimedOut => "timed_out",
        ToolExecutionStatus::PermissionDenied => "permission_denied",
        ToolExecutionStatus::LoopBlocked => "loop_blocked",
    }
}

fn truncate_for_model(mut content: String) -> String {
    if content.len() <= MAX_TOOL_RESULT_CHARS {
        return content;
    }
    content.truncate(MAX_TOOL_RESULT_CHARS);
    content.push_str("\n[tool result truncated]\n");
    content
}

struct SpawnedProcessAdapter {
    inner: Box<dyn process_sandbox::SpawnedProcess>,
}

#[async_trait::async_trait]
impl SpawnedToolProcess for SpawnedProcessAdapter {
    fn pid(&self) -> u32 {
        self.inner.pid()
    }

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        self.inner.take_stdout()
    }

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        self.inner.take_stderr()
    }

    async fn wait(&mut self) -> Result<ToolProcessExit> {
        self.inner
            .wait()
            .await
            .map(|exit| ToolProcessExit { code: exit.code })
            .map_err(|error| MothershipError::Runtime(error.to_string()))
    }

    async fn kill_tree(&mut self) -> Result<()> {
        self.inner
            .kill_tree()
            .await
            .map_err(|error| MothershipError::Runtime(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    /// A [`FileToolSpill`] that records what it was asked to spill and returns a
    /// fixed reference, so tests can assert the full (untruncated) diff was sent
    /// to the output store.
    #[derive(Default)]
    struct RecordingSpill {
        captured: Mutex<Vec<String>>,
    }

    impl FileToolSpill for RecordingSpill {
        fn spill(&self, _tool_call_id: &str, content: &str) -> std::io::Result<String> {
            self.captured.lock().unwrap().push(content.to_string());
            Ok("spill://full-diff".to_string())
        }
    }

    fn outcome_with_diff(diff: String) -> FileToolOutcome {
        FileToolOutcome {
            ok: true,
            model_text: "modified big.txt (1 bytes, sha256 abcd)".to_string(),
            data: json!({ "status": "modified" }),
            sha256: Some("abcd".to_string()),
            diff: Some(diff),
        }
    }

    #[test]
    fn bounded_event_payload_truncates_large_diff_and_spills_full() {
        // A diff far larger than MAX_TOOL_EVENT_BYTES must be truncated for the
        // event/result and the full diff written to the spill with a logRef.
        let big_diff = "+".repeat(MAX_TOOL_EVENT_BYTES * 3);
        let outcome = outcome_with_diff(big_diff.clone());
        let spill = RecordingSpill::default();

        let bounded = bound_event_payload(&outcome, "tc_big", Some(&spill));

        assert!(bounded.truncated, "oversized diff must be flagged truncated");
        let event_diff = bounded.diff.expect("diff present");
        assert!(
            event_diff.len() < big_diff.len(),
            "event diff must be smaller than the full diff"
        );
        assert!(
            event_diff.len() <= MAX_TOOL_EVENT_BYTES + 256,
            "event diff must be bounded near the budget, got {} bytes",
            event_diff.len()
        );
        assert!(event_diff.contains("diff truncated for storage"));
        assert_eq!(bounded.log_ref.as_deref(), Some("spill://full-diff"));
        // The result text must also be bounded (not carry the full diff).
        assert!(bounded.result_text.len() < big_diff.len());
        // The FULL diff reached the spill untouched.
        let captured = spill.captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].len(), big_diff.len());
    }

    #[test]
    fn bounded_event_payload_passes_small_diff_through() {
        let outcome = outcome_with_diff("@@ -1,1 +1,1 @@\n-old\n+new\n".to_string());
        let spill = RecordingSpill::default();

        let bounded = bound_event_payload(&outcome, "tc_small", Some(&spill));

        assert!(!bounded.truncated);
        assert!(bounded.log_ref.is_none(), "small diff must not spill");
        assert!(bounded.diff.unwrap().contains("+new"));
        assert!(spill.captured.lock().unwrap().is_empty());
    }

    #[test]
    fn bound_preview_truncates_large_diff_with_marker() {
        // A preview diff far larger than the event budget (as a write over a huge
        // file would produce) must be bounded before it reaches the persisted
        // PermissionRequested message.
        let big_diff = "+".repeat(MAX_TOOL_EVENT_BYTES * 4);
        let full = big_diff.len();

        let preview = bound_preview("write big.txt", &big_diff);

        assert!(
            preview.len() <= MAX_TOOL_EVENT_BYTES + 256,
            "preview must be bounded near the event budget, got {} bytes",
            preview.len()
        );
        assert!(preview.starts_with("write big.txt"));
        assert!(preview.contains("preview truncated"));
        assert!(
            preview.contains(&full.to_string()),
            "the marker must report the full byte size"
        );
        assert!(preview.contains("approve to apply the full change"));
    }

    #[test]
    fn bound_preview_passes_small_diff_through_unchanged() {
        let diff = "@@ -1,1 +1,1 @@\n-old\n+new\n";
        let preview = bound_preview("write a.txt", diff);
        assert_eq!(preview, format!("write a.txt\n\n{diff}"));
        assert!(!preview.contains("preview truncated"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_pwd_uses_powershell_builtin() {
        let spec = platform_process_spec(ToolProcessSpec {
            program: OsString::from("pwd"),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            timeout: Some(Duration::from_secs(1)),
        });

        assert_eq!(spec.program, OsString::from("powershell.exe"));
        assert!(spec
            .args
            .iter()
            .any(|arg| arg.to_string_lossy().contains("Get-Location")));
    }

    #[test]
    fn project_tool_cwd_defaults_to_project_root() {
        let root = temp_project_dir("default_cwd");
        let project = ToolProjectContext {
            id: "project_default".to_string(),
            root: root.clone(),
        };

        let cwd = resolve_tool_cwd(None, Some(&project))
            .expect("resolve cwd")
            .expect("cwd");

        assert_eq!(cwd, fs::canonicalize(root).expect("canonical root"));
    }

    #[test]
    fn project_tool_cwd_accepts_relative_subdirectory() {
        let root = temp_project_dir("relative_cwd");
        fs::create_dir_all(root.join("src")).expect("create subdir");
        let project = ToolProjectContext {
            id: "project_relative".to_string(),
            root: root.clone(),
        };

        let cwd = resolve_tool_cwd(Some(PathBuf::from("src")), Some(&project))
            .expect("resolve cwd")
            .expect("cwd");

        assert_eq!(
            cwd,
            fs::canonicalize(root.join("src")).expect("canonical subdir")
        );
    }

    #[test]
    fn project_tool_cwd_rejects_paths_outside_project() {
        let root = temp_project_dir("outside_cwd");
        let outside = temp_project_dir("outside_target");
        let project = ToolProjectContext {
            id: "project_outside".to_string(),
            root,
        };

        let error =
            resolve_tool_cwd(Some(outside), Some(&project)).expect_err("outside cwd must fail");

        assert!(error.contains("inside the active project root"));
    }

    fn temp_project_dir(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mothership_tool_{name}_{stamp}"));
        fs::create_dir_all(&path).expect("create temp project");
        path
    }
}
