use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::types::{ToolExecutionRequest, ToolExecutionResult, ToolExecutionStatus};

const COMMAND_TOOL_KIND: &str = "run_command";

#[derive(Debug)]
pub struct ToolRepeatGuard {
    config: ToolRepeatGuardConfig,
    state: Mutex<ToolRepeatGuardState>,
}

impl ToolRepeatGuard {
    pub fn new(config: ToolRepeatGuardConfig) -> Self {
        Self {
            config,
            state: Mutex::new(ToolRepeatGuardState::default()),
        }
    }

    pub fn check_command(&self, request: &ToolExecutionRequest) -> Option<ToolRepeatBlock> {
        let run_key = run_key(request)?;
        let signature_hash = command_signature_hash(request);
        let state = self.state.lock().unwrap();
        let history = state.runs.get(&run_key)?;
        let reference_outcome = history
            .records
            .iter()
            .rev()
            .find(|record| record.signature_hash == signature_hash)
            .map(|record| record.outcome_hash.clone())?;

        let consecutive_repeats = history
            .records
            .iter()
            .rev()
            .take_while(|record| {
                record.signature_hash == signature_hash && record.outcome_hash == reference_outcome
            })
            .count();
        let recent_repeats = history
            .records
            .iter()
            .rev()
            .take(self.config.recent_window)
            .filter(|record| {
                record.signature_hash == signature_hash && record.outcome_hash == reference_outcome
            })
            .count();

        if consecutive_repeats < self.config.consecutive_same_result_limit
            && recent_repeats < self.config.recent_same_result_limit
        {
            return None;
        }

        Some(ToolRepeatBlock {
            signature_hash,
            consecutive_repeats,
            recent_repeats,
            message: format!(
                "Repeated identical tool call suppressed: `{}` already returned the same result. Use a different command, different arguments, or explain why repeating it is necessary.",
                command_label(request)
            ),
        })
    }

    pub fn record_command_result(
        &self,
        request: &ToolExecutionRequest,
        result: &ToolExecutionResult,
    ) {
        if should_ignore_result(result.status) {
            return;
        }
        let Some(run_key) = run_key(request) else {
            return;
        };

        let mut state = self.state.lock().unwrap();
        state.ensure_run(&run_key, self.config.max_tracked_runs);
        let history = state.runs.entry(run_key).or_default();
        history.records.push_back(ToolRepeatRecord {
            signature_hash: command_signature_hash(request),
            outcome_hash: command_outcome_hash(result),
        });
        while history.records.len() > self.config.max_records_per_run {
            history.records.pop_front();
        }
    }
}

