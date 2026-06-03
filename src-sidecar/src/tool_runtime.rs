use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use mothership_core::{
    check_write_file_content_precondition, classify_file_tool, file_tool_preview_diff,
    run_apply_patch_tool, run_edit_file_tool, run_list_files_tool, run_read_file_tool,
    run_search_text_tool, run_write_file_tool_with_limit_and_observation,
    validate_file_tool_args_shallow, DEFAULT_MAX_WRITE_FILE_BYTES, tool_batch_plan,
    ChatCancellationToken, FileTool, FileToolOutcome, FileToolSpill, LlmToolCallHandler,
    LlmToolCallRequest, LlmToolCallResult, MothershipError, ApprovalPreview, BackendOutcome,
    PendingToolApprovalGate, ResourceLease, ResourceRequest, Result, SpawnedToolProcess,
    StdFileSystem, ToolApprovalGate, ToolArtifact, ToolBackend,
    ToolBatchPlan, ToolCallContext, ToolCancellationToken, ToolCapability, ToolCommand, ToolDecision,
    ToolOrchestrator, ToolExecutionEventSink, ToolExecutionRegistry, ToolExecutionRequest,
    ToolExecutionResult, ToolExecutionStatus, ToolExecutor, ToolKind, ToolOutputPolicy,
    ToolOutputStore, ToolPermissionAction, ToolProcessExit, ToolProcessSandbox, ToolProcessSpec,
    ToolSupervisor, Workspace, MAX_TOOL_EVENT_BYTES,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncRead;

const DEFAULT_TOOL_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const MAX_TOOL_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const CHAT_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_TOOL_RESULT_CHARS: usize = 160 * 1024;
const MAX_FILE_OBSERVATIONS: usize = 4096;

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
struct ToolProjectContext {
    id: String,
    root: PathBuf,
}

// ---------------------------------------------------------------------------
// Executors
// ---------------------------------------------------------------------------

/// Process-command executor. Wraps the unchanged [`ToolSupervisor`], which owns
/// the full command lifecycle (queued → permission → approval → spawn → stream →
/// terminal). The dispatcher supplies the per-call cancellation token and the
/// shared registry slot; this executor only adapts arguments and maps the
/// supervisor's result back to the model-facing response.
struct CommandToolExecutor {
    supervisor: Arc<ToolSupervisor>,
    runtime: Arc<tokio::runtime::Runtime>,
    sink: Arc<dyn ToolExecutionEventSink>,
    project: Option<ToolProjectContext>,
    /// Drives the same approval gate the file tools use; the orchestrator blocks
    /// on it during the Ask phase.
    approvals: Arc<PendingToolApprovalGate>,
}

/// Typed file/search executor. Runs the shared classify → permission → approval
/// → started → execute → bound/spill → terminal lifecycle around the pure Core
/// handlers. It mirrors the command lifecycle's cross-cuts (the same approval
/// gate, cancellation token, and event sink) but the "execute" step calls an
/// in-process handler instead of spawning a process.
struct FileToolExecutor {
    runtime: Arc<tokio::runtime::Runtime>,
    sink: Arc<dyn ToolExecutionEventSink>,
    project: Option<ToolProjectContext>,
    approvals: Arc<PendingToolApprovalGate>,
    file_system: StdFileSystem,
    output_store: Option<Arc<dyn ToolOutputStore>>,
    /// Policy ceiling on `write_file` content size, applied to BOTH the
    /// approval preview and the actual write (the composition root sets it; the
    /// default is [`DEFAULT_MAX_WRITE_FILE_BYTES`]).
    max_write_bytes: usize,
    observations: Arc<Mutex<HashMap<FileObservationKey, String>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileObservationKey {
    run_id: String,
    path: PathBuf,
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// The sidecar's [`LlmToolCallHandler`]: a thin typed dispatcher over the two
/// executors. It owns the cross-cuts shared by every tool call — the per-call
/// cancellation slot synced to the chat token, the registry slot that rejects a
/// duplicate active call, and the watcher thread that cancels on chat
/// cancellation — then routes by [`ToolKind`] to the right executor.
#[derive(Clone)]
pub struct SidecarLlmToolHandler {
    command_executor: Arc<CommandToolExecutor>,
    file_executor: Arc<FileToolExecutor>,
    registry: Arc<ToolExecutionRegistry>,
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
        let project = project.map(|(id, root)| ToolProjectContext { id, root });
        let command_executor = Arc::new(CommandToolExecutor {
            supervisor,
            runtime: Arc::clone(&runtime),
            sink: Arc::clone(&sink),
            project: project.clone(),
            approvals: Arc::clone(&approvals),
        });
        let file_executor = Arc::new(FileToolExecutor {
            runtime,
            sink,
            project,
            approvals,
            file_system: StdFileSystem::new(),
            output_store,
            max_write_bytes: DEFAULT_MAX_WRITE_FILE_BYTES,
            observations: Arc::new(Mutex::new(HashMap::new())),
        });
        Self {
            command_executor,
            file_executor,
            registry,
        }
    }

    /// Run the shared lifecycle scaffolding around one executor call: a per-call
    /// cancellation slot synced to the chat token, a registry slot (rejecting a
    /// duplicate active call), and a watcher thread that flips the per-call token
    /// when the chat is cancelled — then dispatch by [`ToolKind`].
    fn dispatch(
        &self,
        kind: ToolKind,
        request: &LlmToolCallRequest,
        chat_cancellation: &ChatCancellationToken,
    ) -> LlmToolCallResult {
        let cancellation = ToolCancellationToken::default();
        if chat_cancellation.is_cancelled() {
            cancellation.cancel();
        }
        if !self
            .registry
            .register(&request.tool_call_id, cancellation.clone())
        {
            return LlmToolCallResult {
                ok: false,
                content: format!("tool call already active: {}", request.tool_call_id),
            };
        }

        // Watch the chat token so a chat cancelled while a tool is awaiting
        // approval (or running) flips the per-call token and unblocks it.
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

        let ctx = ToolCallContext {
            tool_call_id: request.tool_call_id.as_str(),
            run_id: request.run_id.as_deref(),
            tool_name: request.name.as_str(),
            kind,
            arguments: &request.arguments,
            cancellation: &cancellation,
            chat_cancellation,
        };
        let result = if kind.is_process() {
            self.command_executor.execute(ctx)
        } else {
            // FileToolExecutor now implements both ToolExecutor (this dispatcher
            // seam) and ToolBackend (the orchestrator seam); name the trait.
            ToolExecutor::execute(&*self.file_executor, ctx)
        };

        finished.store(true, Ordering::SeqCst);
        let _ = watcher.join();
        self.registry.finish(&request.tool_call_id);
        result
    }
}

impl LlmToolCallHandler for SidecarLlmToolHandler {
    fn handle_tool_call(
        &self,
        request: LlmToolCallRequest,
        chat_cancellation: &ChatCancellationToken,
    ) -> LlmToolCallResult {
        match ToolKind::from_name(&request.name) {
            Some(kind) => self.dispatch(kind, &request, chat_cancellation),
            None => LlmToolCallResult {
                ok: false,
                content: format!("unsupported tool `{}`", request.name),
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

impl ToolExecutor for CommandToolExecutor {
    /// Thin adapter onto the unified [`ToolOrchestrator`]: build the command
    /// request, wrap it in a per-call [`CommandCall`] backend, and drive the
    /// shared lifecycle. The orchestrator owns Queued/Started/terminal events,
    /// the repeat guard, approval, and the resource lease; the backend supplies
    /// only the command-specific work via the supervisor's ports.
    fn execute(&self, ctx: ToolCallContext<'_>) -> LlmToolCallResult {
        let request = match command_request_from_ctx(&ctx, self.project.as_ref()) {
            Ok(request) => request,
            Err(error) => {
                return LlmToolCallResult {
                    ok: false,
                    content: error,
                };
            }
        };
        // Events carry the request's project id (project root, or the cwd
        // fallback) just as the former hand-rolled lifecycle did.
        let event_project_id = request.project_id.clone();
        let backend = CommandCall {
            supervisor: Arc::clone(&self.supervisor),
            runtime: Arc::clone(&self.runtime),
            request,
        };
        let gate: Arc<dyn ToolApprovalGate> = self.approvals.clone();
        let orchestrator = ToolOrchestrator::new(gate, Arc::clone(&self.runtime));
        orchestrator.run(ctx, event_project_id.as_deref(), &backend, &self.sink)
    }
}

/// Per-call command backend: the already-resolved [`ToolExecutionRequest`] plus
/// the shared supervisor/runtime, adapting the supervisor's command ports to the
/// [`ToolBackend`] seam the orchestrator drives. Standalone (owns its `Arc`s) so
/// both the chat dispatcher and the protocol-level `run_tool_command` build one.
struct CommandCall {
    supervisor: Arc<ToolSupervisor>,
    runtime: Arc<tokio::runtime::Runtime>,
    request: ToolExecutionRequest,
}

impl ToolBackend for CommandCall {
    fn classify(&self, _ctx: &ToolCallContext<'_>) -> Result<ToolCapability> {
        Ok(ToolCapability {
            summary: command_summary(&self.request),
            touched_paths: self
                .request
                .cwd
                .as_ref()
                .map(|cwd| vec![cwd.display().to_string()])
                .unwrap_or_default(),
            // Commands always take a concurrency/process lease (the orchestrator
            // acquires it after approval, before the process spawns).
            resource_request: Some(ResourceRequest { needs_lease: true }),
        })
    }

    fn guard(&self, _ctx: &ToolCallContext<'_>) -> Option<BackendOutcome> {
        self.supervisor
            .command_repeat_block(&self.request)
            .map(|block| {
                command_backend_outcome(synthesized_command_result(
                    &self.request,
                    ToolExecutionStatus::LoopBlocked,
                    block.message,
                ))
            })
    }

    fn decide(&self, _ctx: &ToolCallContext<'_>, _capability: &ToolCapability) -> ToolDecision {
        let evaluation = self.supervisor.command_permission(&self.request);
        match evaluation.action {
            ToolPermissionAction::Allow => ToolDecision::Allow,
            ToolPermissionAction::Ask => ToolDecision::Ask {
                reason: evaluation.reason,
            },
            ToolPermissionAction::Deny => ToolDecision::Deny {
                reason: evaluation.reason,
            },
        }
    }

    fn acquire(
        &self,
        ctx: &ToolCallContext<'_>,
        sink: &Arc<dyn ToolExecutionEventSink>,
    ) -> Result<ResourceLease> {
        self.runtime.block_on(self.supervisor.acquire_command_resources(
            &self.request,
            ctx.cancellation,
            sink,
        ))
    }

    fn execute(
        &self,
        ctx: &ToolCallContext<'_>,
        sink: &Arc<dyn ToolExecutionEventSink>,
    ) -> Result<BackendOutcome> {
        let result = self.runtime.block_on(self.supervisor.run_command_process(
            &self.request,
            ctx.cancellation,
            sink,
        ))?;
        Ok(command_backend_outcome(result))
    }

    fn record(&self, _ctx: &ToolCallContext<'_>, outcome: &BackendOutcome) {
        self.supervisor
            .record_command_outcome(&self.request, &outcome.result);
    }
}

/// `program arg1 arg2 …` — the human-readable command summary used as the call's
/// intent (the Started event message and approval-card summary).
fn command_summary(request: &ToolExecutionRequest) -> String {
    let mut parts = Vec::with_capacity(1 + request.command.args.len());
    parts.push(request.command.program.as_str());
    parts.extend(request.command.args.iter().map(String::as_str));
    parts.join(" ")
}

/// Wrap a command [`ToolExecutionResult`] as the orchestrator's terminal outcome:
/// the model sees the formatted text; the typed result carries the log ref.
fn command_backend_outcome(result: ToolExecutionResult) -> BackendOutcome {
    BackendOutcome {
        status: result.status,
        model_text: format_tool_result(&result),
        payload: None,
        touched_paths: Vec::new(),
        artifacts: Vec::new(),
        result,
    }
}

/// A bare command result carrying only a status + message (no output) — used for
/// the repeat-guard `LoopBlocked` terminal.
fn synthesized_command_result(
    request: &ToolExecutionRequest,
    status: ToolExecutionStatus,
    message: String,
) -> ToolExecutionResult {
    ToolExecutionResult {
        tool_call_id: request.tool_call_id.clone(),
        status,
        exit_code: None,
        stdout_preview: String::new(),
        stderr_preview: String::new(),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        stdout_bytes: 0,
        stderr_bytes: 0,
        truncated_for_display: false,
        truncated_for_agent: false,
        log_ref: None,
        message: Some(message),
    }
}

/// Drive one protocol-level `run_command` through the unified [`ToolOrchestrator`]
/// (Queued → policy/approval → lease → process → terminal). Synchronous: the
/// caller MUST invoke it on a dedicated OS thread, never an async-runtime worker,
/// because the orchestrator blocks on `runtime` for approval and process I/O.
/// Used by the `RunToolCommand` protocol handler; the chat dispatcher uses
/// [`CommandToolExecutor`] for the same lifecycle.
pub fn run_command_via_orchestrator(
    request: ToolExecutionRequest,
    cancellation: ToolCancellationToken,
    supervisor: Arc<ToolSupervisor>,
    approvals: Arc<PendingToolApprovalGate>,
    runtime: Arc<tokio::runtime::Runtime>,
    sink: Arc<dyn ToolExecutionEventSink>,
) {
    // No chat context here, so a fresh (never-cancelled) chat token.
    let chat_cancellation = ChatCancellationToken::default();
    let event_project_id = request.project_id.clone();
    let backend = CommandCall {
        supervisor,
        runtime: Arc::clone(&runtime),
        request,
    };
    let ctx = ToolCallContext {
        tool_call_id: &backend.request.tool_call_id,
        run_id: backend.request.run_id.as_deref(),
        tool_name: ToolKind::RunCommand.as_str(),
        kind: ToolKind::RunCommand,
        // CommandCall reads `self.request`, not `ctx.arguments`.
        arguments: &Value::Null,
        cancellation: &cancellation,
        chat_cancellation: &chat_cancellation,
    };
    let gate: Arc<dyn ToolApprovalGate> = approvals;
    let orchestrator = ToolOrchestrator::new(gate, runtime);
    let _ = orchestrator.run(ctx, event_project_id.as_deref(), &backend, &sink);
}

impl ToolExecutor for FileToolExecutor {
    /// Execute one of the typed file/search tools. Mirrors the command lifecycle's
    /// cross-cuts — the same `PermissionRequested` → approval-gate →
    /// `PermissionDenied` flow, cancellation honoring, and event/persistence
    /// plumbing — but the "execute" step calls a pure Core handler instead of
    /// spawning a process. The per-call cancellation slot, registry slot, and
    /// chat-cancellation watcher are owned by the dispatcher; this method only
    /// resolves the project/workspace and runs the in-process lifecycle.
    fn execute(&self, ctx: ToolCallContext<'_>) -> LlmToolCallResult {
        let Some(tool) = ctx.kind.file_tool() else {
            return LlmToolCallResult {
                ok: false,
                content: format!("not a file tool: {}", ctx.tool_name),
            };
        };

        // File tools require a known project root to resolve + contain paths.
        let Some(project) = self.project.clone() else {
            return LlmToolCallResult {
                ok: false,
                content: "file tools require an active project; none is associated with this chat"
                    .to_string(),
            };
        };
        let project_id = Some(project.id.clone());
        let run_id = ctx.run_id.map(str::to_string);

        let workspace = match Workspace::new(&project.root) {
            Ok(workspace) => workspace,
            Err(error) => {
                return LlmToolCallResult {
                    ok: false,
                    content: format!("project root is unavailable: {error}"),
                };
            }
        };

        self.run_file_tool_inner(
            tool,
            ctx.arguments,
            &workspace,
            ctx.tool_call_id,
            &run_id,
            &project_id,
            ctx.cancellation,
            ctx.chat_cancellation,
        )
    }
}

/// The typed file/search backend for the unified [`ToolOrchestrator`]. Splits the
/// former hand-rolled lifecycle into the orchestrator's phases: classify (intent
/// + touched paths), preflight (shallow arg validation), decide (path policy +
/// the write-observation precondition, surfaced as a typed `Reject`), preview
/// (approval diff), and execute (the pure handler + bounding/spill + read
/// observation recording). The orchestrator owns all event emission.
impl ToolBackend for FileToolExecutor {
    fn classify(&self, ctx: &ToolCallContext<'_>) -> Result<ToolCapability> {
        let (tool, workspace) = self.tool_and_workspace(ctx)?;
        let capability = classify_file_tool(tool, ctx.arguments, &workspace)
            .map_err(|error| MothershipError::InvalidRequest(error.to_string()))?;
        Ok(ToolCapability {
            summary: capability.summary,
            touched_paths: capability.touched_paths,
            // File/search tools need no resource lease.
            resource_request: None,
        })
    }

    fn preflight(&self, ctx: &ToolCallContext<'_>) -> Result<()> {
        let Some(tool) = ctx.kind.file_tool() else {
            return Ok(());
        };
        validate_file_tool_args_shallow(tool, ctx.arguments)
            .map_err(|error| MothershipError::InvalidRequest(error.to_string()))
    }

    fn decide(&self, ctx: &ToolCallContext<'_>, _capability: &ToolCapability) -> ToolDecision {
        let (tool, workspace) = match self.tool_and_workspace(ctx) {
            Ok(value) => value,
            Err(error) => {
                return ToolDecision::Deny {
                    reason: error.to_string(),
                }
            }
        };
        let capability = match classify_file_tool(tool, ctx.arguments, &workspace) {
            Ok(capability) => capability,
            Err(error) => {
                return ToolDecision::Deny {
                    reason: error.to_string(),
                }
            }
        };
        if capability.action == ToolPermissionAction::Deny {
            return ToolDecision::Deny {
                reason: capability.summary,
            };
        }

        // Write-observation precondition: refuse a blind overwrite BEFORE approval,
        // carrying the typed failure payload via a Reject.
        if tool == FileTool::Write {
            let run_id = ctx.run_id.map(str::to_string);
            let observed = self.observed_sha_for_write(ctx.arguments, &workspace, &run_id);
            match check_write_file_content_precondition(
                ctx.arguments,
                &workspace,
                &self.file_system,
                observed.as_deref(),
            ) {
                Ok(Some(failure)) => {
                    return ToolDecision::Reject(Box::new(file_failure_outcome(
                        failure,
                        ctx.tool_call_id,
                        Vec::new(),
                    )));
                }
                Ok(None) => {}
                Err(error) => {
                    let mut outcome = bare_failure_outcome(ctx.tool_call_id, error.to_string());
                    outcome.touched_paths = capability.touched_paths;
                    return ToolDecision::Reject(Box::new(outcome));
                }
            }
        }

        match capability.action {
            ToolPermissionAction::Ask => ToolDecision::Ask {
                reason: capability.summary,
            },
            ToolPermissionAction::Allow => ToolDecision::Allow,
            ToolPermissionAction::Deny => unreachable!("deny handled above"),
        }
    }

    fn preview(&self, ctx: &ToolCallContext<'_>, _capability: &ToolCapability) -> ApprovalPreview {
        let Ok((tool, workspace)) = self.tool_and_workspace(ctx) else {
            return ApprovalPreview::default();
        };
        let Ok(capability) = classify_file_tool(tool, ctx.arguments, &workspace) else {
            return ApprovalPreview::default();
        };
        let (message, artifacts) =
            self.build_preview_parts(tool, ctx.arguments, &workspace, &capability.summary);
        ApprovalPreview { message, artifacts }
    }

    fn execute(
        &self,
        ctx: &ToolCallContext<'_>,
        _sink: &Arc<dyn ToolExecutionEventSink>,
    ) -> Result<BackendOutcome> {
        let (tool, workspace) = self.tool_and_workspace(ctx)?;
        let tool_call_id = ctx.tool_call_id;
        let run_id = ctx.run_id.map(str::to_string);

        let observed_write_sha = if tool == FileTool::Write {
            self.observed_sha_for_write(ctx.arguments, &workspace, &run_id)
        } else {
            None
        };

        let spill = self.output_store.as_ref().map(|store| AsyncOutputStoreSpill {
            store: Arc::clone(store),
            runtime: Arc::clone(&self.runtime),
        });
        let spill_ref = spill.as_ref().map(|spill| spill as &dyn FileToolSpill);
        let outcome = match tool {
            FileTool::Read => run_read_file_tool(
                ctx.arguments,
                &workspace,
                &self.file_system,
                tool_call_id,
                spill_ref,
            ),
            FileTool::Write => run_write_file_tool_with_limit_and_observation(
                ctx.arguments,
                &workspace,
                &self.file_system,
                self.max_write_bytes,
                observed_write_sha.as_deref(),
            ),
            FileTool::Edit => run_edit_file_tool(ctx.arguments, &workspace, &self.file_system),
            FileTool::ApplyPatch => {
                run_apply_patch_tool(ctx.arguments, &workspace, &self.file_system)
            }
            FileTool::ListFiles => {
                run_list_files_tool(ctx.arguments, &workspace, tool_call_id, spill_ref)
            }
            FileTool::SearchText => run_search_text_tool(
                ctx.arguments,
                &workspace,
                &self.file_system,
                tool_call_id,
                spill_ref,
            ),
        };
        let outcome = outcome.map_err(|error| MothershipError::Runtime(error.to_string()))?;
        self.record_read_observation(tool, &outcome, &workspace, &run_id);

        // Bound the event/DB payload (the full diff spills to a logRef) and shape
        // the terminal outcome the orchestrator will emit.
        let status = if outcome.ok {
            ToolExecutionStatus::Completed
        } else {
            ToolExecutionStatus::Failed
        };
        let model_text = truncate_for_model(file_outcome_text(&outcome));
        let bounded = bound_event_payload(&outcome, tool_call_id, spill_ref);
        let mut artifacts = Vec::new();
        if let Some(diff_preview) = bounded.diff.clone() {
            let full_len = outcome
                .diff
                .as_deref()
                .map(str::len)
                .unwrap_or(diff_preview.len());
            artifacts.push(ToolArtifact {
                artifact_id: "diff".to_string(),
                kind: "diff".to_string(),
                content_type: "text/x-diff".to_string(),
                preview: diff_preview,
                log_ref: bounded.log_ref.clone(),
                size_bytes: full_len as u64,
                sha256: None,
                truncated: bounded.truncated,
            });
        }
        let mut result =
            synthesized_result(tool_call_id, status, bounded.result_text, bounded.diff.clone());
        result.log_ref = bounded.log_ref;
        result.truncated_for_display = bounded.truncated;

        Ok(BackendOutcome {
            status,
            result,
            model_text,
            payload: Some(outcome.data.clone()),
            touched_paths: touched_paths_from_data(&outcome.data),
            artifacts,
        })
    }
}

/// Build a terminal [`BackendOutcome`] from a file handler's failure outcome
/// (carrying its typed payload), for a pre-approval reject.
fn file_failure_outcome(
    outcome: FileToolOutcome,
    tool_call_id: &str,
    extra_touched: Vec<String>,
) -> BackendOutcome {
    let message = file_outcome_text(&outcome);
    let mut touched_paths = touched_paths_from_data(&outcome.data);
    touched_paths.extend(extra_touched);
    BackendOutcome {
        status: ToolExecutionStatus::Failed,
        result: synthesized_result(
            tool_call_id,
            ToolExecutionStatus::Failed,
            String::new(),
            Some(message.clone()),
        ),
        model_text: message,
        payload: Some(outcome.data.clone()),
        touched_paths,
        artifacts: Vec::new(),
    }
}

/// A bare failed [`BackendOutcome`] carrying only a message (no typed payload).
fn bare_failure_outcome(tool_call_id: &str, message: String) -> BackendOutcome {
    BackendOutcome {
        status: ToolExecutionStatus::Failed,
        result: synthesized_result(
            tool_call_id,
            ToolExecutionStatus::Failed,
            String::new(),
            Some(message.clone()),
        ),
        model_text: message,
        payload: None,
        touched_paths: Vec::new(),
        artifacts: Vec::new(),
    }
}

impl FileToolExecutor {
    fn observation_key_for_path(
        &self,
        run_id: &Option<String>,
        workspace: &Workspace,
        path: &str,
    ) -> Option<FileObservationKey> {
        let run_id = run_id.as_ref()?.clone();
        let resolved = workspace.resolve(path).ok()?;
        Some(FileObservationKey {
            run_id,
            path: resolved,
        })
    }

    fn observed_sha_for_write(
        &self,
        arguments: &Value,
        workspace: &Workspace,
        run_id: &Option<String>,
    ) -> Option<String> {
        let path = arguments.get("path").and_then(Value::as_str)?;
        let key = self.observation_key_for_path(run_id, workspace, path)?;
        self.observations.lock().ok()?.get(&key).cloned()
    }

    fn record_read_observation(
        &self,
        tool: FileTool,
        outcome: &FileToolOutcome,
        workspace: &Workspace,
        run_id: &Option<String>,
    ) {
        if tool != FileTool::Read || !is_complete_read_observation(outcome) {
            return;
        }
        let Some(path) = outcome.data.get("path").and_then(Value::as_str) else {
            return;
        };
        let Some(sha256) = outcome
            .sha256
            .as_deref()
            .or_else(|| outcome.data.get("sha256").and_then(Value::as_str))
        else {
            return;
        };
        let Some(key) = self.observation_key_for_path(run_id, workspace, path) else {
            return;
        };
        let Ok(mut observations) = self.observations.lock() else {
            return;
        };
        if observations.len() >= MAX_FILE_OBSERVATIONS {
            observations.clear();
        }
        observations.insert(key, sha256.to_string());
    }

    /// Resolve the file tool kind and the project workspace for this call. The
    /// workspace is rebuilt from the active project's root (the same source the
    /// dispatcher used); a missing project or unusable root surfaces as
    /// `InvalidRequest`.
    fn tool_and_workspace(&self, ctx: &ToolCallContext<'_>) -> Result<(FileTool, Workspace)> {
        let tool = ctx.kind.file_tool().ok_or_else(|| {
            MothershipError::InvalidRequest(format!("not a file tool: {}", ctx.tool_name))
        })?;
        let project = self.project.as_ref().ok_or_else(|| {
            MothershipError::InvalidRequest(
                "file tools require an active project; none is associated with this chat"
                    .to_string(),
            )
        })?;
        let workspace = Workspace::new(&project.root).map_err(|error| {
            MothershipError::InvalidRequest(format!("project root is unavailable: {error}"))
        })?;
        Ok((tool, workspace))
    }

    /// Thin adapter onto the unified [`ToolOrchestrator`]: build the call context
    /// and drive the shared lifecycle (queued -> classify -> preflight ->
    /// policy/approval -> started -> execute -> terminal) with `self` acting as the
    /// file [`ToolBackend`]. The orchestrator owns every event emission and the
    /// approval block; this method only shapes the context.
    #[allow(clippy::too_many_arguments)]
    fn run_file_tool_inner(
        &self,
        tool: FileTool,
        arguments: &Value,
        _workspace: &Workspace,
        tool_call_id: &str,
        run_id: &Option<String>,
        project_id: &Option<String>,
        cancellation: &ToolCancellationToken,
        chat_cancellation: &ChatCancellationToken,
    ) -> LlmToolCallResult {
        let kind = ToolKind::from(tool);
        let ctx = ToolCallContext {
            tool_call_id,
            run_id: run_id.as_deref(),
            tool_name: kind.as_str(),
            kind,
            arguments,
            cancellation,
            chat_cancellation,
        };
        let gate: Arc<dyn ToolApprovalGate> = self.approvals.clone();
        let orchestrator = ToolOrchestrator::new(gate, Arc::clone(&self.runtime));
        orchestrator.run(ctx, project_id.as_deref(), self, &self.sink)
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
    fn build_preview_parts(
        &self,
        tool: FileTool,
        arguments: &Value,
        workspace: &Workspace,
        summary: &str,
    ) -> (String, Vec<ToolArtifact>) {
        match file_tool_preview_diff(tool, arguments, workspace, &self.file_system, self.max_write_bytes)
        {
            Some(diff) if !diff.is_empty() => {
                // The message keeps the bounded summary+diff string for legacy /
                // fallback rendering, but the diff is ALSO emitted as a typed
                // `diff-preview` artifact so the approval card has a real protocol
                // object (persisted to tool_artifacts; sent to UI/remote) rather
                // than a string the UI has to re-parse.
                let message = bound_preview(summary, &diff);
                let bounded = truncate_on_char_boundary(&diff, MAX_TOOL_EVENT_BYTES);
                let truncated = bounded.len() < diff.len();
                let artifact = ToolArtifact {
                    artifact_id: "preview".to_string(),
                    kind: "diff-preview".to_string(),
                    content_type: "text/x-diff".to_string(),
                    preview: bounded,
                    log_ref: None,
                    size_bytes: diff.len() as u64,
                    sha256: None,
                    truncated,
                };
                (message, vec![artifact])
            }
            _ => (summary.to_string(), Vec::new()),
        }
    }
}

/// Extract the workspace-relative paths a file-tool outcome touched, from its
/// semantic `data` payload — a single `path`, and/or a `files` array (of strings
/// or `{ "path": … }` objects, as `apply_patch` emits).
fn touched_paths_from_data(data: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = data.get("path").and_then(Value::as_str) {
        paths.push(path.to_string());
    }
    if let Some(files) = data.get("files").and_then(Value::as_array) {
        for file in files {
            if let Some(path) = file.as_str() {
                paths.push(path.to_string());
            } else if let Some(path) = file.get("path").and_then(Value::as_str) {
                paths.push(path.to_string());
            }
        }
    }
    paths
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

fn is_complete_read_observation(outcome: &FileToolOutcome) -> bool {
    if !outcome.ok || outcome.sha256.is_none() {
        return false;
    }
    if outcome
        .data
        .get("bytesTruncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    if outcome
        .data
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }

    let start_line = outcome
        .data
        .get("startLine")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let end_line = outcome
        .data
        .get("endLine")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    let total_lines = outcome
        .data
        .get("totalLines")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);

    start_line == 1 && end_line == total_lines
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

fn command_request_from_ctx(
    ctx: &ToolCallContext<'_>,
    project: Option<&ToolProjectContext>,
) -> std::result::Result<ToolExecutionRequest, String> {
    let arguments = serde_json::from_value::<RunCommandArguments>(ctx.arguments.clone())
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
        tool_call_id: ctx.tool_call_id.to_string(),
        run_id: ctx.run_id.map(str::to_string),
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
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    // Event/approval types used only by these tests (the non-test lifecycle now
    // emits exclusively through the orchestrator, so the parent no longer needs
    // them).
    use mothership_core::{ToolApprovalDecision, ToolExecutionEvent, ToolExecutionEventKind};

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

    #[derive(Default)]
    struct RecordingEventSink {
        events: Mutex<Vec<ToolExecutionEvent>>,
    }

    impl ToolExecutionEventSink for RecordingEventSink {
        fn emit(&self, event: ToolExecutionEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn test_file_executor(
        root: &Path,
        sink: Arc<RecordingEventSink>,
        approvals: Arc<PendingToolApprovalGate>,
    ) -> FileToolExecutor {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime"),
        );
        let sink_for_executor: Arc<dyn ToolExecutionEventSink> = sink;
        FileToolExecutor {
            runtime,
            sink: sink_for_executor,
            project: Some(ToolProjectContext {
                id: "project_file_tool_test".to_string(),
                root: root.to_path_buf(),
            }),
            approvals,
            file_system: StdFileSystem::new(),
            output_store: None,
            max_write_bytes: DEFAULT_MAX_WRITE_FILE_BYTES,
            observations: Arc::new(Mutex::new(HashMap::new())),
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

    #[test]
    fn malformed_write_fails_preflight_without_permission_request() {
        let root = temp_project_dir("malformed_write_preflight");
        let workspace = Workspace::new(&root).expect("workspace");
        let sink = Arc::new(RecordingEventSink::default());
        let executor = test_file_executor(&root, sink.clone(), PendingToolApprovalGate::new());
        let cancellation = ToolCancellationToken::default();
        let chat_cancellation = ChatCancellationToken::default();
        let run_id = None;
        let project_id = Some("project_preflight".to_string());

        let result = executor.run_file_tool_inner(
            FileTool::Write,
            &json!({ "path": "out.txt" }),
            &workspace,
            "tc_malformed",
            &run_id,
            &project_id,
            &cancellation,
            &chat_cancellation,
        );

        assert!(!result.ok);
        assert!(result.content.contains("content"), "got: {}", result.content);
        let events = sink.events.lock().unwrap();
        // The orchestrator emits Queued, then the preflight-failure terminal.
        let terminal = events.last().expect("a terminal event");
        assert_eq!(terminal.kind, ToolExecutionEventKind::Failed);
        assert_eq!(
            terminal.result.as_ref().map(|result| result.status),
            Some(ToolExecutionStatus::Failed)
        );
        assert!(
            events.iter().all(|event| event.kind
                != ToolExecutionEventKind::PermissionRequested
                && event.kind != ToolExecutionEventKind::Started),
            "malformed write must not ask for approval or start execution"
        );
    }

    #[test]
    fn blind_existing_write_fails_before_permission_request() {
        let root = temp_project_dir("blind_write_precondition");
        fs::write(root.join("a.txt"), b"old\n").expect("seed file");
        let workspace = Workspace::new(&root).expect("workspace");
        let sink = Arc::new(RecordingEventSink::default());
        let executor = test_file_executor(&root, sink.clone(), PendingToolApprovalGate::new());
        let cancellation = ToolCancellationToken::default();
        let chat_cancellation = ChatCancellationToken::default();
        let run_id = Some("run_blind".to_string());
        let project_id = Some("project_precondition".to_string());

        let result = executor.run_file_tool_inner(
            FileTool::Write,
            &json!({ "path": "a.txt", "content": "new\n", "overwrite": true }),
            &workspace,
            "tc_blind",
            &run_id,
            &project_id,
            &cancellation,
            &chat_cancellation,
        );

        assert!(!result.ok);
        assert!(
            result.content.contains("refusing blind overwrite"),
            "got: {}",
            result.content
        );
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"old\n");
        let events = sink.events.lock().unwrap();
        // The orchestrator emits Queued, then the precondition-failure terminal
        // (no approval, no start).
        let terminal = events.last().expect("a terminal event");
        assert_eq!(terminal.kind, ToolExecutionEventKind::Failed);
        assert_eq!(
            terminal.payload.as_ref().and_then(|payload| payload.get("status")),
            Some(&json!("precondition_required"))
        );
        assert!(
            events
                .iter()
                .all(|event| event.kind != ToolExecutionEventKind::PermissionRequested
                    && event.kind != ToolExecutionEventKind::Started),
            "blind write must not ask approval or start execution"
        );
    }

    #[test]
    fn complete_read_observation_allows_existing_write_without_expected_sha() {
        let root = temp_project_dir("read_observation_write");
        fs::write(root.join("a.txt"), b"old\n").expect("seed file");
        let workspace = Workspace::new(&root).expect("workspace");
        let sink = Arc::new(RecordingEventSink::default());
        let approvals = PendingToolApprovalGate::new();
        let executor = test_file_executor(&root, sink.clone(), approvals.clone());
        let cancellation = ToolCancellationToken::default();
        let chat_cancellation = ChatCancellationToken::default();
        let run_id = Some("run_observed".to_string());
        let project_id = Some("project_precondition".to_string());

        let read_result = executor.run_file_tool_inner(
            FileTool::Read,
            &json!({ "path": "a.txt" }),
            &workspace,
            "tc_read_observed",
            &run_id,
            &project_id,
            &cancellation,
            &chat_cancellation,
        );
        assert!(read_result.ok, "{}", read_result.content);

        let workspace_for_thread = workspace.clone();
        let run_id_for_thread = run_id.clone();
        let project_id_for_thread = project_id.clone();
        let handle = thread::spawn(move || {
            executor.run_file_tool_inner(
                FileTool::Write,
                &json!({ "path": "a.txt", "content": "new\n" }),
                &workspace_for_thread,
                "tc_write_observed",
                &run_id_for_thread,
                &project_id_for_thread,
                &ToolCancellationToken::default(),
                &ChatCancellationToken::default(),
            )
        });

        let mut approved = false;
        for _ in 0..100 {
            if approvals.decide("tc_write_observed", ToolApprovalDecision::Approved) {
                approved = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(approved, "write approval was not registered");
        let write_result = handle.join().expect("write thread");

        assert!(write_result.ok, "{}", write_result.content);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"new\n");
        let events = sink.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.kind == ToolExecutionEventKind::PermissionRequested),
            "observed write should still request approval in normal mode"
        );
        assert!(
            events
                .iter()
                .any(|event| event.kind == ToolExecutionEventKind::Completed
                    && event.tool_call_id == "tc_write_observed"),
            "observed write should complete"
        );
    }

    #[test]
    fn partial_read_observation_does_not_allow_existing_write() {
        let root = temp_project_dir("partial_read_observation_write");
        fs::write(root.join("a.txt"), b"old\nsecond\n").expect("seed file");
        let workspace = Workspace::new(&root).expect("workspace");
        let sink = Arc::new(RecordingEventSink::default());
        let executor = test_file_executor(&root, sink.clone(), PendingToolApprovalGate::new());
        let cancellation = ToolCancellationToken::default();
        let chat_cancellation = ChatCancellationToken::default();
        let run_id = Some("run_partial_read".to_string());
        let project_id = Some("project_precondition".to_string());

        let read_result = executor.run_file_tool_inner(
            FileTool::Read,
            &json!({ "path": "a.txt", "limit": 1 }),
            &workspace,
            "tc_partial_read",
            &run_id,
            &project_id,
            &cancellation,
            &chat_cancellation,
        );
        assert!(read_result.ok, "{}", read_result.content);

        let write_result = executor.run_file_tool_inner(
            FileTool::Write,
            &json!({ "path": "a.txt", "content": "new\n" }),
            &workspace,
            "tc_write_after_partial",
            &run_id,
            &project_id,
            &cancellation,
            &chat_cancellation,
        );

        assert!(!write_result.ok);
        assert!(
            write_result.content.contains("refusing blind overwrite"),
            "got: {}",
            write_result.content
        );
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"old\nsecond\n");
        let events = sink.events.lock().unwrap();
        assert!(
            events.iter().filter(|event| event.tool_call_id == "tc_write_after_partial").all(
                |event| event.kind != ToolExecutionEventKind::PermissionRequested
                    && event.kind != ToolExecutionEventKind::Started
            ),
            "partial read must not authorize write approval/execution"
        );
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

    // --- run_command lifecycle (driven through the unified orchestrator) ------
    //
    // These mirror the former ToolSupervisor::run_command tests 1:1 (stream+spill,
    // deny-without-spawn, non-zero-exit, repeat-block) but exercise the real
    // production path: run_command_via_orchestrator → CommandCall → orchestrator →
    // supervisor command ports.

    use mothership_core::{FileToolOutputStore, ToolRepeatGuard, ToolResourceLimits};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn command_runtime() -> Arc<tokio::runtime::Runtime> {
        Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime"),
        )
    }

    fn event_kinds(sink: &RecordingEventSink) -> Vec<ToolExecutionEventKind> {
        sink.events
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.kind)
            .collect()
    }

    /// The most recent terminal event's result (Completed/Failed/LoopBlocked all
    /// carry one).
    fn last_terminal_result(sink: &RecordingEventSink) -> ToolExecutionResult {
        sink.events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|event| event.result.clone())
            .expect("a terminal result")
    }

    fn command_request(
        tool_call_id: &str,
        program: &str,
        args: &[&str],
        output_policy: ToolOutputPolicy,
    ) -> ToolExecutionRequest {
        ToolExecutionRequest {
            tool_call_id: tool_call_id.to_string(),
            run_id: Some("run_cmd".to_string()),
            project_id: Some("project_cmd".to_string()),
            cwd: None,
            command: ToolCommand::new(program, args.iter().copied()),
            timeout_ms: None,
            output_policy,
        }
    }

    struct FakeSandbox {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exit_code: Option<i32>,
        spawns: AtomicUsize,
    }

    impl FakeSandbox {
        fn new(stdout: Vec<u8>, stderr: Vec<u8>, exit_code: Option<i32>) -> Self {
            Self {
                stdout,
                stderr,
                exit_code,
                spawns: AtomicUsize::new(0),
            }
        }

        fn spawn_count(&self) -> usize {
            self.spawns.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ToolProcessSandbox for FakeSandbox {
        async fn spawn(&self, _spec: ToolProcessSpec) -> Result<Box<dyn SpawnedToolProcess>> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeProcess {
                stdout: Some(self.stdout.clone()),
                stderr: Some(self.stderr.clone()),
                exit_code: self.exit_code,
                killed: false,
            }))
        }
    }

    struct FakeProcess {
        stdout: Option<Vec<u8>>,
        stderr: Option<Vec<u8>>,
        exit_code: Option<i32>,
        killed: bool,
    }

    #[async_trait::async_trait]
    impl SpawnedToolProcess for FakeProcess {
        fn pid(&self) -> u32 {
            42
        }

        fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
            self.stdout.take().map(|bytes| {
                Box::new(std::io::Cursor::new(bytes)) as Box<dyn tokio::io::AsyncRead + Send + Unpin>
            })
        }

        fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
            self.stderr.take().map(|bytes| {
                Box::new(std::io::Cursor::new(bytes)) as Box<dyn tokio::io::AsyncRead + Send + Unpin>
            })
        }

        async fn wait(&mut self) -> Result<ToolProcessExit> {
            if self.killed {
                return Ok(ToolProcessExit { code: Some(1) });
            }
            Ok(ToolProcessExit {
                code: self.exit_code,
            })
        }

        async fn kill_tree(&mut self) -> Result<()> {
            self.killed = true;
            Ok(())
        }
    }

    #[test]
    fn run_command_streams_spills_and_returns_bounded_output() {
        let sandbox = Arc::new(FakeSandbox::new(
            b"abcdefghijklmnopqrstuvwxyz".to_vec(),
            b"warning".to_vec(),
            Some(0),
        ));
        let output_dir = temp_project_dir("cmd_stream_spill");
        let supervisor = Arc::new(ToolSupervisor::new(
            sandbox.clone(),
            Some(Arc::new(FileToolOutputStore::new(&output_dir))),
            ToolResourceLimits::default(),
        ));
        let sink = Arc::new(RecordingEventSink::default());
        let request = command_request(
            "tool_cmd_1",
            "git",
            &["status"],
            ToolOutputPolicy {
                memory_preview_bytes: 10,
                ui_stream_bytes_per_sec: 1024,
                agent_tail_bytes: 6,
                spill_to_file: true,
            },
        );
        let sink_dyn: Arc<dyn ToolExecutionEventSink> = sink.clone();
        run_command_via_orchestrator(
            request,
            ToolCancellationToken::default(),
            supervisor,
            PendingToolApprovalGate::new(),
            command_runtime(),
            sink_dyn,
        );

        let result = last_terminal_result(&sink);
        assert_eq!(result.status, ToolExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout_bytes, 26);
        assert!(result.truncated_for_display);
        assert!(result.truncated_for_agent);
        assert_eq!(result.stdout_tail, "uvwxyz");
        let log = fs::read_to_string(result.log_ref.expect("log ref")).unwrap();
        assert!(log.contains("--- stdout 26 bytes ---"));
        assert!(log.contains("abcdefghijklmnopqrstuvwxyz"));

        let kinds = event_kinds(&sink);
        assert!(kinds.contains(&ToolExecutionEventKind::Queued));
        assert!(kinds.contains(&ToolExecutionEventKind::Started));
        assert!(kinds.contains(&ToolExecutionEventKind::Output));
        assert!(kinds.contains(&ToolExecutionEventKind::Completed));
        let _ = fs::remove_dir_all(output_dir);
    }

    #[test]
    fn command_requiring_approval_does_not_spawn_when_denied() {
        let sandbox = Arc::new(FakeSandbox::new(Vec::new(), Vec::new(), Some(0)));
        let supervisor = Arc::new(ToolSupervisor::new(
            sandbox.clone(),
            None,
            ToolResourceLimits::default(),
        ));
        let approvals = PendingToolApprovalGate::new();
        let sink = Arc::new(RecordingEventSink::default());
        let request = command_request("tool_cmd_2", "npm", &["install"], ToolOutputPolicy::default());

        let handle = {
            let supervisor = Arc::clone(&supervisor);
            let approvals = Arc::clone(&approvals);
            let sink_dyn: Arc<dyn ToolExecutionEventSink> = sink.clone();
            thread::spawn(move || {
                run_command_via_orchestrator(
                    request,
                    ToolCancellationToken::default(),
                    supervisor,
                    approvals,
                    command_runtime(),
                    sink_dyn,
                );
            })
        };

        // npm install requires approval; wait for the request, then deny it.
        let mut denied = false;
        for _ in 0..400 {
            if approvals.decide(
                "tool_cmd_2",
                ToolApprovalDecision::Denied {
                    reason: "not approved".to_string(),
                },
            ) {
                denied = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        handle.join().expect("command thread");
        assert!(denied, "approval was never requested");

        assert_eq!(sandbox.spawn_count(), 0);
        let kinds = event_kinds(&sink);
        assert!(kinds.contains(&ToolExecutionEventKind::PermissionRequested));
        assert!(kinds.contains(&ToolExecutionEventKind::PermissionDenied));
    }

    #[test]
    fn non_zero_exit_is_a_failed_command_terminal() {
        let sandbox = Arc::new(FakeSandbox::new(b"bad".to_vec(), Vec::new(), Some(2)));
        let supervisor = Arc::new(ToolSupervisor::new(sandbox, None, ToolResourceLimits::default()));
        let sink = Arc::new(RecordingEventSink::default());
        let request = command_request("tool_cmd_3", "git", &["status"], ToolOutputPolicy::default());
        let sink_dyn: Arc<dyn ToolExecutionEventSink> = sink.clone();
        run_command_via_orchestrator(
            request,
            ToolCancellationToken::default(),
            supervisor,
            PendingToolApprovalGate::new(),
            command_runtime(),
            sink_dyn,
        );

        let result = last_terminal_result(&sink);
        assert_eq!(result.status, ToolExecutionStatus::Failed);
        assert_eq!(result.exit_code, Some(2));
        assert!(event_kinds(&sink).contains(&ToolExecutionEventKind::Failed));
    }

    #[test]
    fn repeat_guard_blocks_third_identical_command_without_spawning() {
        let sandbox = Arc::new(FakeSandbox::new(
            b"nothing to commit".to_vec(),
            Vec::new(),
            Some(0),
        ));
        let supervisor = Arc::new(
            ToolSupervisor::new(sandbox.clone(), None, ToolResourceLimits::default())
                .with_repeat_guard(Arc::new(ToolRepeatGuard::default())),
        );
        let runtime = command_runtime();
        let approvals = PendingToolApprovalGate::new();
        let sink = Arc::new(RecordingEventSink::default());

        for tool_call_id in ["tool_rep_1", "tool_rep_2", "tool_rep_3"] {
            let request =
                command_request(tool_call_id, "git", &["status"], ToolOutputPolicy::default());
            let sink_dyn: Arc<dyn ToolExecutionEventSink> = sink.clone();
            run_command_via_orchestrator(
                request,
                ToolCancellationToken::default(),
                Arc::clone(&supervisor),
                Arc::clone(&approvals),
                Arc::clone(&runtime),
                sink_dyn,
            );
        }

        assert_eq!(sandbox.spawn_count(), 2);
        let kinds = event_kinds(&sink);
        assert!(kinds.contains(&ToolExecutionEventKind::LoopBlocked));
        let result = last_terminal_result(&sink);
        assert_eq!(result.status, ToolExecutionStatus::LoopBlocked);
        assert!(result
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("Repeated identical tool call suppressed"));
    }
}
