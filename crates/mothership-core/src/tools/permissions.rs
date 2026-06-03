use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::Result;

use super::cancellation::ToolCancellationToken;
use super::types::{ToolCommand, ToolExecutionRequest};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolPermissionEvaluation {
    pub action: ToolPermissionAction,
    pub reason: String,
}

pub trait ToolPermissionPolicy: Send + Sync {
    fn evaluate(&self, request: &ToolExecutionRequest) -> ToolPermissionEvaluation;
}

#[derive(Debug, Default)]
pub struct ConservativeCommandPermissionPolicy;

impl ToolPermissionPolicy for ConservativeCommandPermissionPolicy {
    fn evaluate(&self, request: &ToolExecutionRequest) -> ToolPermissionEvaluation {
        classify_command(&request.command)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalDecision {
    Approved,
    Denied { reason: String },
}

#[async_trait::async_trait]
pub trait ToolApprovalGate: Send + Sync {
    async fn request_approval(
        &self,
        request: &ToolExecutionRequest,
        evaluation: &ToolPermissionEvaluation,
        cancellation: &ToolCancellationToken,
    ) -> Result<ToolApprovalDecision>;

    /// Await an approval decision keyed only by `tool_call_id`. The orchestrator
    /// has already emitted the `PermissionRequested` event (with the preview), so
    /// the gate only blocks until the UI resolves this id (or the call is
    /// cancelled). This is the unified entry point used for every tool kind —
    /// unlike [`request_approval`](Self::request_approval) it needs no
    /// [`ToolExecutionRequest`], so typed file tools use it too.
    async fn request_decision(
        &self,
        tool_call_id: &str,
        cancellation: &ToolCancellationToken,
    ) -> ToolApprovalDecision;
}

#[derive(Debug, Default)]
pub struct PendingToolApprovalGate {
    pending: Mutex<HashMap<String, oneshot::Sender<ToolApprovalDecision>>>,
}

impl PendingToolApprovalGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn decide(&self, tool_call_id: &str, decision: ToolApprovalDecision) -> bool {
        let Some(sender) = self.pending.lock().unwrap().remove(tool_call_id) else {
            return false;
        };
        sender.send(decision).is_ok()
    }
}

#[async_trait::async_trait]
impl ToolApprovalGate for PendingToolApprovalGate {
    async fn request_approval(
        &self,
        request: &ToolExecutionRequest,
        _evaluation: &ToolPermissionEvaluation,
        cancellation: &ToolCancellationToken,
    ) -> Result<ToolApprovalDecision> {
        Ok(self
            .request_decision(&request.tool_call_id, cancellation)
            .await)
    }

    /// Await a decision through the same pending map that [`decide`](Self::decide)
    /// resolves: register a waiter keyed by `tool_call_id` and block until the UI
    /// calls `decide` (or the call is cancelled).
    async fn request_decision(
        &self,
        tool_call_id: &str,
        cancellation: &ToolCancellationToken,
    ) -> ToolApprovalDecision {
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.contains_key(tool_call_id) {
                return ToolApprovalDecision::Denied {
                    reason: "approval is already pending for this tool call".to_string(),
                };
            }
            pending.insert(tool_call_id.to_string(), sender);
        }

        let decision = tokio::select! {
            decision = receiver => decision.unwrap_or(ToolApprovalDecision::Denied {
                reason: "approval channel closed".to_string(),
            }),
            _ = cancellation.cancelled() => ToolApprovalDecision::Denied {
                reason: "tool call was cancelled before approval".to_string(),
            },
        };

        self.pending.lock().unwrap().remove(tool_call_id);
        decision
    }
}

#[derive(Debug)]
pub struct StaticToolApprovalGate {
    decision: ToolApprovalDecision,
}

impl StaticToolApprovalGate {
    pub fn approve() -> Arc<Self> {
        Arc::new(Self {
            decision: ToolApprovalDecision::Approved,
        })
    }

    pub fn deny(reason: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            decision: ToolApprovalDecision::Denied {
                reason: reason.into(),
            },
        })
    }
}

#[async_trait::async_trait]
impl ToolApprovalGate for StaticToolApprovalGate {
    async fn request_approval(
        &self,
        _request: &ToolExecutionRequest,
        _evaluation: &ToolPermissionEvaluation,
        cancellation: &ToolCancellationToken,
    ) -> Result<ToolApprovalDecision> {
        Ok(self.request_decision("", cancellation).await)
    }

    async fn request_decision(
        &self,
        _tool_call_id: &str,
        cancellation: &ToolCancellationToken,
    ) -> ToolApprovalDecision {
        if cancellation.is_cancelled() {
            return ToolApprovalDecision::Denied {
                reason: "tool call was cancelled before approval".to_string(),
            };
        }
        self.decision.clone()
    }
}

