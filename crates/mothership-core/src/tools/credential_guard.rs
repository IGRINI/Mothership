//! Credential firewall foundation.
//!
//! Mothership's standing invariant is that secrets must not leak to the model or
//! into persisted/displayed history. Sensitive *paths* (`.env`, `.ssh`, key
//! files, …) are already skipped/denied by the workspace policy. This module adds
//! the second layer: detection + redaction of obvious secrets that appear in tool
//! *output* — command stdout/stderr, diff previews, artifact previews, event
//! messages — before they are persisted or shown.
//!
//! This is the foundation, applied at the event sink (the single chokepoint for
//! everything stored and pushed to the UI). It deliberately does NOT yet redact
//! the model-facing tool *result* text, because the model frequently needs to see
//! real file contents to do its job (and known-secret files are already blocked
//! by path policy). The [`CredentialGuard`] trait is the seam where a stricter,
//! policy-driven model-visible redaction (and a full managed-gateway / proxy) can
//! later attach.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;

use crate::Result;

use super::output::{ToolOutputStore, ToolOutputWriter};
use super::types::{ToolArtifact, ToolExecutionEvent, ToolOutputStream};

/// Redacts obvious secrets from text destined for storage or display.
pub trait CredentialGuard: Send + Sync {
    /// Return `input` with any detected secrets replaced by a redaction marker.
    /// Borrows unchanged input when nothing matched (cheap common case).
    fn redact<'a>(&self, input: &'a str) -> Cow<'a, str>;
}

/// A guard that never redacts — for contexts where the firewall is intentionally
/// disabled (tests, trusted internal text).
#[derive(Debug, Default)]
pub struct NoopCredentialGuard;

impl CredentialGuard for NoopCredentialGuard {
    fn redact<'a>(&self, input: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(input)
    }
}

/// Pattern-based guard covering common, high-confidence secret shapes: PEM
/// private-key blocks, cloud/provider API keys and tokens, bearer headers, and
/// `key = value` style credential assignments. Conservative by design — it keeps
/// surrounding context (e.g. the variable name) and only blanks the secret value,
/// to minimize false positives on legitimate content.
pub struct PatternCredentialGuard {
    rules: Vec<(Regex, &'static str)>,
}

impl PatternCredentialGuard {
    pub fn new() -> Self {
        // (pattern, replacement). `$N` refers to capture groups so we keep context.
        let specs: &[(&str, &str)] = &[
            // PEM private-key blocks (multi-line).
            (
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
                "[REDACTED:private-key]",
            ),
            // AWS access key id.
            (r"\bAKIA[0-9A-Z]{16}\b", "[REDACTED:aws-key]"),
            // GitHub tokens (ghp_/gho_/ghu_/ghs_/ghr_).
            (r"\bgh[pousr]_[A-Za-z0-9]{36,}\b", "[REDACTED:github-token]"),
            // Slack tokens.
            (r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b", "[REDACTED:slack-token]"),
            // OpenAI / Anthropic style keys.
            (r"\bsk-(?:ant-)?[A-Za-z0-9_-]{20,}\b", "[REDACTED:api-key]"),
            // Google API key.
            (r"\bAIza[0-9A-Za-z_-]{35}\b", "[REDACTED:google-key]"),
            // Bearer token in an Authorization-style header (keep the scheme).
            (
                r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{20,}",
                "${1}[REDACTED]",
            ),
            // `key = value` / `key: value` credential assignments (keep the key).
            (
                r#"(?i)(password|passwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret)("?\s*[:=]\s*"?)[^\s"']{8,}"#,
                "${1}${2}[REDACTED]",
            ),
        ];
        let rules = specs
            .iter()
            .map(|(pattern, replacement)| {
                (
                    Regex::new(pattern).expect("credential-guard pattern compiles"),
                    *replacement,
                )
            })
            .collect();
        Self { rules }
    }
}

impl Default for PatternCredentialGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialGuard for PatternCredentialGuard {
    fn redact<'a>(&self, input: &'a str) -> Cow<'a, str> {
        let mut current = Cow::Borrowed(input);
        for (regex, replacement) in &self.rules {
            // Only allocate when a rule actually matches.
            if regex.is_match(&current) {
                let replaced = regex.replace_all(&current, *replacement).into_owned();
                current = Cow::Owned(replaced);
            }
        }
        current
    }
}

