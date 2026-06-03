//! Unified tool-call orchestration — one lifecycle for every tool kind.
//!
//! ```text
//! queued → classify → preflight → policy/approval
//!        → [resource lease / repeat guard]* → started
//!        → execute (backend) → terminal
//! ```
//! `*` = optional, requested by the call's [`ToolCapability`] (e.g. a process
//! needs a lease; a typed file tool does not).
//!
//! The orchestrator owns the cross-cuts; a [`ToolBackend`] provides only the
//! kind-specific work, split into clear concerns:
//! - `classify` declares **what** the call wants (intent + touched paths +
//!   resources) — it carries no allow/deny decision;
//! - `preflight` rejects malformed arguments *before* an approval prompt;
//! - `decide` is the **policy** step (Allow / Ask / Deny);
//! - `preview` is a side-effect-free approval card (diff/summary);
//! - `execute` does the actual work (spawn+stream+wait for a process; a pure
//!   handler for a typed tool) and returns an already-bounded outcome.
//!
//! The orchestrator emits [`ToolExecutionEvent`]s to the sink and NEVER touches
//! storage / redaction / UI directly — the sink fans those out. Backends are
//! responsible for bounding their own output (spilling to a `log_ref`), so the
//! orchestrator only ever moves bounded payloads.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::{LlmToolCallResult, Result};

use super::permissions::{PendingToolApprovalGate, ToolApprovalDecision};
use super::pipeline::ToolCallContext;
use super::types::{
    ToolArtifact, ToolExecutionEvent, ToolExecutionEventKind, ToolExecutionEventSink,
    ToolExecutionResult, ToolExecutionStatus,
};

/// What a tool call intends to do and the resources it touches. A pure
/// description — it carries NO allow/ask/deny decision (that is [`ToolDecision`],
/// produced by the policy step).
#[derive(Debug, Clone, Default)]
pub struct ToolCapability {
    /// Human-readable intent (shown as the call summary).
    pub summary: String,
    /// Workspace-relative paths the call touches.
    pub touched_paths: Vec<String>,
    /// What runtime resources the call needs before executing, if any. Kept on
    /// the capability (not hardcoded as "process-only") so future backends (LSP,
    /// browser, fetch) can request a lease without the orchestrator changing.
    pub resource_request: Option<ResourceRequest>,
}

/// A backend's declared resource need. Today only a process lease is modeled;
/// the type is the seam for future resource kinds.
#[derive(Debug, Clone, Default)]
pub struct ResourceRequest {
    /// The call needs a concurrency/process lease acquired before it runs.
    pub needs_lease: bool,
}

/// The policy decision for a classified call. Distinct from classification.
#[derive(Debug, Clone)]
pub enum ToolDecision {
    /// Run without asking.
    Allow,
    /// Ask the human first; `reason` explains why.
    Ask { reason: String },
    /// Refuse; `reason` explains why.
    Deny { reason: String },
}

/// A side-effect-free approval preview rendered before the human approves.
#[derive(Debug, Clone, Default)]
pub struct ApprovalPreview {
    /// Summary (+ optional inline diff) message for the approval card.
    pub message: String,
    /// Typed artifacts (e.g. a `diff-preview`) for the approval card.
    pub artifacts: Vec<ToolArtifact>,
}

/// The terminal outcome a backend's `execute` produces. Already bounded by the
/// backend (large content referenced via artifact `log_ref`, never inline). The
/// orchestrator turns this into the terminal event + model-facing result.
pub struct BackendOutcome {
    pub status: ToolExecutionStatus,
    /// The full result record (process result, or a synthesized file result).
    pub result: ToolExecutionResult,
    /// Model-facing text (what the agent loop receives).
    pub model_text: String,
    /// Semantic payload for typed storage / UI cards.
    pub payload: Option<Value>,
    /// Paths the call touched (for the terminal event).
    pub touched_paths: Vec<String>,
    /// Durable artifacts (diff / output / results).
    pub artifacts: Vec<ToolArtifact>,
}

