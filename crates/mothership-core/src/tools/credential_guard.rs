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
            stdout: BoundedRedactor::new(Arc::clone(&self.guard)),
            stderr: BoundedRedactor::new(Arc::clone(&self.guard)),
        }))
    }
}

struct RedactingOutputWriter {
    inner: Box<dyn ToolOutputWriter>,
    stdout: BoundedRedactor,
    stderr: BoundedRedactor,
}

#[async_trait]
impl ToolOutputWriter for RedactingOutputWriter {
    async fn append(&mut self, stream: ToolOutputStream, bytes: &[u8]) -> Result<()> {
        let redacted = match stream {
            ToolOutputStream::Stdout => self.stdout.push(bytes),
            ToolOutputStream::Stderr => self.stderr.push(bytes),
        };
        if redacted.is_empty() {
            return Ok(());
        }
        self.inner.append(stream, &redacted).await
    }

    async fn finish(&mut self) -> Result<String> {
        let stdout_rest = self.stdout.flush();
        if !stdout_rest.is_empty() {
            self.inner
                .append(ToolOutputStream::Stdout, &stdout_rest)
                .await?;
        }
        let stderr_rest = self.stderr.flush();
        if !stderr_rest.is_empty() {
            self.inner
                .append(ToolOutputStream::Stderr, &stderr_rest)
                .await?;
        }
        self.inner.finish().await
    }
}

/// Once a line buffers past this without a newline, it is force-flushed in
/// bounded segments — so a no-newline torrent (`print('x'*500_000_000)`) cannot
/// grow memory without bound. Also caps how long a `BEGIN…END` private-key block
/// is buffered before it is force-redacted.
const REDACTOR_BUFFER_LIMIT: usize = 64 * 1024;
/// Raw bytes retained at a forced no-newline flush so a secret straddling the cut
/// is re-examined with the next chunk. Larger than any single-line secret pattern.
const REDACTOR_OVERLAP: usize = 1024;
const PEM_MARKER: &[u8] = b"[REDACTED:private-key]\n";

/// A bounded, stateful streaming redactor for one output stream. It guarantees
/// two things the naive line-buffer did not:
///
/// 1. **Bounded memory** — it never retains more than ~`REDACTOR_BUFFER_LIMIT`
///    bytes, force-flushing a long no-newline run in segments (keeping a small
///    overlap so a secret split across the cut is still caught).
/// 2. **Multi-line secrets** — a `BEGIN…END PRIVATE KEY` block is held and
///    redacted as a *unit* (emitting a single marker), since the PEM pattern only
///    matches across lines. A runaway block (> the buffer limit without an `END`)
///    is force-redacted to a marker rather than buffered or leaked.
///
/// UTF-8 segments are scrubbed through the guard; non-UTF-8 (binary) segments pass
/// through unchanged to preserve fidelity.
struct BoundedRedactor {
    guard: Arc<dyn CredentialGuard>,
    pending: Vec<u8>,
    in_pem: bool,
}

impl BoundedRedactor {
    fn new(guard: Arc<dyn CredentialGuard>) -> Self {
        Self {
            guard,
            pending: Vec::new(),
            in_pem: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.in_pem {
                if let Some(end) = find_pem_end(&self.pending) {
                    // Complete BEGIN…END block: drop the key bytes, emit one marker.
                    self.pending.drain(..end);
                    out.extend_from_slice(PEM_MARKER);
                    self.in_pem = false;
                    continue;
                }
                if self.pending.len() > REDACTOR_BUFFER_LIMIT {
                    // Runaway key with no END in sight: emit a marker, drop the
                    // buffered key bytes (keep a small tail to catch the END), and
                    // stay in_pem. No key material is written.
                    let drop_to = self.pending.len() - REDACTOR_OVERLAP;
                    self.pending.drain(..drop_to);
                    out.extend_from_slice(PEM_MARKER);
                }
                break;
            }

            if let Some(nl) = self.pending.iter().position(|&byte| byte == b'\n') {
                if line_starts_pem(&self.pending[..=nl]) {
                    // Keep the BEGIN line in the buffer as the block start.
                    self.in_pem = true;
                    continue;
                }
                let line: Vec<u8> = self.pending.drain(..=nl).collect();
                out.extend_from_slice(&redact_segment(self.guard.as_ref(), &line));
                continue;
            }

            if self.pending.len() > REDACTOR_BUFFER_LIMIT {
                // Long no-newline run: flush a bounded prefix, cutting on a
                // non-token boundary so a single-line secret is not split.
                let cut = safe_cut(&self.pending, REDACTOR_OVERLAP);
                let segment: Vec<u8> = self.pending.drain(..cut).collect();
                out.extend_from_slice(&redact_segment(self.guard.as_ref(), &segment));
                continue;
            }

            break;
        }
        out
    }

