use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::{MothershipError, Result};

use super::cancellation::ToolCancellationToken;
use super::output::{drain_stream, SharedToolOutputWriter, ToolOutputStore};
use super::permissions::{
    ConservativeCommandPermissionPolicy, StaticToolApprovalGate, ToolApprovalDecision,
    ToolApprovalGate, ToolPermissionAction, ToolPermissionPolicy,
};
use super::process::{ToolProcessSandbox, ToolProcessSpec};
use super::repeat_guard::ToolRepeatGuard;
use super::resources::{ToolResourceGate, ToolResourceLimits};
use super::types::{
    event, ToolExecutionEventKind, ToolExecutionEventSink, ToolExecutionRequest,
    ToolExecutionResult, ToolExecutionStatus,
};

pub struct ToolSupervisor {
    sandbox: Arc<dyn ToolProcessSandbox>,
    permission_policy: Arc<dyn ToolPermissionPolicy>,
    approval_gate: Arc<dyn ToolApprovalGate>,
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
            approval_gate: StaticToolApprovalGate::deny("approval UI is not connected yet"),
            repeat_guard: None,
        }
    }

    pub fn with_policy(
        mut self,
        permission_policy: Arc<dyn ToolPermissionPolicy>,
        approval_gate: Arc<dyn ToolApprovalGate>,
    ) -> Self {
        self.permission_policy = permission_policy;
        self.approval_gate = approval_gate;
        self
    }

    pub fn with_repeat_guard(mut self, repeat_guard: Arc<ToolRepeatGuard>) -> Self {
        self.repeat_guard = Some(repeat_guard);
        self
    }

    pub async fn run_command(
        &self,
        request: ToolExecutionRequest,
        cancellation: ToolCancellationToken,
        sink: Arc<dyn ToolExecutionEventSink>,
    ) -> Result<ToolExecutionResult> {
        sink.emit(event(&request, ToolExecutionEventKind::Queued));

        if cancellation.is_cancelled() {
            let result = terminal_result(
                &request,
                ToolExecutionStatus::Cancelled,
                None,
                None,
                Some("tool call was cancelled before it started".to_string()),
            );
            emit_terminal(&sink, &request, ToolExecutionEventKind::Cancelled, &result);
            return Ok(result);
        }

        if let Some(block) = self
            .repeat_guard
            .as_ref()
            .and_then(|guard| guard.check_command(&request))
        {
            let result = terminal_result(
                &request,
                ToolExecutionStatus::LoopBlocked,
                None,
                None,
                Some(block.message),
            );
            emit_terminal(
                &sink,
                &request,
                ToolExecutionEventKind::LoopBlocked,
                &result,
            );
            return Ok(result);
        }

        let evaluation = self.permission_policy.evaluate(&request);
        match evaluation.action {
            ToolPermissionAction::Allow => {}
            ToolPermissionAction::Deny => {
                let result = terminal_result(
                    &request,
                    ToolExecutionStatus::PermissionDenied,
                    None,
                    None,
                    Some(evaluation.reason),
                );
                self.record_repeat_guard(&request, &result);
                emit_terminal(
                    &sink,
                    &request,
                    ToolExecutionEventKind::PermissionDenied,
                    &result,
                );
                return Ok(result);
            }
            ToolPermissionAction::Ask => {
                let mut permission_event =
                    event(&request, ToolExecutionEventKind::PermissionRequested);
                permission_event.message = Some(evaluation.reason.clone());
                sink.emit(permission_event);

                match self
                    .approval_gate
                    .request_approval(&request, &evaluation, &cancellation)
                    .await?
                {
                    ToolApprovalDecision::Approved => {}
                    ToolApprovalDecision::Denied { reason } => {
                        let result = terminal_result(
                            &request,
                            ToolExecutionStatus::PermissionDenied,
                            None,
                            None,
                            Some(reason),
                        );
                        self.record_repeat_guard(&request, &result);
                        emit_terminal(
                            &sink,
                            &request,
                            ToolExecutionEventKind::PermissionDenied,
                            &result,
                        );
                        return Ok(result);
                    }
                }
            }
        }

        let _leases = match self.resources.acquire(&request, &cancellation, &sink).await {
            Ok(leases) => leases,
            Err(error) if cancellation.is_cancelled() => {
                let result = terminal_result(
                    &request,
                    ToolExecutionStatus::Cancelled,
                    None,
                    None,
                    Some(error.to_string()),
                );
                self.record_repeat_guard(&request, &result);
                emit_terminal(&sink, &request, ToolExecutionEventKind::Cancelled, &result);
                return Ok(result);
            }
            Err(error) => return Err(error),
        };

        let mut process = match self.sandbox.spawn(process_spec(&request)).await {
            Ok(process) => process,
            Err(error) => {
                let result = terminal_result(
                    &request,
                    ToolExecutionStatus::Failed,
                    None,
                    None,
                    Some(error.to_string()),
                );
                self.record_repeat_guard(&request, &result);
                emit_terminal(&sink, &request, ToolExecutionEventKind::Failed, &result);
                return Ok(result);
            }
        };

        let mut started = event(&request, ToolExecutionEventKind::Started);
        started.message = Some(format!("started pid {}", process.pid()));
        sink.emit(started);

        let writer = if request.output_policy.spill_to_file {
            match &self.output_store {
                Some(store) => Some(
                    Arc::new(Mutex::new(store.open(&request.tool_call_id).await?))
                        as SharedToolOutputWriter,
                ),
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
            let sink = Arc::clone(&sink);
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
            let sink = Arc::clone(&sink);
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

        let (status, exit_code, terminal_message) =
            wait_for_process(&request, &mut *process, &cancellation).await?;

        let stdout = join_drain(stdout_task).await?;
        let stderr = join_drain(stderr_task).await?;

        let log_ref = match writer {
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

        let result = ToolExecutionResult {
            tool_call_id: request.tool_call_id.clone(),
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
        };

        let kind = match status {
            ToolExecutionStatus::Completed => ToolExecutionEventKind::Completed,
            ToolExecutionStatus::Failed => ToolExecutionEventKind::Failed,
            ToolExecutionStatus::Cancelled => ToolExecutionEventKind::Cancelled,
            ToolExecutionStatus::TimedOut => ToolExecutionEventKind::TimedOut,
            ToolExecutionStatus::PermissionDenied => ToolExecutionEventKind::PermissionDenied,
            ToolExecutionStatus::LoopBlocked => ToolExecutionEventKind::LoopBlocked,
        };
        self.record_repeat_guard(&request, &result);
        emit_terminal(&sink, &request, kind, &result);

        Ok(result)
    }

    fn record_repeat_guard(&self, request: &ToolExecutionRequest, result: &ToolExecutionResult) {
        if let Some(repeat_guard) = &self.repeat_guard {
            repeat_guard.record_command_result(request, result);
        }
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

fn terminal_result(
    request: &ToolExecutionRequest,
    status: ToolExecutionStatus,
    exit_code: Option<i32>,
    log_ref: Option<String>,
    message: Option<String>,
) -> ToolExecutionResult {
    ToolExecutionResult {
        tool_call_id: request.tool_call_id.clone(),
        status,
        exit_code,
        stdout_preview: String::new(),
        stderr_preview: String::new(),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        stdout_bytes: 0,
        stderr_bytes: 0,
        truncated_for_display: false,
        truncated_for_agent: false,
        log_ref,
        message,
    }
}

fn emit_terminal(
    sink: &Arc<dyn ToolExecutionEventSink>,
    request: &ToolExecutionRequest,
    kind: ToolExecutionEventKind,
    result: &ToolExecutionResult,
) {
    let mut event = event(request, kind);
    event.result = Some(result.clone());
    event.message = result.message.clone();
    sink.emit(event);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::io::AsyncRead;

    use crate::tools::{
        ConservativeCommandPermissionPolicy, FileToolOutputStore, SpawnedToolProcess,
        StaticToolApprovalGate, ToolCommand, ToolExecutionEvent, ToolExecutionEventKind,
        ToolExecutionEventSink, ToolExecutionRequest, ToolExecutionStatus, ToolOutputPolicy,
        ToolProcessExit, ToolProcessSandbox, ToolProcessSpec, ToolRepeatGuard, ToolResourceLimits,
    };
    use crate::Result;

    use super::ToolCancellationToken;
    use super::ToolSupervisor;

    #[tokio::test]
    async fn run_command_streams_spills_and_returns_bounded_output() {
        let sandbox = Arc::new(FakeSandbox::new(
            b"abcdefghijklmnopqrstuvwxyz".to_vec(),
            b"warning".to_vec(),
            Some(0),
        ));
        let output_dir = unique_temp_dir();
        let sink = Arc::new(RecordingSink::default());
        let supervisor = ToolSupervisor::new(
            sandbox,
            Some(Arc::new(FileToolOutputStore::new(&output_dir))),
            ToolResourceLimits::default(),
        );

        let result = supervisor
            .run_command(
                ToolExecutionRequest {
                    tool_call_id: "tool_test_1".to_string(),
                    run_id: Some("run_1".to_string()),
                    project_id: Some("project_1".to_string()),
                    cwd: None,
                    command: ToolCommand::new("git", ["status"]),
                    timeout_ms: None,
                    output_policy: ToolOutputPolicy {
                        memory_preview_bytes: 10,
                        ui_stream_bytes_per_sec: 1024,
                        agent_tail_bytes: 6,
                        spill_to_file: true,
                    },
                },
                ToolCancellationToken::default(),
                sink.clone(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, ToolExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout_bytes, 26);
        assert!(result.truncated_for_display);
        assert!(result.truncated_for_agent);
        assert_eq!(result.stdout_tail, "uvwxyz");
        let log_ref = result.log_ref.expect("log ref");
        let log = tokio::fs::read_to_string(log_ref).await.unwrap();
        assert!(log.contains("--- stdout 26 bytes ---"));
        assert!(log.contains("abcdefghijklmnopqrstuvwxyz"));

        let kinds = sink.kinds();
        assert!(kinds.contains(&ToolExecutionEventKind::Queued));
        assert!(kinds.contains(&ToolExecutionEventKind::Started));
        assert!(kinds.contains(&ToolExecutionEventKind::Output));
        assert!(kinds.contains(&ToolExecutionEventKind::Completed));

        let _ = tokio::fs::remove_dir_all(output_dir).await;
    }

    #[tokio::test]
    async fn command_requiring_approval_does_not_spawn_when_denied() {
        let sandbox = Arc::new(FakeSandbox::new(Vec::new(), Vec::new(), Some(0)));
        let sink = Arc::new(RecordingSink::default());
        let supervisor = ToolSupervisor::new(sandbox.clone(), None, ToolResourceLimits::default())
            .with_policy(
                Arc::new(ConservativeCommandPermissionPolicy),
                StaticToolApprovalGate::deny("not approved"),
            );

        let result = supervisor
            .run_command(
                ToolExecutionRequest {
                    tool_call_id: "tool_test_2".to_string(),
                    run_id: None,
                    project_id: Some("project_1".to_string()),
                    cwd: None,
                    command: ToolCommand::new("npm", ["install"]),
                    timeout_ms: None,
                    output_policy: ToolOutputPolicy::default(),
                },
                ToolCancellationToken::default(),
                sink.clone(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, ToolExecutionStatus::PermissionDenied);
        assert_eq!(sandbox.spawn_count(), 0);
        let kinds = sink.kinds();
        assert!(kinds.contains(&ToolExecutionEventKind::PermissionRequested));
        assert!(kinds.contains(&ToolExecutionEventKind::PermissionDenied));
    }

    #[tokio::test]
    async fn non_zero_exit_is_a_failed_tool_result_not_a_supervisor_error() {
        let sandbox = Arc::new(FakeSandbox::new(b"bad".to_vec(), Vec::new(), Some(2)));
        let sink = Arc::new(RecordingSink::default());
        let supervisor = ToolSupervisor::new(sandbox, None, ToolResourceLimits::default());

        let result = supervisor
            .run_command(
                ToolExecutionRequest {
                    tool_call_id: "tool_test_3".to_string(),
                    run_id: None,
                    project_id: Some("project_1".to_string()),
                    cwd: None,
                    command: ToolCommand::new("git", ["status"]),
                    timeout_ms: None,
                    output_policy: ToolOutputPolicy::default(),
                },
                ToolCancellationToken::default(),
                sink.clone(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, ToolExecutionStatus::Failed);
        assert_eq!(result.exit_code, Some(2));
        assert!(sink.kinds().contains(&ToolExecutionEventKind::Failed));
    }

    #[tokio::test]
    async fn repeat_guard_blocks_third_identical_command_without_spawning() {
        let sandbox = Arc::new(FakeSandbox::new(
            b"nothing to commit".to_vec(),
            Vec::new(),
            Some(0),
        ));
        let sink = Arc::new(RecordingSink::default());
        let supervisor = ToolSupervisor::new(sandbox.clone(), None, ToolResourceLimits::default())
            .with_repeat_guard(Arc::new(ToolRepeatGuard::default()));

        let first = repeat_guard_request("tool_repeat_1");
        let second = repeat_guard_request("tool_repeat_2");
        let third = repeat_guard_request("tool_repeat_3");

        let first_result = supervisor
            .run_command(first, ToolCancellationToken::default(), sink.clone())
            .await
            .unwrap();
        let second_result = supervisor
            .run_command(second, ToolCancellationToken::default(), sink.clone())
            .await
            .unwrap();
        let third_result = supervisor
            .run_command(third, ToolCancellationToken::default(), sink.clone())
            .await
            .unwrap();

        assert_eq!(first_result.status, ToolExecutionStatus::Completed);
        assert_eq!(second_result.status, ToolExecutionStatus::Completed);
        assert_eq!(third_result.status, ToolExecutionStatus::LoopBlocked);
        assert_eq!(sandbox.spawn_count(), 2);
        assert!(third_result
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("Repeated identical tool call suppressed"));
        assert!(sink.kinds().contains(&ToolExecutionEventKind::LoopBlocked));
    }

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<ToolExecutionEvent>>,
    }

    impl RecordingSink {
        fn kinds(&self) -> Vec<ToolExecutionEventKind> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|event| event.kind)
                .collect()
        }
    }

    impl ToolExecutionEventSink for RecordingSink {
        fn emit(&self, event: ToolExecutionEvent) {
            self.events.lock().unwrap().push(event);
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

        fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
            self.stdout.take().map(|bytes| {
                Box::new(std::io::Cursor::new(bytes)) as Box<dyn AsyncRead + Send + Unpin>
            })
        }

        fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
            self.stderr.take().map(|bytes| {
                Box::new(std::io::Cursor::new(bytes)) as Box<dyn AsyncRead + Send + Unpin>
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

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mothership-tool-test-{nanos}"))
    }

    fn repeat_guard_request(tool_call_id: &str) -> ToolExecutionRequest {
        ToolExecutionRequest {
            tool_call_id: tool_call_id.to_string(),
            run_id: Some("run_repeat_guard".to_string()),
            project_id: Some("project_1".to_string()),
            cwd: None,
            command: ToolCommand::new("git", ["status"]),
            timeout_ms: None,
            output_policy: ToolOutputPolicy::default(),
        }
    }
}
