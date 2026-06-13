use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use ts_rs::TS;

use crate::Result;

use super::cancellation::ToolCancellationToken;
use super::filesystem::{is_sensitive_relative, lexically_normalize, path_contains};
use super::types::{ToolCommand, ToolExecutionRequest};
use super::ToolKind;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ToolApprovalMode {
    /// Current conservative behavior: ask for every non-read-only command and
    /// mutating file tool.
    Manual,
    /// Auto-run ordinary writes/commands, but keep prompts for commands that are
    /// likely to destroy data, rewrite repository history, or alter OS state.
    AutoSafe,
    /// No approval prompts. Preflight, cancellation, workspace containment, and
    /// tool-specific runtime guards still run; this mode only bypasses approval.
    Yolo,
}

impl Default for ToolApprovalMode {
    fn default() -> Self {
        Self::Manual
    }
}

/// Live approval modes for the chats whose runs are currently in flight.
///
/// One store is shared process-wide, but every entry is scoped to a chat id so
/// concurrent runs in different chats each enforce their OWN chat's mode — a
/// `yolo` chat must never loosen the gating of a `manual` chat running next to
/// it. Entries are reference-counted by active run ([`begin_run`]) and removed
/// when the last run for the chat ends (its guard drops, including on unwind),
/// so the map stays bounded by the number of in-flight runs. Chats without an
/// entry — and calls with no chat context at all — get `default_mode`.
///
/// [`begin_run`]: Self::begin_run
#[derive(Debug)]
pub struct ToolApprovalModeStore {
    default_mode: ToolApprovalMode,
    chats: Mutex<HashMap<String, ChatModeEntry>>,
}

#[derive(Debug)]
struct ChatModeEntry {
    mode: ToolApprovalMode,
    active_runs: usize,
}

impl Default for ToolApprovalModeStore {
    fn default() -> Self {
        Self {
            default_mode: ToolApprovalMode::default(),
            chats: Mutex::new(HashMap::new()),
        }
    }
}

impl ToolApprovalModeStore {
    pub fn new(default_mode: ToolApprovalMode) -> Arc<Self> {
        Arc::new(Self {
            default_mode,
            chats: Mutex::new(HashMap::new()),
        })
    }

    /// The fallback mode used when a call has no chat context (or the chat has
    /// no run in flight).
    pub fn default_mode(&self) -> ToolApprovalMode {
        self.default_mode
    }

    /// The mode governing tool decisions for `chat_id`. Unknown chats — and
    /// `None` (e.g. the protocol-level `run_command`) — get the default.
    pub fn mode_for_chat(&self, chat_id: Option<&str>) -> ToolApprovalMode {
        let Some(chat_id) = chat_id else {
            return self.default_mode;
        };
        self.chats()
            .get(chat_id)
            .map(|entry| entry.mode)
            .unwrap_or(self.default_mode)
    }

    /// Register a run starting in `chat_id` with the chat's saved mode. The
    /// returned guard keeps the entry alive; dropping it (run finished, failed,
    /// cancelled, or panicked) releases it, removing the entry once the chat
    /// has no other active run.
    pub fn begin_run(
        self: &Arc<Self>,
        chat_id: &str,
        mode: ToolApprovalMode,
    ) -> ToolApprovalRunGuard {
        {
            let mut chats = self.chats();
            let entry = chats.entry(chat_id.to_string()).or_insert(ChatModeEntry {
                mode,
                active_runs: 0,
            });
            entry.mode = mode;
            entry.active_runs += 1;
        }
        ToolApprovalRunGuard {
            store: Arc::clone(self),
            chat_id: chat_id.to_string(),
        }
    }

    /// Live-update the mode of a chat with a run in flight (the composer's
    /// mid-run mode switch), scoped to that chat only. Chats without an active
    /// run are left alone — their next run seeds from the persisted chat state
    /// — so idle chats never accumulate entries. Returns whether an in-flight
    /// entry was updated.
    pub fn update_chat_mode(&self, chat_id: &str, mode: ToolApprovalMode) -> bool {
        match self.chats().get_mut(chat_id) {
            Some(entry) => {
                entry.mode = mode;
                true
            }
            None => false,
        }
    }

    fn end_run(&self, chat_id: &str) {
        let mut chats = self.chats();
        if let Some(entry) = chats.get_mut(chat_id) {
            entry.active_runs = entry.active_runs.saturating_sub(1);
            if entry.active_runs == 0 {
                chats.remove(chat_id);
            }
        }
    }