    fn flush(&mut self) -> Vec<u8> {
        if self.pending.is_empty() {
            self.in_pem = false;
            return Vec::new();
        }
        let rest = std::mem::take(&mut self.pending);
        let out = if self.in_pem {
            // Trailing, unterminated key material — never write it raw.
            PEM_MARKER.to_vec()
        } else {
            redact_segment(self.guard.as_ref(), &rest)
        };
        self.in_pem = false;
        out
    }
}

/// Redact one segment: UTF-8 → scrubbed through the guard; non-UTF-8 → raw.
fn redact_segment(guard: &dyn CredentialGuard, bytes: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(bytes) {
        Ok(text) => guard.redact(text).into_owned().into_bytes(),
        Err(_) => bytes.to_vec(),
    }
}

/// True if a (CR-tolerant) line opens a PEM private-key block.
fn line_starts_pem(line: &[u8]) -> bool {
    std::str::from_utf8(line)
        .map(|text| text.contains("BEGIN") && text.contains("PRIVATE KEY"))
        .unwrap_or(false)
}

/// If `buf` contains the closing line of a PEM private-key block, return the byte
/// index just past that line (so `buf[..idx]` is the whole `BEGIN…END` block).
fn find_pem_end(buf: &[u8]) -> Option<usize> {
    let mut offset = 0;
    for line in buf.split_inclusive(|&byte| byte == b'\n') {
        offset += line.len();
        if let Ok(text) = std::str::from_utf8(line) {
            if text.contains("END") && text.contains("PRIVATE KEY") {
                return Some(offset);
            }
        }
    }
    None
}

/// Choose a flush cut for a long no-newline run: target `len - overlap`, then back
/// up (at most `overlap` more) to a non-token, char-boundary byte so a single-line
/// secret isn't split across the cut. Always returns a positive, bounded cut.
fn safe_cut(buf: &[u8], overlap: usize) -> usize {
    let target = buf.len().saturating_sub(overlap).max(1);
    let floor = target.saturating_sub(overlap).max(1);
    let is_token = |byte: u8| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'/' | b'+' | b'=' | b':')
    };
    let mut cut = target;
    while cut > floor {
        let byte = buf[cut - 1];
        if is_token(byte) || (byte & 0xC0) == 0x80 {
            cut -= 1;
        } else {
            break;
        }
    }
    // Don't cut in the middle of a UTF-8 character.
    while cut > floor && (buf[cut] & 0xC0) == 0x80 {
        cut -= 1;
    }
    cut.max(floor)
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

    #[test]
    fn bounded_redactor_stays_bounded_on_no_newline_torrent() {
        // A no-newline torrent (e.g. `print('x'*500_000_000, end='')`) must not
        // grow memory without bound: pending stays capped and bytes still flow.
        let mut r = BoundedRedactor::new(Arc::new(PatternCredentialGuard::new()));
        let chunk = vec![b'x'; 8 * 1024];
        let mut total = 0usize;
        for _ in 0..256 {
            total += r.push(&chunk).len();
            assert!(
                r.pending.len() <= REDACTOR_BUFFER_LIMIT + chunk.len(),
                "pending must stay bounded, got {}",
                r.pending.len()
            );
        }
        total += r.flush().len();
        assert_eq!(total, 256 * 8 * 1024, "benign bytes pass through without loss");
    }

    #[test]
    fn bounded_redactor_redacts_secret_in_long_run() {
        let mut r = BoundedRedactor::new(Arc::new(PatternCredentialGuard::new()));
        let chunk = vec![b'x'; 8 * 1024];
        let mut out = Vec::new();
        for _ in 0..12 {
            out.extend(r.push(&chunk)); // ~96 KiB with no newline, force-flushed
        }
        out.extend(r.push(b" AKIAIOSFODNN7EXAMPLE \n"));
        out.extend(r.flush());
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("[REDACTED:aws-key]"),
            "a secret after a long no-newline run must still be redacted"
        );
    }

    #[test]
    fn bounded_redactor_redacts_multiline_pem_block() {
        // The PEM pattern only matches across lines; the durable path must hold
        // the whole BEGIN…END block and redact it as a unit, not line-by-line.
        let mut r = BoundedRedactor::new(Arc::new(PatternCredentialGuard::new()));
        let mut out =
            r.push(b"before\n-----BEGIN RSA PRIVATE KEY-----\nMIIsecretAAAA\nBBBBsecret\n");
        out.extend(r.push(b"-----END RSA PRIVATE KEY-----\nafter\n"));
        out.extend(r.flush());
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("[REDACTED:private-key]"),
            "multi-line PEM must be redacted in the durable blob: {text}"
        );
        assert!(
            !text.contains("MIIsecret"),
            "key material must not reach disk: {text}"
        );
        assert!(
            text.contains("before") && text.contains("after"),
            "content surrounding the key is kept"
        );
    }
}