/// The kind-specific seam. The orchestrator runs the shared lifecycle around it.
#[async_trait]
pub trait ToolBackend: Send + Sync {
    /// Declare intent + touched paths + resource needs. No allow/deny decision.
    fn classify(&self, ctx: &ToolCallContext<'_>) -> Result<ToolCapability>;

    /// Reject malformed arguments before an approval prompt. Default: accept.
    fn preflight(&self, _ctx: &ToolCallContext<'_>) -> Result<()> {
        Ok(())
    }

    /// Apply policy to a classified call → Allow / Ask / Deny.
    fn decide(&self, ctx: &ToolCallContext<'_>, capability: &ToolCapability) -> ToolDecision;

    /// A side-effect-free preview for the approval card. Default: none.
    fn preview(&self, _ctx: &ToolCallContext<'_>, _capability: &ToolCapability) -> ApprovalPreview {
        ApprovalPreview::default()
    }

    /// Execute after approval; may stream `Output` events through `sink`. Returns
    /// an already-bounded outcome.
    async fn execute(
        &self,
        ctx: &ToolCallContext<'_>,
        sink: &Arc<dyn ToolExecutionEventSink>,
    ) -> Result<BackendOutcome>;
}

/// Drives the unified lifecycle for any [`ToolBackend`]. Owns the cross-cuts
/// (approval gate today; resource lease / repeat guard attach here as backends
/// that need them are migrated).
pub struct ToolOrchestrator {
    approval_gate: Arc<PendingToolApprovalGate>,
}

impl ToolOrchestrator {
    pub fn new(approval_gate: Arc<PendingToolApprovalGate>) -> Self {
        Self { approval_gate }
    }

    /// Run one tool call through the full lifecycle. Always resolves to a
    /// model-facing [`LlmToolCallResult`]; lifecycle events are emitted to `sink`.
    pub async fn run(
        &self,
        ctx: ToolCallContext<'_>,
        project_id: Option<&str>,
        backend: &dyn ToolBackend,
        sink: &Arc<dyn ToolExecutionEventSink>,
    ) -> LlmToolCallResult {
        let emit = |kind, message: Option<String>| {
            sink.emit(ToolExecutionEvent {
                tool_call_id: ctx.tool_call_id.to_string(),
                run_id: ctx.run_id.map(str::to_string),
                project_id: project_id.map(str::to_string),
                kind,
                message,
                tool_kind: Some(ctx.kind),
                ..Default::default()
            });
        };

        emit(ToolExecutionEventKind::Queued, None);

        if ctx.cancellation.is_cancelled() || ctx.chat_cancellation.is_cancelled() {
            return self.terminal(
                &ctx,
                project_id,
                sink,
                cancelled_outcome(ctx.tool_call_id, "tool call was cancelled before it started"),
            );
        }

        // classify — intent + touched paths + resource needs (no decision).
        let capability = match backend.classify(&ctx) {
            Ok(capability) => capability,
            Err(error) => return self.fail(&ctx, project_id, sink, error.to_string(), Vec::new()),
        };

        // preflight — reject malformed args BEFORE asking for approval.
        if let Err(error) = backend.preflight(&ctx) {
            return self.fail(
                &ctx,
                project_id,
                sink,
                error.to_string(),
                capability.touched_paths.clone(),
            );
        }

        // policy → approval.
        match backend.decide(&ctx, &capability) {
            ToolDecision::Deny { reason } => {
                return self.denied(&ctx, project_id, sink, reason, &capability);
            }
            ToolDecision::Ask { reason } => {
                let preview = backend.preview(&ctx, &capability);
                let message = if preview.message.is_empty() {
                    reason
                } else {
                    preview.message
                };
                sink.emit(ToolExecutionEvent {
                    tool_call_id: ctx.tool_call_id.to_string(),
                    run_id: ctx.run_id.map(str::to_string),
                    project_id: project_id.map(str::to_string),
                    kind: ToolExecutionEventKind::PermissionRequested,
                    message: Some(message),
                    tool_kind: Some(ctx.kind),
                    touched_paths: capability.touched_paths.clone(),
                    artifacts: preview.artifacts,
                    ..Default::default()
                });
                if let ToolApprovalDecision::Denied { reason } = self
                    .approval_gate
                    .request_decision(ctx.tool_call_id, ctx.cancellation)
                    .await
                {
                    return self.denied(&ctx, project_id, sink, reason, &capability);
                }
            }
            ToolDecision::Allow => {}
        }

        // A chat cancelled during approval is reported as a cancellation rather
        // than running the side effect.
        if ctx.cancellation.is_cancelled() || ctx.chat_cancellation.is_cancelled() {
            return self.terminal(
                &ctx,
                project_id,
                sink,
                cancelled_outcome(ctx.tool_call_id, "tool call was cancelled"),
            );
        }

        // NOTE: resource lease (capability.resource_request) + repeat guard are
        // command-only cross-cuts; they attach here when run_command migrates onto
        // the orchestrator.

        emit(
            ToolExecutionEventKind::Started,
            Some(capability.summary.clone()),
        );

        match backend.execute(&ctx, sink).await {
            Ok(outcome) => self.terminal(&ctx, project_id, sink, outcome),
            Err(error) => self.fail(
                &ctx,
                project_id,
                sink,
                error.to_string(),
                capability.touched_paths,
            ),
        }
    }

    /// Emit the terminal event for an outcome and return the model-facing result.
    fn terminal(
        &self,
        ctx: &ToolCallContext<'_>,
        project_id: Option<&str>,
        sink: &Arc<dyn ToolExecutionEventSink>,
        outcome: BackendOutcome,
    ) -> LlmToolCallResult {
        let ok = outcome.status == ToolExecutionStatus::Completed;
        sink.emit(ToolExecutionEvent {
            tool_call_id: ctx.tool_call_id.to_string(),
            run_id: ctx.run_id.map(str::to_string),
            project_id: project_id.map(str::to_string),
            kind: terminal_kind(outcome.status),
            message: outcome.result.message.clone(),
            result: Some(outcome.result),
            tool_kind: Some(ctx.kind),
            payload: outcome.payload,
            touched_paths: outcome.touched_paths,
            artifacts: outcome.artifacts,
            ..Default::default()
        });
        LlmToolCallResult {
            ok,
            content: outcome.model_text,
        }
    }

    fn denied(
        &self,
        ctx: &ToolCallContext<'_>,
        project_id: Option<&str>,
        sink: &Arc<dyn ToolExecutionEventSink>,
        reason: String,
        capability: &ToolCapability,
    ) -> LlmToolCallResult {
        let mut outcome = terminal_outcome(
            ctx.tool_call_id,
            ToolExecutionStatus::PermissionDenied,
            reason.clone(),
        );
        outcome.touched_paths = capability.touched_paths.clone();
        self.terminal(ctx, project_id, sink, outcome)
    }

    fn fail(
        &self,
        ctx: &ToolCallContext<'_>,
        project_id: Option<&str>,
        sink: &Arc<dyn ToolExecutionEventSink>,
        message: String,
        touched_paths: Vec<String>,
    ) -> LlmToolCallResult {
        let mut outcome =
            terminal_outcome(ctx.tool_call_id, ToolExecutionStatus::Failed, message);
        outcome.touched_paths = touched_paths;
        self.terminal(ctx, project_id, sink, outcome)
    }
}

fn terminal_kind(status: ToolExecutionStatus) -> ToolExecutionEventKind {
    match status {
        ToolExecutionStatus::Completed => ToolExecutionEventKind::Completed,
        ToolExecutionStatus::Failed => ToolExecutionEventKind::Failed,
        ToolExecutionStatus::Cancelled => ToolExecutionEventKind::Cancelled,
        ToolExecutionStatus::TimedOut => ToolExecutionEventKind::TimedOut,
        ToolExecutionStatus::PermissionDenied => ToolExecutionEventKind::PermissionDenied,
        ToolExecutionStatus::LoopBlocked => ToolExecutionEventKind::LoopBlocked,
    }
}

/// A bare terminal outcome carrying only a status + message (no output) — used
/// for cancelled/denied/failed terminals.
fn terminal_outcome(
    tool_call_id: &str,
    status: ToolExecutionStatus,
    message: String,
) -> BackendOutcome {
    BackendOutcome {
        status,
        result: ToolExecutionResult {
            tool_call_id: tool_call_id.to_string(),
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
            message: Some(message.clone()),
        },
        model_text: message,
        payload: None,
        touched_paths: Vec::new(),
        artifacts: Vec::new(),
    }
}

fn cancelled_outcome(tool_call_id: &str, message: &str) -> BackendOutcome {
    terminal_outcome(
        tool_call_id,
        ToolExecutionStatus::Cancelled,
        message.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatCancellationToken, ToolCancellationToken, ToolKind};
    use serde_json::json;
    use std::sync::Mutex;

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

    /// A configurable fake backend that records whether `execute` ran.
    struct FakeBackend {
        decision: ToolDecision,
        preflight_ok: bool,
        executed: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl ToolBackend for FakeBackend {
        fn classify(&self, _ctx: &ToolCallContext<'_>) -> Result<ToolCapability> {
            Ok(ToolCapability {
                summary: "fake".to_string(),
                touched_paths: vec!["a.txt".to_string()],
                resource_request: None,
            })
        }
        fn preflight(&self, _ctx: &ToolCallContext<'_>) -> Result<()> {
            if self.preflight_ok {
                Ok(())
            } else {
                Err(crate::MothershipError::InvalidRequest("malformed".to_string()))
            }
        }
        fn decide(&self, _ctx: &ToolCallContext<'_>, _cap: &ToolCapability) -> ToolDecision {
            self.decision.clone()
        }
        async fn execute(
            &self,
            ctx: &ToolCallContext<'_>,
            _sink: &Arc<dyn ToolExecutionEventSink>,
        ) -> Result<BackendOutcome> {
            *self.executed.lock().unwrap() = true;
            let mut outcome = terminal_outcome(
                ctx.tool_call_id,
                ToolExecutionStatus::Completed,
                "done".to_string(),
            );
            outcome.model_text = "ok".to_string();
            Ok(outcome)
        }
    }

    fn run_fake(
        decision: ToolDecision,
        preflight_ok: bool,
        decide_after: Option<ToolApprovalDecision>,
    ) -> (LlmToolCallResult, Vec<ToolExecutionEventKind>, bool) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let gate = PendingToolApprovalGate::new();
            let orchestrator = ToolOrchestrator::new(Arc::clone(&gate));
            let recording = Arc::new(RecordingSink::default());
            let sink: Arc<dyn ToolExecutionEventSink> = recording.clone();
            let executed = Arc::new(Mutex::new(false));
            let backend = FakeBackend {
                decision,
                preflight_ok,
                executed: Arc::clone(&executed),
            };
            let cancellation = ToolCancellationToken::default();
            let chat = ChatCancellationToken::default();
            let arguments = json!({});

            // Resolve a pending approval from another task, if requested.
            if let Some(decision) = decide_after {
                let gate = Arc::clone(&gate);
                tokio::spawn(async move {
                    for _ in 0..200 {
                        if gate.decide("tc", decision.clone()) {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                });
            }

            let ctx = ToolCallContext {
                tool_call_id: "tc",
                run_id: Some("run"),
                tool_name: "run_command",
                kind: ToolKind::RunCommand,
                arguments: &arguments,
                cancellation: &cancellation,
                chat_cancellation: &chat,
            };
            let result = orchestrator
                .run(ctx, Some("project"), &backend, &sink)
                .await;
            let kinds = recording.kinds();
            let executed = *executed.lock().unwrap();
            (result, kinds, executed)
        })
    }

    #[test]
    fn allow_runs_without_approval() {
        let (result, kinds, executed) = run_fake(ToolDecision::Allow, true, None);
        assert!(result.ok);
        assert!(executed);
        assert!(kinds.contains(&ToolExecutionEventKind::Queued));
        assert!(kinds.contains(&ToolExecutionEventKind::Started));
        assert!(kinds.contains(&ToolExecutionEventKind::Completed));
        assert!(!kinds.contains(&ToolExecutionEventKind::PermissionRequested));
    }

    #[test]
    fn deny_does_not_execute() {
        let (result, kinds, executed) = run_fake(
            ToolDecision::Deny {
                reason: "nope".to_string(),
            },
            true,
            None,
        );
        assert!(!result.ok);
        assert!(!executed, "denied call must not execute");
        assert!(kinds.contains(&ToolExecutionEventKind::PermissionDenied));
        assert!(!kinds.contains(&ToolExecutionEventKind::Started));
    }

    #[test]
    fn ask_then_approved_executes() {
        let (result, kinds, executed) = run_fake(
            ToolDecision::Ask {
                reason: "approve?".to_string(),
            },
            true,
            Some(ToolApprovalDecision::Approved),
        );
        assert!(result.ok);
        assert!(executed);
        assert!(kinds.contains(&ToolExecutionEventKind::PermissionRequested));
        assert!(kinds.contains(&ToolExecutionEventKind::Completed));
    }

    #[test]
    fn ask_then_denied_does_not_execute() {
        let (result, kinds, executed) = run_fake(
            ToolDecision::Ask {
                reason: "approve?".to_string(),
            },
            true,
            Some(ToolApprovalDecision::Denied {
                reason: "user said no".to_string(),
            }),
        );
        assert!(!result.ok);
        assert!(!executed);
        assert!(kinds.contains(&ToolExecutionEventKind::PermissionRequested));
        assert!(kinds.contains(&ToolExecutionEventKind::PermissionDenied));
    }

    #[test]
    fn malformed_preflight_fails_before_approval() {
        let (result, kinds, executed) = run_fake(
            ToolDecision::Ask {
                reason: "approve?".to_string(),
            },
            false,
            None,
        );
        assert!(!result.ok);
        assert!(!executed);
        assert!(
            !kinds.contains(&ToolExecutionEventKind::PermissionRequested),
            "preflight failure must not ask for approval"
        );
        assert!(kinds.contains(&ToolExecutionEventKind::Failed));
    }
}