    /// Recover from a poisoned lock: entries are plain copies (a panic can
    /// never leave one half-written), and approval decisions must keep working
    /// after an unrelated panic was contained by the request isolation layer.
    fn chats(&self) -> std::sync::MutexGuard<'_, HashMap<String, ChatModeEntry>> {
        self.chats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// RAII registration of one active run in a [`ToolApprovalModeStore`]; dropping
/// it ends the run's claim on its chat entry (also on unwind, so a panicking
/// run never leaks a stale mode).
#[derive(Debug)]
pub struct ToolApprovalRunGuard {
    store: Arc<ToolApprovalModeStore>,
    chat_id: String,
}

impl Drop for ToolApprovalRunGuard {
    fn drop(&mut self) {
        self.store.end_run(&self.chat_id);
    }
}

/// User-authored allow/deny rules layered over the built-in command
/// classification and the typed-tool catalog. Persisted in `app_settings` and
/// loaded into a [`ToolPolicyStore`] at sidecar startup; the UI edits them on
/// the Permissions screen. They are advisory overrides — workspace containment,
/// cancellation, and the runtime guards still run regardless.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ToolPolicySettings {
    /// Command program basenames (case-insensitive, extension-insensitive) that
    /// are always auto-approved, skipping the per-command approval prompt.
    pub command_allow: Vec<String>,
    /// Command program basenames that are always blocked outright.
    pub command_deny: Vec<String>,
    /// Wire tool names (`run_command`, `read_file`, …) the agent may not use.
    pub disabled_tools: Vec<String>,
}

impl ToolPolicySettings {
    /// Trim, drop empties, and de-duplicate every list, keeping only tool names
    /// the runtime actually knows. Stored and compared in this canonical form so
    /// lookups are order-, whitespace-, and extension-insensitive.
    pub fn sanitized(self) -> Self {
        Self {
            command_allow: sanitize_program_list(self.command_allow),
            command_deny: sanitize_program_list(self.command_deny),
            disabled_tools: sanitize_tool_list(self.disabled_tools),
        }
    }
}

fn sanitize_program_list(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for item in items {
        let key = program_key(&item);
        if key.is_empty() || !seen.insert(key.clone()) {
            continue;
        }
        out.push(key);
    }
    out
}

fn sanitize_tool_list(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for item in items {
        let name = item.trim().to_ascii_lowercase();
        if ToolKind::from_routable_name(&name).is_none() || !seen.insert(name.clone()) {
            continue;
        }
        out.push(name);
    }
    out
}

/// Reduce a program to a stable comparison key: basename, lowercased, with a
/// trailing executable extension stripped — so `rm`, `RM`, and `rm.exe` all map
/// to the same allow/deny entry.
fn program_key(value: &str) -> String {
    let base = normalized(value);
    for ext in [".exe", ".com", ".bat", ".cmd", ".ps1"] {
        if let Some(stripped) = base.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    base
}

/// Runtime view of [`ToolPolicySettings`], shared between the command permission
/// policy and the typed-tool dispatcher. Updated live when the user saves, so a
/// running session honors new rules without a restart.
#[derive(Debug, Default)]
pub struct ToolPolicyStore {
    settings: Mutex<ToolPolicySettings>,
}

impl ToolPolicyStore {
    pub fn new(settings: ToolPolicySettings) -> Arc<Self> {
        Arc::new(Self {
            settings: Mutex::new(settings.sanitized()),
        })
    }

    pub fn settings(&self) -> ToolPolicySettings {
        self.settings.lock().unwrap().clone()
    }

    /// Replace the rules (sanitizing first) and return the canonical form that
    /// was stored, so the caller can persist exactly what the store now holds.
    pub fn set_settings(&self, settings: ToolPolicySettings) -> ToolPolicySettings {
        let sanitized = settings.sanitized();
        *self.settings.lock().unwrap() = sanitized.clone();
        sanitized
    }

    pub fn is_tool_disabled(&self, kind: ToolKind) -> bool {
        let name = kind.as_str();
        self.settings
            .lock()
            .unwrap()
            .disabled_tools
            .iter()
            .any(|tool| tool == name)
    }

    /// A user-rule override for `program`, or `None` to defer to default policy.
    /// Deny wins over allow when a program appears in both lists.
    pub fn command_decision(&self, program: &str) -> Option<ToolPermissionAction> {
        let key = program_key(program);
        if key.is_empty() {
            return None;
        }
        let settings = self.settings.lock().unwrap();
        if settings.command_deny.iter().any(|item| item == &key) {
            return Some(ToolPermissionAction::Deny);
        }
        if settings.command_allow.iter().any(|item| item == &key) {
            return Some(ToolPermissionAction::Allow);
        }
        None
    }
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
        // The conservative default IS manual-mode semantics (including the
        // read-only argument screen), just without a per-chat mode store.
        command_permission_for_mode(ToolApprovalMode::Manual, request)
    }
}

#[derive(Debug)]
pub struct ModeAwareCommandPermissionPolicy {
    mode: Arc<ToolApprovalModeStore>,
}

impl ModeAwareCommandPermissionPolicy {
    pub fn new(mode: Arc<ToolApprovalModeStore>) -> Self {
        Self { mode }
    }
}

impl ToolPermissionPolicy for ModeAwareCommandPermissionPolicy {
    fn evaluate(&self, request: &ToolExecutionRequest) -> ToolPermissionEvaluation {
        command_permission_for_mode(self.mode.mode_for_chat(request.chat_id.as_deref()), request)
    }
}