fn classify_command(command: &ToolCommand) -> ToolPermissionEvaluation {
    let program = normalized(&command.program);
    if program.is_empty() {
        return deny("empty command program");
    }

    if is_hard_blocked_program(program.as_str()) {
        return deny(format!(
            "command `{}` is blocked by default",
            command.program
        ));
    }

    if is_shell(program.as_str()) {
        return ask("shell commands require approval");
    }

    if program == "git" {
        return classify_git(command);
    }

    if is_read_only_program(program.as_str()) {
        return allow("read-only command");
    }

    ask("command is not known to be read-only")
}

fn classify_git(command: &ToolCommand) -> ToolPermissionEvaluation {
    let Some(subcommand) = command.args.first().map(|arg| normalized(arg)) else {
        return ask("git without a subcommand requires approval");
    };
    match subcommand.as_str() {
        "status" | "diff" | "log" | "show" | "branch" | "remote" | "rev-parse" | "ls-files" => {
            allow("read-only git command")
        }
        "clean" | "reset" | "checkout" | "switch" | "restore" | "commit" | "push" | "pull"
        | "merge" | "rebase" => ask("git command can modify repository state"),
        _ => ask("git subcommand is not known to be read-only"),
    }
}

fn is_shell(program: &str) -> bool {
    matches!(
        program,
        "cmd"
            | "cmd.exe"
            | "powershell"
            | "powershell.exe"
            | "pwsh"
            | "pwsh.exe"
            | "bash"
            | "sh"
            | "zsh"
    )
}

fn is_hard_blocked_program(program: &str) -> bool {
    matches!(
        program,
        "format"
            | "format.com"
            | "diskpart"
            | "shutdown"
            | "reboot"
            | "reg"
            | "reg.exe"
            | "bcdedit"
            | "cipher"
    )
}

fn is_read_only_program(program: &str) -> bool {
    matches!(
        program,
        "pwd"
            | "ls"
            | "dir"
            | "cat"
            | "type"
            | "findstr"
            | "where"
            | "whoami"
            | "node"
            | "python"
            | "python3"
    )
}

fn normalized(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase()
}

fn allow(reason: impl Into<String>) -> ToolPermissionEvaluation {
    ToolPermissionEvaluation {
        action: ToolPermissionAction::Allow,
        reason: reason.into(),
    }
}

fn ask(reason: impl Into<String>) -> ToolPermissionEvaluation {
    ToolPermissionEvaluation {
        action: ToolPermissionAction::Ask,
        reason: reason.into(),
    }
}

fn deny(reason: impl Into<String>) -> ToolPermissionEvaluation {
    ToolPermissionEvaluation {
        action: ToolPermissionAction::Deny,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::tools::{ToolCommand, ToolExecutionRequest, ToolOutputPolicy};

    #[tokio::test]
    async fn pending_gate_resolves_after_decision() {
        let gate = PendingToolApprovalGate::new();
        let request = request("tool_approval_1");
        let cancellation = ToolCancellationToken::default();
        let evaluation = ToolPermissionEvaluation {
            action: ToolPermissionAction::Ask,
            reason: "needs approval".to_string(),
        };

        let waiter = {
            let gate = Arc::clone(&gate);
            let request = request.clone();
            let evaluation = evaluation.clone();
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                gate.request_approval(&request, &evaluation, &cancellation)
                    .await
                    .unwrap()
            })
        };

        tokio::task::yield_now().await;
        assert!(gate.decide("tool_approval_1", ToolApprovalDecision::Approved));
        assert_eq!(waiter.await.unwrap(), ToolApprovalDecision::Approved);
    }

    #[tokio::test]
    async fn pending_gate_cancellation_removes_pending_decision() {
        let gate = PendingToolApprovalGate::new();
        let request = request("tool_approval_2");
        let cancellation = ToolCancellationToken::default();
        let evaluation = ToolPermissionEvaluation {
            action: ToolPermissionAction::Ask,
            reason: "needs approval".to_string(),
        };

        let waiter = {
            let gate = Arc::clone(&gate);
            let request = request.clone();
            let evaluation = evaluation.clone();
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                gate.request_approval(&request, &evaluation, &cancellation)
                    .await
                    .unwrap()
            })
        };

        tokio::task::yield_now().await;
        cancellation.cancel();
        assert!(matches!(
            waiter.await.unwrap(),
            ToolApprovalDecision::Denied { .. }
        ));
        assert!(!gate.decide("tool_approval_2", ToolApprovalDecision::Approved));
    }

    fn request(tool_call_id: &str) -> ToolExecutionRequest {
        ToolExecutionRequest {
            tool_call_id: tool_call_id.to_string(),
            run_id: None,
            project_id: Some("project_1".to_string()),
            cwd: None,
            command: ToolCommand {
                program: "npm".to_string(),
                args: vec!["install".to_string()],
                env: BTreeMap::new(),
            },
            timeout_ms: None,
            output_policy: ToolOutputPolicy::default(),
        }
    }
}
