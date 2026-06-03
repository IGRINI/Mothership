//! Unified tool-call orchestration — one lifecycle for every tool kind.
//!
//! ```text
//! queued → classify → preflight → guard → policy/approval
//!        → [resource lease]? → started → execute (backend) → terminal
//! ```
//! `?` = optional, requested by the call's [`ToolCapability`] (a process needs a
//! lease; a typed file tool does not).
//!
//! The orchestrator owns the cross-cuts; a [`ToolBackend`] provides only the
//! kind-specific work, split into clear concerns:
//! - `classify` declares **what** the call wants (intent + touched paths +
//!   resources) — it carries no allow/deny decision;
//! - `preflight` rejects malformed arguments *before* an approval prompt;
//! - `guard` is a pre-policy short-circuit (e.g. repeat suppression → a
//!   `LoopBlocked` terminal) that runs before any approval or execution;
//! - `decide` is the **policy** step (Allow / Ask / Deny / Reject);
//! - `preview` is a side-effect-free approval card (diff/summary);
//! - `acquire` takes a resource lease (held across `execute`) when the
//!   capability asks for one;
//! - `execute` does the actual work (spawn+stream+wait for a process; a pure
//!   handler for a typed tool) and returns an already-bounded outcome;
//! - `record` is post-terminal bookkeeping (the repeat guard) — the orchestrator
//!   calls it ONLY for an executed outcome, never for a pre-execution terminal.
//!
//! The orchestrator emits [`ToolExecutionEvent`]s to the sink and NEVER touches
//! storage / redaction / UI directly — the sink fans those out. Backends are
//! responsible for bounding their own output (spilling to a `log_ref`), so the
//! orchestrator only ever moves bounded payloads.

use std::sync::Arc;

use serde_json::Value;

use crate::{LlmToolCallResult, Result};

use super::permissions::{ToolApprovalDecision, ToolApprovalGate};
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
    /// Refuse with a fully-formed typed terminal outcome (e.g. a write
    /// precondition failure that must carry its own `status`/payload). Emitted as
    /// the terminal directly — no approval, no execute. This is how a policy step
    /// rejects a call before approval while preserving a typed payload.
    Reject(Box<BackendOutcome>),
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
#[derive(Debug, Clone)]
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