impl Default for ToolRepeatGuard {
    fn default() -> Self {
        Self::new(ToolRepeatGuardConfig::default())
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ToolRepeatGuardConfig {
    /// Number of already-observed identical command outcomes after which the
    /// next same command is blocked. The default blocks the third identical
    /// no-progress request.
    pub consecutive_same_result_limit: usize,
    /// Number of identical command outcomes in the recent window after which
    /// an interleaved repeat is blocked.
    pub recent_same_result_limit: usize,
    pub recent_window: usize,
    pub max_records_per_run: usize,
    pub max_tracked_runs: usize,
}

impl Default for ToolRepeatGuardConfig {
    fn default() -> Self {
        Self {
            consecutive_same_result_limit: 2,
            recent_same_result_limit: 4,
            recent_window: 10,
            max_records_per_run: 64,
            max_tracked_runs: 128,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ToolRepeatBlock {
    pub signature_hash: String,
    pub consecutive_repeats: usize,
    pub recent_repeats: usize,
    pub message: String,
}

#[derive(Debug, Default)]
struct ToolRepeatGuardState {
    runs: HashMap<String, RunToolHistory>,
    run_order: VecDeque<String>,
}

impl ToolRepeatGuardState {
    fn ensure_run(&mut self, run_key: &str, max_tracked_runs: usize) {
        if self.runs.contains_key(run_key) {
            return;
        }
        while self.runs.len() >= max_tracked_runs {
            let Some(oldest) = self.run_order.pop_front() else {
                break;
            };
            self.runs.remove(&oldest);
        }
        self.run_order.push_back(run_key.to_string());
    }
}

#[derive(Debug, Default)]
struct RunToolHistory {
    records: VecDeque<ToolRepeatRecord>,
}

#[derive(Debug)]
struct ToolRepeatRecord {
    signature_hash: String,
    outcome_hash: String,
}

#[derive(Serialize)]
struct CanonicalCommandSignature {
    tool_kind: &'static str,
    project_id: Option<String>,
    cwd: Option<String>,
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

#[derive(Serialize)]
struct CanonicalCommandOutcome<'a> {
    status: ToolExecutionStatus,
    exit_code: Option<i32>,
    stdout_tail: &'a str,
    stderr_tail: &'a str,
    stdout_bytes: usize,
    stderr_bytes: usize,
    truncated_for_agent: bool,
    message: Option<&'a str>,
}

fn command_signature_hash(request: &ToolExecutionRequest) -> String {
    let signature = CanonicalCommandSignature {
        tool_kind: COMMAND_TOOL_KIND,
        project_id: request
            .project_id
            .as_deref()
            .map(|project_id| project_id.trim().to_string()),
        cwd: request.cwd.as_deref().map(normalize_path),
        program: normalize_program(&request.command.program),
        args: request.command.args.clone(),
        env: request
            .command
            .env
            .iter()
            .map(|(key, value)| (key.trim().to_string(), value.clone()))
            .collect(),
    };
    stable_hash(&signature)
}

fn command_outcome_hash(result: &ToolExecutionResult) -> String {
    stable_hash(&CanonicalCommandOutcome {
        status: result.status,
        exit_code: result.exit_code,
        stdout_tail: &result.stdout_tail,
        stderr_tail: &result.stderr_tail,
        stdout_bytes: result.stdout_bytes,
        stderr_bytes: result.stderr_bytes,
        truncated_for_agent: result.truncated_for_agent,
        message: result.message.as_deref(),
    })
}

fn stable_hash(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("canonical tool repeat value must serialize");
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn run_key(request: &ToolExecutionRequest) -> Option<String> {
    let run_id = request.run_id.as_deref()?.trim();
    if run_id.is_empty() {
        None
    } else {
        Some(run_id.to_string())
    }
}

fn normalize_program(program: &str) -> String {
    let normalized = program.trim().replace('\\', "/");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn normalize_path(path: &Path) -> String {
    let mut normalized = path.to_string_lossy().trim().replace('\\', "/");
    while normalized.ends_with('/') && normalized.len() > 1 {
        normalized.pop();
    }
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn command_label(request: &ToolExecutionRequest) -> String {
    let mut parts = Vec::with_capacity(1 + request.command.args.len());
    parts.push(request.command.program.as_str());
    parts.extend(request.command.args.iter().map(String::as_str));
    let mut label = parts.join(" ");
    const MAX_LABEL_CHARS: usize = 180;
    if label.len() > MAX_LABEL_CHARS {
        label.truncate(MAX_LABEL_CHARS);
        label.push_str("...");
    }
    label
}

fn should_ignore_result(status: ToolExecutionStatus) -> bool {
    matches!(
        status,
        ToolExecutionStatus::Cancelled | ToolExecutionStatus::LoopBlocked
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolCommand, ToolOutputPolicy};

    #[test]
    fn blocks_third_identical_command_with_same_result_in_run() {
        let guard = ToolRepeatGuard::default();
        let first_request = request("tool_1", Some("run_1"), ["status"]);
        let result = result("unchanged");

        guard.record_command_result(&first_request, &result);
        guard.record_command_result(&request("tool_2", Some("run_1"), ["status"]), &result);

        let block = guard
            .check_command(&request("tool_3", Some("run_1"), ["status"]))
            .expect("third identical no-progress request should be blocked");
        assert_eq!(block.consecutive_repeats, 2);
    }

    #[test]
    fn allows_same_command_when_result_changed() {
        let guard = ToolRepeatGuard::default();

        guard.record_command_result(
            &request("tool_1", Some("run_1"), ["status"]),
            &result("first"),
        );
        guard.record_command_result(
            &request("tool_2", Some("run_1"), ["status"]),
            &result("second"),
        );

        assert!(guard
            .check_command(&request("tool_3", Some("run_1"), ["status"]))
            .is_none());
    }

    #[test]
    fn allows_different_arguments_and_different_runs() {
        let guard = ToolRepeatGuard::default();
        let result = result("same");

        guard.record_command_result(&request("tool_1", Some("run_1"), ["status"]), &result);
        guard.record_command_result(&request("tool_2", Some("run_1"), ["status"]), &result);

        assert!(guard
            .check_command(&request("tool_3", Some("run_1"), ["diff"]))
            .is_none());
        assert!(guard
            .check_command(&request("tool_4", Some("run_2"), ["status"]))
            .is_none());
    }

    #[test]
    fn ignores_unscoped_manual_tool_requests() {
        let guard = ToolRepeatGuard::default();
        let result = result("same");

        guard.record_command_result(&request("tool_1", None, ["status"]), &result);
        guard.record_command_result(&request("tool_2", None, ["status"]), &result);

        assert!(guard
            .check_command(&request("tool_3", None, ["status"]))
            .is_none());
    }

    fn request(
        tool_call_id: &str,
        run_id: Option<&str>,
        args: impl IntoIterator<Item = &'static str>,
    ) -> ToolExecutionRequest {
        ToolExecutionRequest {
            tool_call_id: tool_call_id.to_string(),
            run_id: run_id.map(str::to_string),
            project_id: Some("project_1".to_string()),
            cwd: Some(Path::new("E:/Mothership").to_path_buf()),
            command: ToolCommand::new("git", args),
            timeout_ms: Some(60_000),
            output_policy: ToolOutputPolicy::default(),
        }
    }

    fn result(stdout_tail: &str) -> ToolExecutionResult {
        ToolExecutionResult {
            tool_call_id: "tool_result".to_string(),
            status: ToolExecutionStatus::Completed,
            exit_code: Some(0),
            stdout_preview: stdout_tail.to_string(),
            stderr_preview: String::new(),
            stdout_tail: stdout_tail.to_string(),
            stderr_tail: String::new(),
            stdout_bytes: stdout_tail.len(),
            stderr_bytes: 0,
            truncated_for_display: false,
            truncated_for_agent: false,
            log_ref: None,
            message: None,
        }
    }
}