/// Mode-aware command policy with the user's allow/deny list layered on top: a
/// denylisted program is refused, an allowlisted one auto-approved, and anything
/// else falls through to the built-in [`command_permission_for_mode`] decision.
#[derive(Debug)]
pub struct UserAwareCommandPermissionPolicy {
    policy: Arc<ToolPolicyStore>,
    mode: Arc<ToolApprovalModeStore>,
}

impl UserAwareCommandPermissionPolicy {
    pub fn new(policy: Arc<ToolPolicyStore>, mode: Arc<ToolApprovalModeStore>) -> Self {
        Self { policy, mode }
    }
}

impl ToolPermissionPolicy for UserAwareCommandPermissionPolicy {
    fn evaluate(&self, request: &ToolExecutionRequest) -> ToolPermissionEvaluation {
        match self.policy.command_decision(&request.command.program) {
            Some(ToolPermissionAction::Deny) => deny("command is on your denylist"),
            Some(ToolPermissionAction::Allow) => allow("command is on your allowlist"),
            _ => command_permission_for_mode(
                self.mode.mode_for_chat(request.chat_id.as_deref()),
                request,
            ),
        }
    }
}

pub fn command_permission_for_mode(
    mode: ToolApprovalMode,
    request: &ToolExecutionRequest,
) -> ToolPermissionEvaluation {
    let program = normalized(&request.command.program);
    if program.is_empty() {
        return deny("empty command program");
    }

    match mode {
        ToolApprovalMode::Manual => {
            let evaluation = classify_command(&request.command);
            // A read-only program's auto-allow is screened in MANUAL mode only:
            // `cat`/`type`/… are safe per se, but an argument can still point
            // the read at a secret (`cat .env`) or outside the workspace. In
            // auto_safe/yolo sensitive reads are an explicitly allowed
            // capability, so the screen does not run there.
            if evaluation.action == ToolPermissionAction::Allow {
                if let Some(reason) = read_only_argument_screen(request) {
                    return ask(reason);
                }
            }
            evaluation
        }
        ToolApprovalMode::AutoSafe => {
            let base = classify_command(&request.command);
            if base.action == ToolPermissionAction::Deny {
                return base;
            }
            if command_requires_manual_approval(&request.command) {
                return ask("dangerous command requires approval in auto mode");
            }
            allow("auto-safe mode approved command")
        }
        ToolApprovalMode::Yolo => allow("yolo mode approved command"),
    }
}

