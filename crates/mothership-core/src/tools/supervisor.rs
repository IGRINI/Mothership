use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify};

use crate::{MothershipError, Result};

use super::cancellation::ToolCancellationToken;
use super::orchestrator::ResourceLease;
use super::output::{
    drain_stream, DrainedStream, SharedToolOutputWriter, ToolOutputContext, ToolOutputStore,
};
use super::permissions::{
    ConservativeCommandPermissionPolicy, ToolPermissionEvaluation, ToolPermissionPolicy,
};
use super::process::{ToolProcessSandbox, ToolProcessSpec};
use super::repeat_guard::{ToolRepeatBlock, ToolRepeatGuard};
use super::resources::{ToolResourceGate, ToolResourceLimits};
use super::types::{
    event, run_command_typed_payload, ToolBackgroundCompletionSink, ToolExecutionEventKind,
    ToolExecutionEventSink, ToolExecutionRequest, ToolExecutionResult, ToolExecutionStatus,
};

/// The command "ports" the unified [`ToolOrchestrator`] drives: policy
/// (`command_permission`), repeat detection (`command_repeat_block`), the
/// resource lease (`acquire_command_resources`), the side effect itself
/// (`run_command_process` — spawn/stream/wait only), and bookkeeping
/// (`record_command_outcome`). Approval and lifecycle sequencing now live in the
/// orchestrator; this type no longer owns an approval gate or a `run_command`
/// monolith.
pub struct ToolSupervisor {
    sandbox: Arc<dyn ToolProcessSandbox>,
    permission_policy: Arc<dyn ToolPermissionPolicy>,
    resources: ToolResourceGate,
    output_store: Option<Arc<dyn ToolOutputStore>>,
    repeat_guard: Option<Arc<ToolRepeatGuard>>,
}

impl ToolSupervisor {
    pub fn new(
        sandbox: Arc<dyn ToolProcessSandbox>,
        output_store: Option<Arc<dyn ToolOutputStore>>,
        limits: ToolResourceLimits,
    ) -> Self {
        Self {
            sandbox,
            output_store,
            resources: ToolResourceGate::new(limits),
            permission_policy: Arc::new(ConservativeCommandPermissionPolicy),
            repeat_guard: None,
        }
    }

    pub fn with_policy(mut self, permission_policy: Arc<dyn ToolPermissionPolicy>) -> Self {
        self.permission_policy = permission_policy;
        self
    }

    pub fn with_repeat_guard(mut self, repeat_guard: Arc<ToolRepeatGuard>) -> Self {
        self.repeat_guard = Some(repeat_guard);
        self
    }

    /// Policy decision for a command (Allow / Ask / Deny). The orchestrator
    /// applies it; the supervisor only consults the configured policy.
    pub fn command_permission(&self, request: &ToolExecutionRequest) -> ToolPermissionEvaluation {
        self.permission_policy.evaluate(request)
    }

    /// Repeat-suppression check: a block when this command already returned the
    /// same result enough times in the run, else `None`.
    pub fn command_repeat_block(&self, request: &ToolExecutionRequest) -> Option<ToolRepeatBlock> {
        self.repeat_guard
            .as_ref()
            .and_then(|guard| guard.check_command(request))
    }

    /// Acquire the command's resource leases (global + per-project shell, plus a
    /// per-project git lease for git commands), emitting `WaitingForResource` at
    /// capacity. The returned [`ResourceLease`] holds the permits until dropped.
    pub async fn acquire_command_resources(
        &self,
        request: &ToolExecutionRequest,
        cancellation: &ToolCancellationToken,
        sink: &Arc<dyn ToolExecutionEventSink>,
    ) -> Result<ResourceLease> {
        let leases = self.resources.acquire(request, cancellation, sink).await?;
        Ok(ResourceLease::new(Box::new(leases)))
    }

    /// Run the command process: spawn, stream stdout/stderr (emitting `Output`
    /// events and spilling to the output store when requested), and wait with
    /// timeout/cancellation. Returns the bounded result. This is the only command
    /// step that touches a process — Queued/Started/terminal events, approval, the
    /// repeat guard, and the resource lease are all owned by the orchestrator.
    pub async fn run_command_process(
        &self,
        request: &ToolExecutionRequest,
        cancellation: &ToolCancellationToken,
        sink: &Arc<dyn ToolExecutionEventSink>,
        background_completion: Option<Arc<dyn ToolBackgroundCompletionSink>>,
    ) -> Result<ToolExecutionResult> {
        let running = self.start_command_process(request, sink).await?;
        let yield_ms = request.yield_ms.filter(|value| *value > 0);
        if request.background || yield_ms.is_some() {
            return self
                .run_command_process_with_background_option(
                    running,
                    cancellation.clone(),
                    sink,
                    background_completion,
                    request.background,
                    yield_ms,
                )
                .await;
        }

        finish_command_process(running, cancellation).await
    }

