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
    tool_batch_plan, ChatCancellationToken, LlmToolCallHandler, LlmToolCallRequest,
    LlmToolCallResult, MothershipError, Result, SpawnedToolProcess, ToolBatchPlan,
    ToolCancellationToken, ToolCommand, ToolExecutionEventSink, ToolExecutionRegistry,
    ToolExecutionRequest, ToolExecutionResult, ToolExecutionStatus, ToolOutputPolicy,
    ToolProcessExit, ToolProcessSandbox, ToolProcessSpec, ToolSupervisor, RUN_COMMAND_TOOL_NAME,
};
use serde::Deserialize;
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
    ) -> Self {
        Self {
            supervisor,
            registry,
            runtime,
            sink,
            project: project.map(|(id, root)| ToolProjectContext { id, root }),
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
            other => LlmToolCallResult {
                ok: false,
                content: format!("unsupported tool `{other}`"),
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
    use std::time::Duration;

    use super::*;

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