pub fn file_permission_action_for_mode(
    mode: ToolApprovalMode,
    action: ToolPermissionAction,
) -> ToolPermissionAction {
    match (mode, action) {
        (_, ToolPermissionAction::Allow) => ToolPermissionAction::Allow,
        (_, ToolPermissionAction::Deny) => ToolPermissionAction::Deny,
        (ToolApprovalMode::Manual, ToolPermissionAction::Ask) => ToolPermissionAction::Ask,
        (ToolApprovalMode::AutoSafe | ToolApprovalMode::Yolo, ToolPermissionAction::Ask) => {
            ToolPermissionAction::Allow
        }
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

    pub fn decide_all(&self, decision: ToolApprovalDecision) -> usize {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        let count = pending.len();
        for sender in pending.into_values() {
            let _ = sender.send(decision.clone());
        }
        count
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

    if is_script_interpreter(program.as_str()) {
        if is_safe_interpreter_probe(program.as_str(), &command.args) {
            return allow("read-only interpreter probe");
        }
        return ask("script interpreter command requires approval");
    }

    if is_read_only_program(program.as_str()) {
        return allow("read-only command");
    }

    ask("command is not known to be read-only")
}

fn command_requires_manual_approval(command: &ToolCommand) -> bool {
    let program = normalized(&command.program);
    if is_hard_blocked_program(&program) {
        return true;
    }
    if program == "git" {
        return git_requires_manual_approval(command);
    }
    if is_script_interpreter(&program) {
        return !is_safe_interpreter_probe(&program, &command.args);
    }
    if is_shell(&program) {
        return shell_script_requires_manual_approval(command);
    }
    false
}

/// Manual-mode argument screen for auto-allowed read-only programs (`cat`,
/// `type`, `findstr`, `ls`, …): any argument that references a sensitive path
/// (the same component rules the file tools use) or resolves outside the
/// workspace turns the auto-allow into an `Ask`. Bare, non-sensitive words stay
/// auto-allowed — `cat README.md` must not prompt; the goal is catching secret
/// reads and workspace escapes, not harassing the user. Returns the ask reason,
/// or `None` to keep the auto-allow.
fn read_only_argument_screen(request: &ToolExecutionRequest) -> Option<String> {
    if !is_read_only_program(&normalized(&request.command.program)) {
        return None;
    }

    // Containment context: relative arguments resolve against the command's cwd
    // (falling back to the workspace root), and a path is in-bounds when it is
    // under the workspace root OR the (already vetted) cwd — the cwd may sit in
    // the writable tmp root, whose files a read-only program may read. With no
    // context at all, containment cannot be judged and only the sensitive-name
    // screen applies (ambiguity prefers auto-allow).
    let base = request.cwd.as_deref().or(request.workspace_root.as_deref());
    let mut roots: Vec<&Path> = Vec::new();
    roots.extend(request.workspace_root.as_deref());
    roots.extend(request.cwd.as_deref());

    request
        .command
        .args
        .iter()
        .find_map(|argument| screen_read_only_argument(argument, base, &roots))
}

/// Screen one argument of a read-only program. Flag tokens are skipped; a token
/// whose path components match the sensitive rules asks; a path-like token that
/// resolves (lexically, against `base`) outside every allowed root asks; bare
/// non-path words are tested against the sensitive names only (catching
/// `cat .env`) and otherwise stay auto-allowed.
fn screen_read_only_argument(
    argument: &str,
    base: Option<&Path>,
    roots: &[&Path],
) -> Option<String> {
    let token = argument.trim().trim_matches(|ch| matches!(ch, '"' | '\''));
    if token.is_empty() {
        return None;
    }
    // Flags: `-…` always; a `/x` Windows switch only when nothing after it looks
    // like a path separator (so `/etc/passwd` is not mistaken for a switch).
    if token.starts_with('-') {
        return None;
    }
    if let Some(rest) = token.strip_prefix('/') {
        if !rest.contains(['/', '\\']) {
            return None;
        }
    }

    // Judge the same shape on every platform: fold `\` to `/` so a backslash
    // path embedded in one token cannot dodge `..` resolution on non-Windows.
    // Conservative — it can only over-ask on an exotic filename containing a
    // literal backslash, never under-ask.
    let token_normalized = token.replace('\\', "/");
    let token_path = Path::new(&token_normalized);

    // Sensitive names anywhere in the token's components — catches a bare
    // `.env`, a relative `config/.ssh/id_rsa`, and an absolute
    // `C:\Users\me\.ssh\id_rsa` alike, reusing the file tools' single source of
    // truth for what counts as sensitive.
    if is_sensitive_relative(token_path) {
        return Some(format!(
            "`{token}` references a sensitive path — reading it requires approval in manual mode"
        ));
    }

    // Home-anchored references live outside the workspace by definition.
    if token == "~" || token.starts_with("~/") || token.starts_with("~\\") {
        return Some(outside_workspace_reason(token));
    }

    // Containment is tested only for path-like tokens: a bare word resolves
    // under the cwd anyway, and prompting on every ambiguous word would make
    // manual mode unusable.
    if !is_path_like_token(token) {
        return None;
    }
    let base = base?;
    let resolved = if token_path.is_absolute() {
        lexically_normalize(token_path)
    } else {
        lexically_normalize(&base.join(token_path))
    };
    if roots.iter().any(|root| path_contains(root, &resolved)) {
        return None;
    }
    Some(outside_workspace_reason(token))
}

fn outside_workspace_reason(token: &str) -> String {
    format!(
        "`{token}` points outside the project workspace — reading it requires approval in manual mode"
    )
}

/// Whether a token plausibly names a filesystem path (vs a bare word, pattern,
/// or switch): it contains a separator, climbs with `..`, or starts with a
/// Windows drive prefix (`C:`).
fn is_path_like_token(token: &str) -> bool {
    if token.contains(['/', '\\']) {
        return true;
    }
    if token == ".." {
        return true;
    }
    let bytes = token.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
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

fn git_requires_manual_approval(command: &ToolCommand) -> bool {
    let Some(subcommand) = command.args.first().map(|arg| normalized(arg)) else {
        return true;
    };
    let args = command
        .args
        .iter()
        .skip(1)
        .map(|arg| arg.to_ascii_lowercase())
        .collect::<Vec<_>>();
    match subcommand.as_str() {
        "clean" | "restore" | "rebase" | "merge" => true,
        "reset" => args.iter().any(|arg| arg == "--hard"),
        "checkout" | "switch" => args
            .iter()
            .any(|arg| arg == "-f" || arg == "--force" || arg == "--"),
        "push" => args.iter().any(|arg| {
            arg == "-f" || arg == "--force" || arg == "--force-with-lease" || arg == "--mirror"
        }),
        "branch" => args.iter().any(|arg| arg == "-d" || arg == "-D"),
        _ => false,
    }
}

fn shell_script_requires_manual_approval(command: &ToolCommand) -> bool {
    let program = normalized(&command.program);
    if is_powershell_shell(&program)
        && command
            .args
            .iter()
            .any(|arg| is_powershell_encoded_flag(&arg.to_ascii_lowercase()))
    {
        return true;
    }

    let script = shell_script_text(command);
    let tokens = shell_tokens(&script);
    if tokens.is_empty() {
        return false;
    }

    if tokens.iter().any(|token| is_dangerous_shell_program(token)) {
        return true;
    }

    if shell_invokes_encoded_powershell(&tokens) {
        return true;
    }

    for (index, token) in tokens.iter().enumerate() {
        if is_shell_delete_command(token) {
            return true;
        }
        if (token == "reg" || token == "reg.exe")
            && tokens.get(index + 1).is_some_and(|subcommand| {
                matches!(subcommand.as_str(), "add" | "delete" | "import")
            })
        {
            return true;
        }
    }

    false
}

fn shell_script_text(command: &ToolCommand) -> String {
    let program = normalized(&command.program);
    let command_flags = match program.as_str() {
        "cmd" | "cmd.exe" => &["/c", "/k"][..],
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => &["-command", "-c", "/c"][..],
        "bash" | "sh" | "zsh" => &["-c"][..],
        _ => &[][..],
    };

    for (index, arg) in command.args.iter().enumerate() {
        if command_flags
            .iter()
            .any(|flag| arg.eq_ignore_ascii_case(flag))
        {
            return command.args[index + 1..].join(" ");
        }
    }
    command.args.join(" ")
}

fn shell_tokens(script: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for ch in script.chars() {
        match quote {
            Some(active) if ch == active => {
                quote = None;
            }
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
            }
            None if ch.is_whitespace() || matches!(ch, '&' | '|' | ';' | '(' | ')') => {
                push_shell_token(&mut tokens, &mut current);
            }
            None => current.push(ch),
        }
    }

    push_shell_token(&mut tokens, &mut current);
    tokens
}

fn push_shell_token(tokens: &mut Vec<String>, current: &mut String) {
    let token = current
        .trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | ',' | '.'))
        .to_ascii_lowercase();
    if !token.is_empty() {
        tokens.push(token);
    }
    current.clear();
}