/// The process-wide default guard. Compiling the pattern set is done once.
pub fn default_credential_guard() -> &'static PatternCredentialGuard {
    static GUARD: OnceLock<PatternCredentialGuard> = OnceLock::new();
    GUARD.get_or_init(PatternCredentialGuard::new)
}

/// Return a redacted clone of a tool event: every model-visible / persisted text
/// field (message, output chunk, result previews/tails, artifact previews, and
/// string values in the typed payload) is run through the guard. Identity,
/// status, counts, paths, and hashes are left untouched.
pub fn redact_event(guard: &dyn CredentialGuard, event: &ToolExecutionEvent) -> ToolExecutionEvent {
    let mut event = event.clone();

    if let Some(message) = event.message.take() {
        event.message = Some(guard.redact(&message).into_owned());
    }
    if let Some(chunk) = event.chunk.take() {
        event.chunk = Some(guard.redact(&chunk).into_owned());
    }
    // Command line: args and env VALUES are model-authored text that may carry a
    // secret (e.g. `curl -H "Authorization: Bearer …"`). Redact them so the
    // command is not stored/shown verbatim. Program and env keys are left intact.
    // This also covers the synthesized run_command payload, which is built from
    // `command` at the storage layer after this redaction.
    if let Some(command) = event.command.as_mut() {
        for arg in command.args.iter_mut() {
            redact_in_place(guard, arg);
        }
        for value in command.env.values_mut() {
            redact_in_place(guard, value);
        }
    }
    if let Some(result) = event.result.as_mut() {
        redact_in_place(guard, &mut result.stdout_preview);
        redact_in_place(guard, &mut result.stderr_preview);
        redact_in_place(guard, &mut result.stdout_tail);
        redact_in_place(guard, &mut result.stderr_tail);
        if let Some(message) = result.message.take() {
            result.message = Some(guard.redact(&message).into_owned());
        }
    }
    for artifact in event.artifacts.iter_mut() {
        redact_artifact(guard, artifact);
    }
    if let Some(payload) = event.payload.as_mut() {
        redact_json_strings(guard, payload);
    }

    event
}

fn redact_in_place(guard: &dyn CredentialGuard, text: &mut String) {
    if let Cow::Owned(redacted) = guard.redact(text) {
        *text = redacted;
    }
}

fn redact_artifact(guard: &dyn CredentialGuard, artifact: &mut ToolArtifact) {
    redact_in_place(guard, &mut artifact.preview);
}

