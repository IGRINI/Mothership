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
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use super::types::{ToolArtifact, ToolExecutionEvent};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolExecutionEventKind;

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
}