fn shell_invokes_encoded_powershell(tokens: &[String]) -> bool {
    let has_powershell = tokens.iter().any(|token| is_powershell_shell(token));
    has_powershell && tokens.iter().any(|token| is_powershell_encoded_flag(token))
}

fn is_powershell_shell(program: &str) -> bool {
    matches!(
        program,
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    )
}

fn is_powershell_encoded_flag(token: &str) -> bool {
    matches!(
        token,
        "-encodedcommand" | "/encodedcommand" | "-enc" | "/enc" | "-e" | "/e"
    )
}

fn is_dangerous_shell_program(token: &str) -> bool {
    matches!(
        token,
        "diskpart"
            | "diskpart.exe"
            | "format"
            | "format.com"
            | "shutdown"
            | "shutdown.exe"
            | "reboot"
            | "bcdedit"
            | "bcdedit.exe"
            | "cipher"
            | "cipher.exe"
            | "set-executionpolicy"
            | "takeown"
            | "takeown.exe"
    )
}

fn is_shell_delete_command(token: &str) -> bool {
    matches!(
        token,
        "rm" | "rm.exe"
            | "del"
            | "del.exe"
            | "erase"
            | "erase.exe"
            | "rd"
            | "rd.exe"
            | "rmdir"
            | "rmdir.exe"
            | "remove-item"
            | "ri"
    )
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
        "pwd" | "ls" | "dir" | "cat" | "type" | "findstr" | "where" | "whoami"
    )
}

fn is_script_interpreter(program: &str) -> bool {
    matches!(
        program,
        "node" | "node.exe" | "python" | "python.exe" | "python3" | "python3.exe"
    )
}