/// Walk a JSON payload, redacting secrets inside string values in place (object
/// keys and non-string scalars are left alone).
fn redact_json_strings(guard: &dyn CredentialGuard, value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Cow::Owned(redacted) = guard.redact(text) {
                *text = redacted;
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json_strings(guard, item);
            }
        }
        Value::Object(map) => {
            for (_key, val) in map.iter_mut() {
                redact_json_strings(guard, val);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Redacting durable output store
// ---------------------------------------------------------------------------

/// Wraps a [`ToolOutputStore`] so durable spilled blobs (command stdout/stderr,
/// file-tool diffs/reads/search results) are credential-redacted on the way to
/// disk — closing the gap where `redact_event` only scrubbed the inline preview,
/// not the full content behind a `log_ref`. Redaction is line-buffered: complete
/// lines are scrubbed before they are written; a partial line is held until the
/// next append (or `finish`). UTF-8 lines are redacted; non-UTF-8 (binary) lines
/// pass through unchanged so output fidelity is preserved.
pub struct RedactingOutputStore {
    inner: Arc<dyn ToolOutputStore>,
    guard: Arc<dyn CredentialGuard>,
}

impl RedactingOutputStore {
    pub fn new(inner: Arc<dyn ToolOutputStore>, guard: Arc<dyn CredentialGuard>) -> Self {
        Self { inner, guard }
    }
}

#[async_trait]
impl ToolOutputStore for RedactingOutputStore {
    async fn open(&self, tool_call_id: &str) -> Result<Box<dyn ToolOutputWriter>> {
        let inner = self.inner.open(tool_call_id).await?;
        Ok(Box::new(RedactingOutputWriter {
            inner,
            guard: Arc::clone(&self.guard),
            stdout_buf: Vec::new(),
            stderr_buf: Vec::new(),
        }))
    }
}

struct RedactingOutputWriter {
    inner: Box<dyn ToolOutputWriter>,
    guard: Arc<dyn CredentialGuard>,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
}

#[async_trait]
impl ToolOutputWriter for RedactingOutputWriter {
    async fn append(&mut self, stream: ToolOutputStream, bytes: &[u8]) -> Result<()> {
        // Buffer, then flush only the complete-line prefix (through the last
        // newline) so a secret can never be split across a redaction boundary.
        let complete = {
            let buf = match stream {
                ToolOutputStream::Stdout => &mut self.stdout_buf,
                ToolOutputStream::Stderr => &mut self.stderr_buf,
            };
            buf.extend_from_slice(bytes);
            match buf.iter().rposition(|&byte| byte == b'\n') {
                Some(pos) => buf.drain(..=pos).collect::<Vec<u8>>(),
                None => return Ok(()),
            }
        };
        let redacted = redact_bytes_linewise(self.guard.as_ref(), &complete);
        self.inner.append(stream, &redacted).await
    }

    async fn finish(&mut self) -> Result<String> {
        let stdout_rest = std::mem::take(&mut self.stdout_buf);
        if !stdout_rest.is_empty() {
            let redacted = redact_bytes_linewise(self.guard.as_ref(), &stdout_rest);
            self.inner
                .append(ToolOutputStream::Stdout, &redacted)
                .await?;
        }
        let stderr_rest = std::mem::take(&mut self.stderr_buf);
        if !stderr_rest.is_empty() {
            let redacted = redact_bytes_linewise(self.guard.as_ref(), &stderr_rest);
            self.inner
                .append(ToolOutputStream::Stderr, &redacted)
                .await?;
        }
        self.inner.finish().await
    }
}

/// Redact a byte buffer line by line: UTF-8 lines are scrubbed through the guard;
/// non-UTF-8 (binary) lines pass through unchanged to preserve fidelity.
fn redact_bytes_linewise(guard: &dyn CredentialGuard, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|&byte| byte == b'\n') {
        match std::str::from_utf8(line) {
            Ok(text) => out.extend_from_slice(guard.redact(text).as_bytes()),
            Err(_) => out.extend_from_slice(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolExecutionEventKind;
    use std::sync::Mutex;

    fn guard() -> PatternCredentialGuard {
        PatternCredentialGuard::new()
    }

    #[test]
    fn redacts_common_secret_shapes() {
        let g = guard();
        assert!(g
            .redact("AWS key AKIAIOSFODNN7EXAMPLE here")
            .contains("[REDACTED:aws-key]"));
        assert!(g
            .redact("token=ghp_0123456789abcdef0123456789abcdef0123")
            .contains("[REDACTED:github-token]"));
        assert!(g
            .redact("OPENAI=sk-abcdEFGH1234ijklMNOP5678")
            .contains("[REDACTED:api-key]"));
        assert!(g
            .redact("Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123")
            .contains("[REDACTED]"));
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIBjunk\n-----END RSA PRIVATE KEY-----";
        assert_eq!(g.redact(pem), "[REDACTED:private-key]");
    }

    #[test]
    fn redacts_assignment_but_keeps_key_name() {
        let g = guard();
        let out = g.redact("password = hunter2hunter2");
        assert!(out.contains("password"), "key name kept: {out}");
        assert!(out.contains("[REDACTED]"), "value redacted: {out}");
        assert!(!out.contains("hunter2hunter2"), "secret gone: {out}");
    }

    #[test]
    fn leaves_benign_text_borrowed_and_unchanged() {
        let g = guard();
        let input = "just a normal log line: built 12 files in 3.4s";
        match g.redact(input) {
            Cow::Borrowed(text) => assert_eq!(text, input),
            Cow::Owned(text) => panic!("benign text should not allocate: {text}"),
        }
    }

    #[test]
    fn redact_event_scrubs_output_and_payload_not_paths() {
        let g = guard();
        let event = ToolExecutionEvent {
            tool_call_id: "tc1".to_string(),
            kind: ToolExecutionEventKind::Output,
            chunk: Some("export TOKEN=ghp_0123456789abcdef0123456789abcdef0123".to_string()),
            payload: Some(serde_json::json!({
                "path": "src/config.rs",
                "stdoutPreview": "AKIAIOSFODNN7EXAMPLE",
            })),
            ..Default::default()
        };
        let redacted = redact_event(&g, &event);
        assert!(redacted.chunk.unwrap().contains("[REDACTED:github-token]"));
        let payload = redacted.payload.unwrap();
        // Path is untouched; the secret-bearing preview is scrubbed.
        assert_eq!(payload["path"], "src/config.rs");
        assert_eq!(payload["stdoutPreview"], "[REDACTED:aws-key]");
    }

    #[test]
    fn redact_event_scrubs_command_args() {
        // A secret in a command line (e.g. an auth header) must not be stored/shown
        // verbatim; the program and env keys are kept.
        let g = guard();
        let event = ToolExecutionEvent {
            tool_call_id: "tc1".to_string(),
            kind: ToolExecutionEventKind::Queued,
            command: Some(crate::ToolCommand::new(
                "curl",
                ["-H", "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123"],
            )),
            ..Default::default()
        };
        let redacted = redact_event(&g, &event);
        let command = redacted.command.expect("command present");
        assert_eq!(command.program, "curl");
        assert!(
            command.args.iter().any(|arg| arg.contains("[REDACTED]")),
            "bearer token in args must be redacted: {:?}",
            command.args
        );
        assert!(
            !command
                .args
                .iter()
                .any(|arg| arg.contains("abcdefghijklmnopqrstuvwxyz0123")),
            "raw token must be gone"
        );
    }

    #[derive(Default)]
    struct RecordingStore {
        appended: Arc<Mutex<Vec<u8>>>,
    }

    #[async_trait]
    impl ToolOutputStore for RecordingStore {
        async fn open(&self, _tool_call_id: &str) -> Result<Box<dyn ToolOutputWriter>> {
            Ok(Box::new(RecordingWriter {
                appended: Arc::clone(&self.appended),
            }))
        }
    }

    struct RecordingWriter {
        appended: Arc<Mutex<Vec<u8>>>,
    }

    #[async_trait]
    impl ToolOutputWriter for RecordingWriter {
        async fn append(&mut self, _stream: ToolOutputStream, bytes: &[u8]) -> Result<()> {
            self.appended.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }
        async fn finish(&mut self) -> Result<String> {
            Ok("recorded".to_string())
        }
    }

    #[tokio::test]
    async fn redacting_output_store_scrubs_durable_lines() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let inner =
            Arc::new(RecordingStore { appended: Arc::clone(&recorded) }) as Arc<dyn ToolOutputStore>;
        let store = RedactingOutputStore::new(inner, Arc::new(PatternCredentialGuard::new()));
        let mut writer = store.open("tc1").await.unwrap();

        // A secret split across two appends WITHIN one line: it must still be
        // redacted because the writer buffers until the newline.
        writer
            .append(ToolOutputStream::Stdout, b"export TOKEN=ghp_0123456789")
            .await
            .unwrap();
        writer
            .append(
                ToolOutputStream::Stdout,
                b"abcdef0123456789abcdef0123\nplain line\n",
            )
            .await
            .unwrap();
        writer.finish().await.unwrap();

        let durable = String::from_utf8(recorded.lock().unwrap().clone()).unwrap();
        assert!(
            durable.contains("[REDACTED:github-token]"),
            "durable blob must be redacted: {durable}"
        );
        assert!(
            !durable.contains("ghp_0123456789abcdef0123456789abcdef0123"),
            "raw token must not reach disk"
        );
        assert!(durable.contains("plain line"), "benign content kept");
    }
}