/// An opaque RAII lease the orchestrator holds across `execute`; dropping it
/// releases whatever the backend acquired (e.g. resource-gate permits). A no-op
/// when the backend needs no lease. Kept generic (not process-only) so future
/// backends (LSP, browser, fetch) can request leases without changing the
/// orchestrator.
pub struct ResourceLease(#[allow(dead_code)] Option<Box<dyn Send>>);

impl ResourceLease {
    /// A lease that holds nothing.
    pub fn none() -> Self {
        Self(None)
    }

    /// Wrap an owned guard whose `Drop` releases the resource.
    pub fn new(guard: Box<dyn Send>) -> Self {
        Self(Some(guard))
    }
}

/// The kind-specific seam. The orchestrator runs the shared lifecycle around it.
///
/// Synchronous on purpose: the file backends' output spill bridges to the async
/// output store via `block_on`, so a backend may only `block_on` its own async
/// work (process spawn/wait, store spill) at the leaf — the orchestrator never
/// wraps `execute` in its own runtime, keeping every `block_on` un-nested.
pub trait ToolBackend: Send + Sync {
    /// Declare intent + touched paths + resource needs. No allow/deny decision.
    fn classify(&self, ctx: &ToolCallContext<'_>) -> Result<ToolCapability>;

    /// Reject malformed arguments before an approval prompt. Default: accept.
    fn preflight(&self, _ctx: &ToolCallContext<'_>) -> Result<()> {
        Ok(())
    }

    /// A pre-policy guard run before approval: return a terminal outcome (e.g. a
    /// `LoopBlocked` repeat suppression) to short-circuit the call without asking
    /// or executing, or `None` to proceed. Default: proceed.
    fn guard(&self, _ctx: &ToolCallContext<'_>) -> Option<BackendOutcome> {
        None
    }

    /// Apply policy to a classified call → Allow / Ask / Deny / Reject.
    fn decide(&self, ctx: &ToolCallContext<'_>, capability: &ToolCapability) -> ToolDecision;

    /// A side-effect-free preview for the approval card. Default: none.
    fn preview(&self, _ctx: &ToolCallContext<'_>, _capability: &ToolCapability) -> ApprovalPreview {
        ApprovalPreview::default()
    }

    /// Acquire a resource lease before execution — called only when the
    /// capability's `resource_request` asks for one. The returned
    /// [`ResourceLease`] is held by the orchestrator across `execute` and dropped
    /// after, so any permits/guards inside it stay held for the call's duration.
    /// Default: no lease.
    fn acquire(
        &self,
        _ctx: &ToolCallContext<'_>,
        _sink: &Arc<dyn ToolExecutionEventSink>,
    ) -> Result<ResourceLease> {
        Ok(ResourceLease::none())
    }

    /// Execute after approval; may stream `Output` events through `sink`. Returns
    /// an already-bounded outcome.
    fn execute(
        &self,
        ctx: &ToolCallContext<'_>,
        sink: &Arc<dyn ToolExecutionEventSink>,
    ) -> Result<BackendOutcome>;

    /// Record the terminal outcome for cross-call bookkeeping (e.g. the repeat
    /// guard's history). Called on every terminal — including denials and
    /// cancellations — so the backend decides what is worth remembering. Default:
    /// noop.
    fn record(&self, _ctx: &ToolCallContext<'_>, _outcome: &BackendOutcome) {}
}

/// Drives the unified lifecycle for any [`ToolBackend`]. Owns the cross-cuts
/// (approval gate today; resource lease / repeat guard attach here as backends
/// that need them are migrated).
pub struct ToolOrchestrator {
    approval_gate: Arc<dyn ToolApprovalGate>,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl ToolOrchestrator {
    pub fn new(
        approval_gate: Arc<dyn ToolApprovalGate>,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Self {
        Self {
            approval_gate,
            runtime,
        }
    }

    /// Run one tool call through the full lifecycle. Always resolves to a
    /// model-facing [`LlmToolCallResult`]; lifecycle events are emitted to `sink`.
    pub fn run(
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

        // guard — a pre-policy short-circuit (e.g. repeat-suppression) emitted as
        // a terminal without asking or executing.
        if let Some(outcome) = backend.guard(&ctx) {
            return self.terminal(&ctx, project_id, sink, outcome);
        }

        // policy → approval.
        match backend.decide(&ctx, &capability) {
            ToolDecision::Reject(outcome) => {
                return self.terminal(&ctx, project_id, sink, *outcome);
            }
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
                let decision = self.runtime.block_on(
                    self.approval_gate
                        .request_decision(ctx.tool_call_id, ctx.cancellation),
                );
                // A cancel (chat or tool) unblocks the gate as a Denied — but it is
                // a cancellation, not a refusal. Check the token BEFORE mapping the
                // decision so the terminal is Cancelled, and (being pre-execute) it
                // never reaches the repeat guard.
                if ctx.cancellation.is_cancelled() || ctx.chat_cancellation.is_cancelled() {
                    return self.terminal(
                        &ctx,
                        project_id,
                        sink,
                        cancelled_outcome(ctx.tool_call_id, "tool call was cancelled"),
                    );
                }
                if let ToolApprovalDecision::Denied { reason } = decision {
                    return self.denied(&ctx, project_id, sink, reason, &capability);
                }
            }
            ToolDecision::Allow => {}
        }

        // A chat cancelled after an Allow (or otherwise before execute) is a
        // cancellation rather than running the side effect.
        if ctx.cancellation.is_cancelled() || ctx.chat_cancellation.is_cancelled() {
            return self.terminal(
                &ctx,
                project_id,
                sink,
                cancelled_outcome(ctx.tool_call_id, "tool call was cancelled"),
            );
        }

        // resource lease — acquired only when the capability asks for one (e.g. a
        // process needs a concurrency/git lease; a typed file tool does not). Held
        // across execute and dropped when this call returns, so its permits stay
        // for the whole side effect.
        let _lease = if capability
            .resource_request
            .as_ref()
            .is_some_and(|request| request.needs_lease)
        {
            match backend.acquire(&ctx, sink) {
                Ok(lease) => lease,
                Err(error) => {
                    let outcome = if ctx.cancellation.is_cancelled()
                        || ctx.chat_cancellation.is_cancelled()
                    {
                        cancelled_outcome(
                            ctx.tool_call_id,
                            "tool call cancelled while waiting for resources",
                        )
                    } else {
                        terminal_outcome(
                            ctx.tool_call_id,
                            ToolExecutionStatus::Failed,
                            error.to_string(),
                        )
                    };
                    return self.terminal(&ctx, project_id, sink, outcome);
                }
            }
        } else {
            ResourceLease::none()
        };

        emit(
            ToolExecutionEventKind::Started,
            Some(capability.summary.clone()),
        );

        // ONLY an executed outcome (Ok, or a spawn/IO failure synthesized here) is
        // recorded for cross-call bookkeeping. Pre-execution terminals — cancel,
        // guard, reject, denial, lease failure — must NOT feed the repeat guard, so
        // a user's repeated *denial* is never suppressed as a repeated *execution*.
        // `_lease` is held through execute + terminal, then dropped on return.
        let outcome = match backend.execute(&ctx, sink) {
            Ok(outcome) => outcome,
            Err(error) => {
                let mut outcome = terminal_outcome(
                    ctx.tool_call_id,
                    ToolExecutionStatus::Failed,
                    error.to_string(),
                );
                outcome.touched_paths = capability.touched_paths;
                outcome
            }
        };
        backend.record(&ctx, &outcome);
        self.terminal(&ctx, project_id, sink, outcome)
    }

    /// Emit the terminal event for an outcome and return the model-facing result.
    /// Does NOT record — recording is the executed path's job (see `run`), so
    /// pre-execution terminals never reach the repeat guard.
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
        let mut outcome =
            terminal_outcome(ctx.tool_call_id, ToolExecutionStatus::PermissionDenied, reason);
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
        let mut outcome = terminal_outcome(ctx.tool_call_id, ToolExecutionStatus::Failed, message);
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
    use crate::{ChatCancellationToken, PendingToolApprovalGate, ToolCancellationToken, ToolKind};
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
        fn execute(
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
        let runtime = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let gate = PendingToolApprovalGate::new();
        let gate_dyn: Arc<dyn ToolApprovalGate> = gate.clone();
        let orchestrator = ToolOrchestrator::new(gate_dyn, Arc::clone(&runtime));
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

        // Resolve a pending approval from another thread, if requested (the
        // orchestrator blocks on the gate while it runs).
        if let Some(decision) = decide_after {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    if gate.decide("tc", decision.clone()) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
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
        let result = orchestrator.run(ctx, Some("project"), &backend, &sink);
        let kinds = recording.kinds();
        let executed = *executed.lock().unwrap();
        (result, kinds, executed)
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

    #[test]
    fn cancel_during_approval_is_cancelled_not_denied() {
        // The gate returns Denied when the call is cancelled; the orchestrator
        // must still report a *cancellation*, not a refusal, and not execute.
        let runtime = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let gate = PendingToolApprovalGate::new();
        let gate_dyn: Arc<dyn ToolApprovalGate> = gate.clone();
        let orchestrator = ToolOrchestrator::new(gate_dyn, Arc::clone(&runtime));
        let recording = Arc::new(RecordingSink::default());
        let sink: Arc<dyn ToolExecutionEventSink> = recording.clone();
        let executed = Arc::new(Mutex::new(false));
        let backend = FakeBackend {
            decision: ToolDecision::Ask {
                reason: "approve?".to_string(),
            },
            preflight_ok: true,
            executed: Arc::clone(&executed),
        };
        let cancellation = ToolCancellationToken::default();
        let chat = ChatCancellationToken::default();
        let arguments = json!({});

        // Cancel the call once the approval request has been emitted (the gate is
        // then blocked waiting for a decision).
        let canceller = {
            let recording = Arc::clone(&recording);
            let cancellation = cancellation.clone();
            std::thread::spawn(move || {
                for _ in 0..500 {
                    if recording
                        .kinds()
                        .contains(&ToolExecutionEventKind::PermissionRequested)
                    {
                        cancellation.cancel();
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            })
        };

        let ctx = ToolCallContext {
            tool_call_id: "tc",
            run_id: Some("run"),
            tool_name: "run_command",
            kind: ToolKind::RunCommand,
            arguments: &arguments,
            cancellation: &cancellation,
            chat_cancellation: &chat,
        };
        let result = orchestrator.run(ctx, Some("project"), &backend, &sink);
        canceller.join().unwrap();

        assert!(!result.ok);
        assert!(!*executed.lock().unwrap(), "cancelled call must not execute");
        let kinds = recording.kinds();
        assert!(kinds.contains(&ToolExecutionEventKind::PermissionRequested));
        assert!(
            kinds.contains(&ToolExecutionEventKind::Cancelled),
            "cancel during approval must terminate as Cancelled: {kinds:?}"
        );
        assert!(
            !kinds.contains(&ToolExecutionEventKind::PermissionDenied),
            "cancel must not be reported as a denial: {kinds:?}"
        );
    }
}