fn is_safe_interpreter_probe(program: &str, args: &[String]) -> bool {
    let [flag] = args else {
        return false;
    };
    match program {
        "node" | "node.exe" => matches!(flag.as_str(), "--version" | "-v"),
        "python" | "python.exe" | "python3" | "python3.exe" => {
            matches!(flag.as_str(), "--version" | "-V" | "-VV")
        }
        _ => false,
    }
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
    use crate::tools::{ToolCommand, ToolExecutionRequest, ToolKind, ToolOutputPolicy};

    #[test]
    fn user_denylist_blocks_command_even_in_yolo() {
        let policy = UserAwareCommandPermissionPolicy::new(
            ToolPolicyStore::new(ToolPolicySettings {
                command_deny: vec!["rm".to_string()],
                ..Default::default()
            }),
            ToolApprovalModeStore::new(ToolApprovalMode::Yolo),
        );
        let request = command_request("rm", &["-rf", "src"]);
        assert_eq!(policy.evaluate(&request).action, ToolPermissionAction::Deny);
    }

    #[test]
    fn user_allowlist_auto_approves_command_that_would_otherwise_ask() {
        let policy = UserAwareCommandPermissionPolicy::new(
            ToolPolicyStore::new(ToolPolicySettings {
                command_allow: vec!["npm".to_string()],
                ..Default::default()
            }),
            ToolApprovalModeStore::new(ToolApprovalMode::Manual),
        );
        // `npm install` is otherwise "ask" under manual mode.
        let request = command_request("npm", &["install"]);
        assert_eq!(
            policy.evaluate(&request).action,
            ToolPermissionAction::Allow
        );
    }

    #[test]
    fn tool_policy_sanitizes_and_matches_extension_insensitively() {
        let store = ToolPolicyStore::new(ToolPolicySettings {
            command_allow: vec!["  Git.EXE ".to_string(), "git".to_string()],
            disabled_tools: vec![
                "read_file".to_string(),
                "defunct_tool".to_string(),
                "bogus_tool".to_string(),
            ],
            ..Default::default()
        });
        let settings = store.settings();
        // Trimmed, lowercased, extension-stripped, de-duplicated.
        assert_eq!(settings.command_allow, vec!["git".to_string()]);
        // Unknown and non-routable tool names are dropped.
        assert_eq!(settings.disabled_tools, vec!["read_file".to_string()]);
        assert!(store.is_tool_disabled(ToolKind::ReadFile));
        assert!(!store.is_tool_disabled(ToolKind::WriteFile));
        assert_eq!(
            store.command_decision("GIT.exe"),
            Some(ToolPermissionAction::Allow)
        );
    }

    #[test]
    fn user_deny_wins_over_allow() {
        let store = ToolPolicyStore::new(ToolPolicySettings {
            command_allow: vec!["git".to_string()],
            command_deny: vec!["git".to_string()],
            ..Default::default()
        });
        assert_eq!(
            store.command_decision("git"),
            Some(ToolPermissionAction::Deny)
        );
    }

    #[test]
    fn concurrent_chats_each_enforce_their_own_mode() {
        // The privilege-escalation regression this store exists to prevent: a
        // run starting in a yolo chat must not loosen the gating of a manual
        // chat running concurrently.
        let mode = ToolApprovalModeStore::new(ToolApprovalMode::Manual);
        let policy = UserAwareCommandPermissionPolicy::new(
            ToolPolicyStore::new(ToolPolicySettings::default()),
            Arc::clone(&mode),
        );
        let _yolo_run = mode.begin_run("chat_yolo", ToolApprovalMode::Yolo);
        let _manual_run = mode.begin_run("chat_manual", ToolApprovalMode::Manual);

        // `npm install` is "ask" under manual mode, auto-approved under yolo.
        let yolo_request = chat_command_request("npm", &["install"], Some("chat_yolo"));
        let manual_request = chat_command_request("npm", &["install"], Some("chat_manual"));
        assert_eq!(
            policy.evaluate(&yolo_request).action,
            ToolPermissionAction::Allow
        );
        assert_eq!(
            policy.evaluate(&manual_request).action,
            ToolPermissionAction::Ask
        );
        // No chat context at all (protocol-level run_command) → default mode.
        let bare_request = chat_command_request("npm", &["install"], None);
        assert_eq!(
            policy.evaluate(&bare_request).action,
            ToolPermissionAction::Ask
        );

        // The read-only argument screen is per-chat too: the manual chat's
        // `cat .env` asks while the concurrent yolo chat reads it freely.
        let manual_env = chat_command_request("cat", &[".env"], Some("chat_manual"));
        assert_eq!(
            policy.evaluate(&manual_env).action,
            ToolPermissionAction::Ask,
            "manual chat must be asked before a secret read"
        );
        let yolo_env = chat_command_request("cat", &[".env"], Some("chat_yolo"));
        assert_eq!(
            policy.evaluate(&yolo_env).action,
            ToolPermissionAction::Allow,
            "yolo chat reads sensitive paths without a prompt"
        );
    }

    #[test]
    fn mid_run_mode_change_affects_only_its_chat() {
        let mode = ToolApprovalModeStore::new(ToolApprovalMode::Manual);
        let _run_a = mode.begin_run("chat_a", ToolApprovalMode::Manual);
        let _run_b = mode.begin_run("chat_b", ToolApprovalMode::Manual);

        assert!(mode.update_chat_mode("chat_a", ToolApprovalMode::Yolo));
        assert_eq!(
            mode.mode_for_chat(Some("chat_a")),
            ToolApprovalMode::Yolo,
            "the changed chat follows its new mode mid-run"
        );
        assert_eq!(
            mode.mode_for_chat(Some("chat_b")),
            ToolApprovalMode::Manual,
            "a sibling chat's in-flight run must keep its own mode"
        );

        // A chat with no run in flight is not registered by an update — its
        // next run seeds from the persisted chat state instead.
        assert!(!mode.update_chat_mode("chat_idle", ToolApprovalMode::Yolo));
        assert_eq!(
            mode.mode_for_chat(Some("chat_idle")),
            ToolApprovalMode::Manual
        );
    }

    #[test]
    fn run_guard_drop_releases_chat_entry() {
        let mode = ToolApprovalModeStore::new(ToolApprovalMode::Manual);
        let guard = mode.begin_run("chat_a", ToolApprovalMode::Yolo);
        assert_eq!(mode.mode_for_chat(Some("chat_a")), ToolApprovalMode::Yolo);

        drop(guard);
        assert_eq!(
            mode.mode_for_chat(Some("chat_a")),
            ToolApprovalMode::Manual,
            "a terminal run must release its chat's entry"
        );
        assert!(
            !mode.update_chat_mode("chat_a", ToolApprovalMode::Yolo),
            "the entry must actually be removed, not just reset"
        );
    }

    #[test]
    fn overlapping_runs_in_one_chat_keep_entry_until_last_ends() {
        let mode = ToolApprovalModeStore::new(ToolApprovalMode::Manual);
        let first = mode.begin_run("chat_a", ToolApprovalMode::AutoSafe);
        let second = mode.begin_run("chat_a", ToolApprovalMode::AutoSafe);

        drop(first);
        assert_eq!(
            mode.mode_for_chat(Some("chat_a")),
            ToolApprovalMode::AutoSafe,
            "the entry must survive while another run of the chat is active"
        );
        drop(second);
        assert_eq!(mode.mode_for_chat(Some("chat_a")), ToolApprovalMode::Manual);
    }

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

    #[tokio::test]
    async fn pending_gate_can_resolve_all_waiters() {
        let gate = PendingToolApprovalGate::new();
        let request = request("tool_approval_all");
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
        assert_eq!(gate.decide_all(ToolApprovalDecision::Approved), 1);
        assert_eq!(waiter.await.unwrap(), ToolApprovalDecision::Approved);
    }

    fn request(tool_call_id: &str) -> ToolExecutionRequest {
        ToolExecutionRequest {
            tool_call_id: tool_call_id.to_string(),
            run_id: None,
            chat_id: None,
            project_id: Some("project_1".to_string()),
            workspace_root: None,
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

    fn command_request(program: &str, args: &[&str]) -> ToolExecutionRequest {
        chat_command_request(program, args, None)
    }

    fn chat_command_request(
        program: &str,
        args: &[&str],
        chat_id: Option<&str>,
    ) -> ToolExecutionRequest {
        ToolExecutionRequest {
            tool_call_id: "tool_command".to_string(),
            run_id: None,
            chat_id: chat_id.map(str::to_string),
            project_id: Some("project_1".to_string()),
            workspace_root: None,
            cwd: None,
            command: ToolCommand {
                program: program.to_string(),
                args: args.iter().map(|arg| arg.to_string()).collect(),
                env: BTreeMap::new(),
            },
            timeout_ms: None,
            output_policy: ToolOutputPolicy::default(),
        }
    }

    /// A command request with a workspace root + cwd, as the chat dispatcher
    /// builds them, for the read-only argument screen tests. The paths are used
    /// purely lexically, so they need not exist.
    fn screen_request(
        program: &str,
        args: &[&str],
        workspace_root: &std::path::Path,
        cwd: &std::path::Path,
    ) -> ToolExecutionRequest {
        let mut request = command_request(program, args);
        request.workspace_root = Some(workspace_root.to_path_buf());
        request.cwd = Some(cwd.to_path_buf());
        request
    }

    #[test]
    fn auto_safe_allows_ordinary_commands_without_prompt() {
        let request = command_request("npm", &["test"]);
        let evaluation = command_permission_for_mode(ToolApprovalMode::AutoSafe, &request);
        assert_eq!(evaluation.action, ToolPermissionAction::Allow);
    }

    #[test]
    fn auto_safe_keeps_destructive_commands_on_approval_path() {
        let request = command_request("powershell.exe", &["-Command", "Remove-Item -Recurse src"]);
        let evaluation = command_permission_for_mode(ToolApprovalMode::AutoSafe, &request);
        assert_eq!(evaluation.action, ToolPermissionAction::Ask);

        let request = command_request("git", &["reset", "--hard", "HEAD"]);
        let evaluation = command_permission_for_mode(ToolApprovalMode::AutoSafe, &request);
        assert_eq!(evaluation.action, ToolPermissionAction::Ask);
    }

    #[test]
    fn auto_safe_keeps_shell_bypass_forms_on_approval_path() {
        let cases = [
            (
                "powershell.exe",
                &["-EncodedCommand", "UgBlAG0AbwB2AGUALQBJAHQAZQBtAA=="][..],
            ),
            ("cmd.exe", &["/c", "rd /q /s target"][..]),
            ("cmd.exe", &["/c", "del /q /s *.tmp"][..]),
            ("bash", &["-c", "rm -r -f target"][..]),
            ("cmd.exe", &["/c", "powershell -EncodedCommand AAAA"][..]),
        ];

        for (program, args) in cases {
            let request = command_request(program, args);
            let evaluation = command_permission_for_mode(ToolApprovalMode::AutoSafe, &request);
            assert_eq!(
                evaluation.action,
                ToolPermissionAction::Ask,
                "{program} {args:?}"
            );
        }
    }

    #[test]
    fn script_interpreters_require_approval_except_version_probes() {
        for (program, args) in [
            ("python", &["-c", "open('x', 'w').write('y')"][..]),
            ("node", &["-e", "require('fs').writeFileSync('x', 'y')"][..]),
        ] {
            let request = command_request(program, args);
            let evaluation = command_permission_for_mode(ToolApprovalMode::AutoSafe, &request);
            assert_eq!(
                evaluation.action,
                ToolPermissionAction::Ask,
                "{program} {args:?}"
            );
        }

        for (program, args) in [("python", &["--version"][..]), ("node", &["-v"][..])] {
            let request = command_request(program, args);
            let evaluation = command_permission_for_mode(ToolApprovalMode::AutoSafe, &request);
            assert_eq!(
                evaluation.action,
                ToolPermissionAction::Allow,
                "{program} {args:?}"
            );
        }
    }

    #[test]
    fn auto_safe_preserves_hard_denies() {
        let request = command_request("diskpart", &[]);
        let evaluation = command_permission_for_mode(ToolApprovalMode::AutoSafe, &request);
        assert_eq!(evaluation.action, ToolPermissionAction::Deny);
    }

    #[test]
    fn yolo_allows_commands_that_manual_policy_would_block() {
        let request = command_request("diskpart", &[]);
        let evaluation = command_permission_for_mode(ToolApprovalMode::Yolo, &request);
        assert_eq!(evaluation.action, ToolPermissionAction::Allow);
    }

    #[test]
    fn approval_mode_rewrites_file_asks_but_not_denies() {
        assert_eq!(
            file_permission_action_for_mode(ToolApprovalMode::AutoSafe, ToolPermissionAction::Ask),
            ToolPermissionAction::Allow
        );
        assert_eq!(
            file_permission_action_for_mode(ToolApprovalMode::Yolo, ToolPermissionAction::Deny),
            ToolPermissionAction::Deny
        );
    }

    // ---- read-only argument screen (manual mode) ---------------------------

    #[test]
    fn manual_screens_read_only_commands_for_sensitive_arguments() {
        let root = std::env::temp_dir().join("mothership_screen_ws");
        let action = |program: &str, args: &[&str]| {
            command_permission_for_mode(
                ToolApprovalMode::Manual,
                &screen_request(program, args, &root, &root),
            )
            .action
        };

        // Secret reads must ask, whether named bare, relative, or absolute.
        assert_eq!(action("cat", &[".env"]), ToolPermissionAction::Ask);
        assert_eq!(
            action("type", &["config\\.ssh\\id_rsa"]),
            ToolPermissionAction::Ask
        );
        assert_eq!(
            action("cat", &["C:\\Users\\me\\.ssh\\id_rsa"]),
            ToolPermissionAction::Ask
        );
        // The reason must say WHY so the approval card is meaningful.
        let evaluation = command_permission_for_mode(
            ToolApprovalMode::Manual,
            &screen_request("cat", &[".env"], &root, &root),
        );
        assert!(
            evaluation.reason.contains("sensitive"),
            "got: {}",
            evaluation.reason
        );

        // Ordinary workspace reads stay auto-allowed — no prompt harassment.
        assert_eq!(action("cat", &["src/main.rs"]), ToolPermissionAction::Allow);
        assert_eq!(action("cat", &["README.md"]), ToolPermissionAction::Allow);
        assert_eq!(action("ls", &["-la"]), ToolPermissionAction::Allow);
        // Flags and Windows switches are not treated as paths.
        assert_eq!(
            action("findstr", &["/i", "needle", "src\\app.rs"]),
            ToolPermissionAction::Allow
        );
    }

    #[test]
    fn manual_screens_read_only_commands_for_workspace_escapes() {
        let root = std::env::temp_dir().join("mothership_screen_ws");
        let action = |program: &str, args: &[&str]| {
            command_permission_for_mode(
                ToolApprovalMode::Manual,
                &screen_request(program, args, &root, &root),
            )
            .action
        };

        // Relative climb-outs and absolute outside paths must ask.
        assert_eq!(
            action("type", &["..\\..\\outside.txt"]),
            ToolPermissionAction::Ask
        );
        assert_eq!(
            action("cat", &["../secrets-elsewhere"]),
            ToolPermissionAction::Ask
        );
        let outside = std::env::temp_dir()
            .join("mothership_screen_outside")
            .join("notes.txt");
        assert_eq!(
            action("cat", &[outside.to_string_lossy().as_ref()]),
            ToolPermissionAction::Ask
        );
        // Home-anchored references are outside the workspace by definition.
        assert_eq!(action("cat", &["~/todo.txt"]), ToolPermissionAction::Ask);

        // An absolute path INSIDE the workspace stays auto-allowed.
        let inside = root.join("docs").join("guide.md");
        assert_eq!(
            action("cat", &[inside.to_string_lossy().as_ref()]),
            ToolPermissionAction::Allow
        );
    }

    #[test]
    fn auto_safe_and_yolo_skip_the_read_only_argument_screen() {
        // In auto_safe/yolo sensitive reads are an explicitly allowed
        // capability: no screening, the read-only auto-allow stands.
        let request = command_request("cat", &[".env"]);
        assert_eq!(
            command_permission_for_mode(ToolApprovalMode::AutoSafe, &request).action,
            ToolPermissionAction::Allow
        );
        assert_eq!(
            command_permission_for_mode(ToolApprovalMode::Yolo, &request).action,
            ToolPermissionAction::Allow
        );
    }

    #[test]
    fn screen_judges_sensitive_names_even_without_path_context() {
        // A protocol-level request may carry neither workspace root nor cwd;
        // containment cannot be judged then, but secret names still ask.
        let request = command_request("cat", &[".env"]);
        assert_eq!(
            command_permission_for_mode(ToolApprovalMode::Manual, &request).action,
            ToolPermissionAction::Ask
        );
        // …while a plain workspace-looking read stays allowed (ambiguity
        // prefers auto-allow).
        let request = command_request("cat", &["src/main.rs"]);
        assert_eq!(
            command_permission_for_mode(ToolApprovalMode::Manual, &request).action,
            ToolPermissionAction::Allow
        );
    }
}