    /// Record a command's terminal outcome into the repeat guard (a no-op for
    /// cancelled/loop-blocked results, which the guard ignores).
    pub fn record_command_outcome(
        &self,
        request: &ToolExecutionRequest,
        result: &ToolExecutionResult,
    ) {
        if let Some(repeat_guard) = &self.repeat_guard {
            repeat_guard.record_command_result(request, result);
        }
    }

    async fn start_command_process(
        &self,
        request: &ToolExecutionRequest,
        sink: &Arc<dyn ToolExecutionEventSink>,
    ) -> Result<RunningCommandProcess> {
        let mut process = self.sandbox.spawn(process_spec(request)).await?;

        let writer = if request.output_policy.spill_to_file {
            match &self.output_store {
                Some(store) => Some(Arc::new(Mutex::new(
                    store
                        .open_with_context(ToolOutputContext::for_artifact(
                            request.tool_call_id.clone(),
                            request.run_id.clone(),
                            request.project_id.clone(),
                            "output",
                        ))
                        .await?,
                )) as SharedToolOutputWriter),
                None => None,
            }
        } else {
            None
        };

        let stdout = process.take_stdout();
        let stderr = process.take_stderr();
        let stdout_task = stdout.map(|stdout| {
            let request = request.clone();
            let policy = request.output_policy;
            let sink = Arc::clone(sink);
            let writer = writer.as_ref().map(Arc::clone);
            tokio::spawn(async move {
                drain_stream(
                    request,
                    super::types::ToolOutputStream::Stdout,
                    stdout,
                    policy,
                    sink,
                    writer,
                )
                .await
            })
        });
        let stderr_task = stderr.map(|stderr| {
            let request = request.clone();
            let policy = request.output_policy;
            let sink = Arc::clone(sink);
            let writer = writer.as_ref().map(Arc::clone);
            tokio::spawn(async move {
                drain_stream(
                    request,
                    super::types::ToolOutputStream::Stderr,
                    stderr,
                    policy,
                    sink,
                    writer,
                )
                .await
            })
        });

        Ok(RunningCommandProcess {
            request: request.clone(),
            process,
            stdout_task,
            stderr_task,
            writer,
        })
    }

    async fn run_command_process_with_background_option(
        &self,
        running: RunningCommandProcess,
        cancellation: ToolCancellationToken,
        sink: &Arc<dyn ToolExecutionEventSink>,
        completion_sink: Option<Arc<dyn ToolBackgroundCompletionSink>>,
        force_background: bool,
        yield_ms: Option<u64>,
    ) -> Result<ToolExecutionResult> {
        let request = running.request.clone();
        let state = Arc::new(BackgroundRunState::default());
        if force_background {
            *state.decision.lock().await = BackgroundRunDecision::Backgrounded;
            let result = backgrounded_result(&request, "background requested");
            emit_background_terminal_event(&request, &result, sink);
            spawn_command_completion_task(
                running,
                cancellation,
                Arc::clone(sink),
                completion_sink,
                self.repeat_guard.clone(),
                Arc::clone(&state),
            );
            return Ok(result);
        }
        spawn_command_completion_task(
            running,
            cancellation,
            Arc::clone(sink),
            completion_sink,
            self.repeat_guard.clone(),
            Arc::clone(&state),
        );

        let Some(yield_ms) = yield_ms else {
            return wait_for_background_decision(&state).await;
        };

        tokio::select! {
            result = wait_for_background_decision(&state) => result,
            _ = tokio::time::sleep(Duration::from_millis(yield_ms)) => {
                let mut decision = state.decision.lock().await;
                match std::mem::replace(&mut *decision, BackgroundRunDecision::Backgrounded) {
                    BackgroundRunDecision::Completed(result) => Ok(result),
                    BackgroundRunDecision::Awaiting | BackgroundRunDecision::Backgrounded => {
                        let result = backgrounded_result(&request, "yield elapsed");
                        emit_background_terminal_event(&request, &result, sink);
                        Ok(result)
                    }
                }
            }
        }
    }
}

type DrainTask = Option<tokio::task::JoinHandle<Result<DrainedStream>>>;

struct RunningCommandProcess {
    request: ToolExecutionRequest,
    process: Box<dyn super::process::SpawnedToolProcess>,
    stdout_task: DrainTask,
    stderr_task: DrainTask,
    writer: Option<SharedToolOutputWriter>,
}

#[derive(Default)]
struct BackgroundRunState {
    decision: Mutex<BackgroundRunDecision>,
    notify: Notify,
}

#[derive(Default)]
enum BackgroundRunDecision {
    #[default]
    Awaiting,
    Completed(ToolExecutionResult),
    Backgrounded,
}

fn spawn_command_completion_task(
    running: RunningCommandProcess,
    cancellation: ToolCancellationToken,
    sink: Arc<dyn ToolExecutionEventSink>,
    completion_sink: Option<Arc<dyn ToolBackgroundCompletionSink>>,
    repeat_guard: Option<Arc<ToolRepeatGuard>>,
    state: Arc<BackgroundRunState>,
) {
    tokio::spawn(async move {
        let request = running.request.clone();
        let result = finish_command_process(running, &cancellation)
            .await
            .unwrap_or_else(|error| failed_background_result(&request, error.to_string()));

        let mut decision = state.decision.lock().await;
        match &*decision {
            BackgroundRunDecision::Awaiting => {
                *decision = BackgroundRunDecision::Completed(result);
                state.notify.notify_waiters();
            }
            BackgroundRunDecision::Backgrounded => {
                drop(decision);
                if let Some(repeat_guard) = repeat_guard {
                    repeat_guard.record_command_result(&request, &result);
                }
                emit_background_terminal_event(&request, &result, &sink);
                if let Some(completion_sink) = completion_sink {
                    completion_sink.on_background_command_complete(&request, &result);
                }
            }
            BackgroundRunDecision::Completed(_) => {}
        }
    });
}

async fn wait_for_background_decision(
    state: &Arc<BackgroundRunState>,
) -> Result<ToolExecutionResult> {
    loop {
        let notified = state.notify.notified();
        {
            let mut decision = state.decision.lock().await;
            if matches!(*decision, BackgroundRunDecision::Completed(_)) {
                let BackgroundRunDecision::Completed(result) =
                    std::mem::replace(&mut *decision, BackgroundRunDecision::Backgrounded)
                else {
                    unreachable!();
                };
                return Ok(result);
            }
        }
        notified.await;
    }
}

async fn finish_command_process(
    mut running: RunningCommandProcess,
    cancellation: &ToolCancellationToken,
) -> Result<ToolExecutionResult> {
    let (status, exit_code, terminal_message) =
        wait_for_process(&running.request, &mut *running.process, cancellation).await?;

    let stdout = join_drain(running.stdout_task).await?;
    let stderr = join_drain(running.stderr_task).await?;

    let log_ref = match running.writer {
        Some(writer) => Some(writer.lock().await.finish().await?),
        None => None,
    };

    let stdout_preview = stdout
        .as_ref()
        .map(|stream| stream.preview.clone())
        .unwrap_or_default();
    let stderr_preview = stderr
        .as_ref()
        .map(|stream| stream.preview.clone())
        .unwrap_or_default();
    let stdout_tail = stdout
        .as_ref()
        .map(|stream| stream.tail.clone())
        .unwrap_or_default();
    let stderr_tail = stderr
        .as_ref()
        .map(|stream| stream.tail.clone())
        .unwrap_or_default();
    let stdout_bytes = stdout
        .as_ref()
        .map(|stream| stream.total_bytes)
        .unwrap_or_default();
    let stderr_bytes = stderr
        .as_ref()
        .map(|stream| stream.total_bytes)
        .unwrap_or_default();
    let truncated_for_display = stdout
        .as_ref()
        .map(|stream| stream.preview_truncated)
        .unwrap_or(false)
        || stderr
            .as_ref()
            .map(|stream| stream.preview_truncated)
            .unwrap_or(false);
    let truncated_for_agent = stdout
        .as_ref()
        .map(|stream| stream.tail_truncated)
        .unwrap_or(false)
        || stderr
            .as_ref()
            .map(|stream| stream.tail_truncated)
            .unwrap_or(false);

    Ok(ToolExecutionResult {
        tool_call_id: running.request.tool_call_id,
        status,
        exit_code,
        stdout_preview,
        stderr_preview,
        stdout_tail,
        stderr_tail,
        stdout_bytes,
        stderr_bytes,
        truncated_for_display,
        truncated_for_agent,
        log_ref,
        message: terminal_message,
    })
}

fn backgrounded_result(request: &ToolExecutionRequest, reason: &str) -> ToolExecutionResult {
    let mut message = format!(
        "command is running in background ({reason}); tool_call_id: {}",
        request.tool_call_id
    );
    if request.notify_on_complete {
        message.push_str("; completion notification is enabled for the active run");
    }
    ToolExecutionResult {
        tool_call_id: request.tool_call_id.clone(),
        status: ToolExecutionStatus::Backgrounded,
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

fn failed_background_result(
    request: &ToolExecutionRequest,
    message: String,
) -> ToolExecutionResult {
    ToolExecutionResult {
        tool_call_id: request.tool_call_id.clone(),
        status: ToolExecutionStatus::Failed,
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

fn emit_background_terminal_event(
    request: &ToolExecutionRequest,
    result: &ToolExecutionResult,
    sink: &Arc<dyn ToolExecutionEventSink>,
) {
    let mut event = event(request, event_kind_for_status(result.status));
    event.message = result.message.clone();
    event.result = Some(result.clone());
    let (mut payload, artifacts) = run_command_typed_payload(&request.command, result);
    enrich_command_payload(&mut payload, request);
    event.payload = Some(payload);
    event.artifacts = artifacts;
    sink.emit(event);
}

fn event_kind_for_status(status: ToolExecutionStatus) -> ToolExecutionEventKind {
    match status {
        ToolExecutionStatus::Backgrounded => ToolExecutionEventKind::Backgrounded,
        ToolExecutionStatus::Completed => ToolExecutionEventKind::Completed,
        ToolExecutionStatus::Failed => ToolExecutionEventKind::Failed,
        ToolExecutionStatus::Cancelled => ToolExecutionEventKind::Cancelled,
        ToolExecutionStatus::TimedOut => ToolExecutionEventKind::TimedOut,
        ToolExecutionStatus::PermissionDenied => ToolExecutionEventKind::PermissionDenied,
        ToolExecutionStatus::LoopBlocked => ToolExecutionEventKind::LoopBlocked,
    }
}

fn enrich_command_payload(payload: &mut serde_json::Value, request: &ToolExecutionRequest) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    object.insert(
        "executionMode".to_string(),
        serde_json::json!(if request.background {
            "background"
        } else {
            "foreground"
        }),
    );
    object.insert(
        "background".to_string(),
        serde_json::json!(request.background),
    );
    object.insert(
        "notifyOnComplete".to_string(),
        serde_json::json!(request.notify_on_complete),
    );
    if let Some(timeout_ms) = request.timeout_ms {
        object.insert("timeoutMs".to_string(), serde_json::json!(timeout_ms));
    }
    if let Some(yield_ms) = request.yield_ms {
        object.insert("yieldMs".to_string(), serde_json::json!(yield_ms));
    }
}

async fn wait_for_process(
    request: &ToolExecutionRequest,
    process: &mut dyn super::process::SpawnedToolProcess,
    cancellation: &ToolCancellationToken,
) -> Result<(ToolExecutionStatus, Option<i32>, Option<String>)> {
    if let Some(timeout) = request.timeout_ms.map(Duration::from_millis) {
        tokio::select! {
            exit = process.wait() => {
                let exit = exit?;
                let status = if exit.is_success() {
                    ToolExecutionStatus::Completed
                } else {
                    ToolExecutionStatus::Failed
                };
                Ok((status, exit.code, None))
            }
            _ = cancellation.cancelled() => {
                process.kill_tree().await?;
                let _ = process.wait().await;
                Ok((ToolExecutionStatus::Cancelled, None, Some("tool call cancelled".to_string())))
            }
            _ = tokio::time::sleep(timeout) => {
                process.kill_tree().await?;
                let _ = process.wait().await;
                Ok((ToolExecutionStatus::TimedOut, None, Some(format!("tool timed out after {} ms", request.timeout_ms.unwrap_or_default()))))
            }
        }
    } else {
        tokio::select! {
            exit = process.wait() => {
                let exit = exit?;
                let status = if exit.is_success() {
                    ToolExecutionStatus::Completed
                } else {
                    ToolExecutionStatus::Failed
                };
                Ok((status, exit.code, None))
            }
            _ = cancellation.cancelled() => {
                process.kill_tree().await?;
                let _ = process.wait().await;
                Ok((ToolExecutionStatus::Cancelled, None, Some("tool call cancelled".to_string())))
            }
        }
    }
}

async fn join_drain(
    task: Option<tokio::task::JoinHandle<Result<super::output::DrainedStream>>>,
) -> Result<Option<super::output::DrainedStream>> {
    match task {
        Some(task) => task
            .await
            .map_err(|error| {
                MothershipError::Runtime(format!("tool output drain panicked: {error}"))
            })? // join failure
            .map(Some),
        None => Ok(None),
    }
}

fn process_spec(request: &ToolExecutionRequest) -> ToolProcessSpec {
    ToolProcessSpec {
        program: OsString::from(&request.command.program),
        args: request.command.args.iter().map(OsString::from).collect(),
        cwd: request.cwd.clone(),
        env: request
            .command
            .env
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect(),
        timeout: request.timeout_ms.map(Duration::from_millis),
    }
}
