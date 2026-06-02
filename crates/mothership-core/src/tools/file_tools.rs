//! Typed handlers for the four first-class file tools: `read_file`,
//! `write_file`, `edit_file`, and `apply_patch`.
//!
//! Each handler takes typed input (deserialized from the tool call's
//! `arguments`), a [`Workspace`] (path policy), a [`FileSystem`] port (byte IO),
//! and — for `read_file` — an optional [`FileToolSpill`] sink for large output.
//! It returns a [`FileToolOutcome`] (the bounded text shown to the model plus
//! structured `data`, a content hash, and a diff where applicable).
//!
//! Separately, [`classify`] inspects an input and reports a
//! [`FileToolCapability`] — the *intent* (read vs. mutate), the paths touched,
//! and a [`ToolPermissionAction`] (`Allow`/`Ask`/`Deny`). The supervisor/runner
//! uses the action to decide whether to auto-run, prompt for approval, or
//! refuse — keeping the tool-declares-intent / policy-decides split from the
//! design doc. Reads inside the workspace are `Allow`; mutations are `Ask`;
//! anything outside the workspace or touching a sensitive path is `Deny`.
//!
//! These handlers never call `std::fs` directly: all byte IO flows through the
//! injected [`FileSystem`], and all path resolution flows through [`Workspace`],
//! so the whole module is unit-testable against a temp directory.

#![allow(dead_code)]

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::file_edit::{apply_edit, EditError};
use super::filesystem::{FileSystem, PathError, Workspace};
use super::patch::{parse_v4a, plan_patch, FileMap, PatchPlan, PlannedFile, PlannedKind};
use super::permissions::ToolPermissionAction;

/// Default cap on how many bytes of file content are inlined into the model
/// result before the read spills to the output store. Mirrors the conservative
/// preview budget used for command output.
const DEFAULT_READ_MAX_BYTES: usize = 256 * 1024;

/// Hard ceiling on how many bytes `read_file` will load off disk in a single
/// call. A file larger than this is read only up to the ceiling (and reported as
/// truncated) so a multi-gigabyte file cannot be slurped into memory; callers
/// that need more can page with `startLine`/`limit`.
const MAX_READ_FILE_BYTES: usize = 10 * 1024 * 1024;

/// Cap on the diff / synthesized result text that a mutating tool puts into the
/// persisted execution event and `ToolExecutionResult` (UI/DB). The model-facing
/// response has its own, larger budget; this only bounds what is stored/streamed
/// so a huge overwrite cannot push megabytes into the event store.
pub const MAX_TOOL_EVENT_BYTES: usize = 64 * 1024;

/// Number of leading bytes sampled for the binary-content heuristic.
const BINARY_SNIFF_BYTES: usize = 4096;

/// The structured result of running a file tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileToolOutcome {
    /// Whether the operation succeeded. A handled, expected failure (file not
    /// found, ambiguous edit, sha conflict, binary file) is reported as
    /// `ok = false` with an explanatory `model_text` — not as an `Err`.
    pub ok: bool,
    /// Bounded text returned to the model.
    pub model_text: String,
    /// Structured machine-readable detail (path, counts, status, etc.).
    pub data: Value,
    /// The resulting content hash, when the tool produced or read file content.
    pub sha256: Option<String>,
    /// A unified-diff-style preview for mutating tools.
    pub diff: Option<String>,
}

impl FileToolOutcome {
    fn failure(model_text: impl Into<String>, data: Value) -> Self {
        Self {
            ok: false,
            model_text: model_text.into(),
            data,
            sha256: None,
            diff: None,
        }
    }
}

/// A tool's declared intent, used by the policy/runner to decide allow/ask/deny.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileToolCapability {
    /// What the runner should do before executing.
    pub action: ToolPermissionAction,
    /// Human-readable summary (shown on an approval card / denial message).
    pub summary: String,
    /// The workspace-relative (or raw, if unresolvable) paths this call touches.
    pub touched_paths: Vec<String>,
}

/// Errors that abort a file tool before it can produce an outcome — bad
/// arguments or an underlying IO failure. Expected, content-level failures
/// (not-found, ambiguous, conflict, binary) are returned as a non-ok
/// [`FileToolOutcome`] instead.
#[derive(Debug)]
pub enum FileToolError {
    /// The `arguments` JSON did not match the tool's input schema.
    InvalidArguments(String),
    /// Underlying filesystem IO failed.
    Io(String),
}

impl std::fmt::Display for FileToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileToolError::InvalidArguments(detail) => {
                write!(f, "invalid arguments: {detail}")
            }
            FileToolError::Io(detail) => write!(f, "filesystem error: {detail}"),
        }
    }
}

impl std::error::Error for FileToolError {}

/// A synchronous sink for spilling oversized read output to durable storage and
/// getting back a reference (path/URI). The sidecar implements this by bridging
/// to the async [`ToolOutputStore`](super::output::ToolOutputStore); tests can
/// pass `None`.
pub trait FileToolSpill {
    /// Persist `content` for `tool_call_id` and return a reference string.
    fn spill(&self, tool_call_id: &str, content: &str) -> std::io::Result<String>;
}

// ===========================================================================
// Inputs
// ===========================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadFileInput {
    pub path: String,
    #[serde(default, alias = "start_line")]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default, alias = "max_bytes")]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteFileInput {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub create: Option<bool>,
    #[serde(default)]
    pub overwrite: Option<bool>,
    #[serde(default, alias = "expected_sha256")]
    pub expected_sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditFileInput {
    pub path: String,
    #[serde(alias = "old_text")]
    pub old_text: String,
    #[serde(alias = "new_text")]
    pub new_text: String,
    #[serde(default, alias = "replace_all")]
    pub replace_all: Option<bool>,
    #[serde(default, alias = "expected_sha256")]
    pub expected_sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPatchInput {
    pub patch: String,
}

/// The file tools, identified by name, used for classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTool {
    Read,
    Write,
    Edit,
    ApplyPatch,
}

impl FileTool {
    /// Map a tool name to a [`FileTool`], or `None` if it is not a file tool.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            super::catalog::READ_FILE_TOOL_NAME => Some(FileTool::Read),
            super::catalog::WRITE_FILE_TOOL_NAME => Some(FileTool::Write),
            super::catalog::EDIT_FILE_TOOL_NAME => Some(FileTool::Edit),
            super::catalog::APPLY_PATCH_TOOL_NAME => Some(FileTool::ApplyPatch),
            _ => None,
        }
    }
}

// ===========================================================================
// Capability classification (intent → allow/ask/deny)
// ===========================================================================

/// Classify a file-tool call into a [`FileToolCapability`]. Resolves every path
/// the call touches against the workspace; an unresolvable or out-of-workspace
/// path, or any sensitive path, yields `Deny`. Reads are otherwise `Allow`;
/// mutations are `Ask`.
pub fn classify(
    tool: FileTool,
    arguments: &Value,
    workspace: &Workspace,
) -> Result<FileToolCapability, FileToolError> {
    match tool {
        FileTool::Read => {
            let input: ReadFileInput = parse_args(arguments)?;
            let touched = vec![input.path.clone()];
            match guard_paths(workspace, &[&input.path]) {
                Ok(()) => Ok(FileToolCapability {
                    action: ToolPermissionAction::Allow,
                    summary: format!("read {}", input.path),
                    touched_paths: touched,
                }),
                Err(reason) => Ok(deny(reason, touched)),
            }
        }
        FileTool::Write => {
            let input: WriteFileInput = parse_args(arguments)?;
            let touched = vec![input.path.clone()];
            match guard_paths(workspace, &[&input.path]) {
                Ok(()) => Ok(FileToolCapability {
                    action: ToolPermissionAction::Ask,
                    summary: format!("write {}", input.path),
                    touched_paths: touched,
                }),
                Err(reason) => Ok(deny(reason, touched)),
            }
        }
        FileTool::Edit => {
            let input: EditFileInput = parse_args(arguments)?;
            let touched = vec![input.path.clone()];
            match guard_paths(workspace, &[&input.path]) {
                Ok(()) => Ok(FileToolCapability {
                    action: ToolPermissionAction::Ask,
                    summary: format!("edit {}", input.path),
                    touched_paths: touched,
                }),
                Err(reason) => Ok(deny(reason, touched)),
            }
        }
        FileTool::ApplyPatch => {
            let input: ApplyPatchInput = parse_args(arguments)?;
            // Parsing surfaces every referenced path so we can guard them all;
            // a parse error is reported now (the handler will report it too).
            let ops = match parse_v4a(&input.patch) {
                Ok(ops) => ops,
                Err(error) => {
                    return Ok(FileToolCapability {
                        action: ToolPermissionAction::Deny,
                        summary: format!("apply_patch could not be parsed: {error}"),
                        touched_paths: Vec::new(),
                    });
                }
            };
            let touched = patch_paths(&ops);
            let refs: Vec<&str> = touched.iter().map(String::as_str).collect();
            match guard_paths(workspace, &refs) {
                Ok(()) => Ok(FileToolCapability {
                    action: ToolPermissionAction::Ask,
                    summary: format!("apply_patch touching {} file(s)", touched.len()),
                    touched_paths: touched,
                }),
                Err(reason) => Ok(deny(reason, touched)),
            }
        }
    }
}

/// Compute a side-effect-free diff preview for a *mutating* file tool, for the
/// approval card the UI shows before the Approve button. Returns `None` for
/// `read_file` (nothing to preview) or when the change cannot be previewed
/// (file missing, edit would not match, patch would not apply) — the runner
/// falls back to the capability summary in that case. This never writes
/// anything: `edit_file` dry-runs [`apply_edit`] on the current content,
/// `apply_patch` dry-runs [`plan_patch`], and `write_file` diffs old vs. new.
pub fn preview_diff(
    tool: FileTool,
    arguments: &Value,
    workspace: &Workspace,
    fs: &dyn FileSystem,
) -> Option<String> {
    match tool {
        FileTool::Read => None,
        FileTool::Write => {
            let input: WriteFileInput = parse_args(arguments).ok()?;
            let resolved = resolve_guarded(workspace, &input.path).ok()?;
            let old = if fs.exists(&resolved) {
                fs.read(&resolved)
                    .ok()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            } else {
                None
            };
            let new_text = match &old {
                Some(old) => match_bom_and_eol(old, &input.content),
                None => input.content.clone(),
            };
            Some(render_full_diff(old.as_deref().unwrap_or(""), &new_text))
        }
        FileTool::Edit => {
            let input: EditFileInput = parse_args(arguments).ok()?;
            let resolved = resolve_guarded(workspace, &input.path).ok()?;
            let bytes = fs.read(&resolved).ok()?;
            let content = String::from_utf8_lossy(&bytes).into_owned();
            let replace_all = input.replace_all.unwrap_or(false);
            apply_edit(&content, &input.old_text, &input.new_text, replace_all)
                .ok()
                .map(|applied| applied.diff)
        }
        FileTool::ApplyPatch => {
            let input: ApplyPatchInput = parse_args(arguments).ok()?;
            let ops = parse_v4a(&input.patch).ok()?;
            let file_map = WorkspaceFileMap::new(workspace, fs);
            let plan = plan_patch(&ops, &file_map).ok()?;
            let mut out = String::new();
            for file in &plan.files {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "# {} {} (+{} -{})\n",
                    planned_kind_name(&file.op),
                    file.path,
                    file.added,
                    file.removed
                ));
            }
            Some(out)
        }
    }
}

fn deny(reason: String, touched: Vec<String>) -> FileToolCapability {
    FileToolCapability {
        action: ToolPermissionAction::Deny,
        summary: reason,
        touched_paths: touched,
    }
}

/// Resolve and screen a set of paths: every path must resolve inside the
/// workspace and must not be sensitive. Returns a human-readable denial reason
/// on the first offending path.
fn guard_paths(workspace: &Workspace, paths: &[&str]) -> Result<(), String> {
    for path in paths {
        let resolved = workspace
            .resolve(path)
            .map_err(|error: PathError| error.to_string())?;
        if workspace.is_sensitive(&resolved) {
            return Err(format!("`{path}` is a sensitive path and is blocked"));
        }
    }
    Ok(())
}

// ===========================================================================
// read_file
// ===========================================================================

/// Run `read_file`. Resolves + contains the path, reads via the [`FileSystem`],
/// refuses binary content, computes a sha256, and returns line-numbered text.
/// Output larger than `maxBytes` spills to `spill` (when provided) and the model
/// gets a preview + reference instead of the full bytes.
pub fn read_file(
    arguments: &Value,
    workspace: &Workspace,
    fs: &dyn FileSystem,
    tool_call_id: &str,
    spill: Option<&dyn FileToolSpill>,
) -> Result<FileToolOutcome, FileToolError> {
    let input: ReadFileInput = parse_args(arguments)?;
    let resolved = match resolve_guarded(workspace, &input.path) {
        Ok(path) => path,
        Err(reason) => return Ok(path_failure(&input.path, reason)),
    };

    // Gate on size first: cap the read at MAX_READ_FILE_BYTES so a huge file is
    // never fully loaded. The byte budget is widened to the caller's `maxBytes`
    // request when it asks for more than the default ceiling, so an explicit
    // large read still works up to the hard limit. The sha is computed over the
    // bytes actually read; when the read was capped that is a prefix hash, which
    // we surface via `bytesTruncated`.
    let requested_max = input.max_bytes.unwrap_or(DEFAULT_READ_MAX_BYTES);
    let read_ceiling = requested_max.max(MAX_READ_FILE_BYTES);
    let (bytes, bytes_truncated) = fs
        .read_capped(&resolved, read_ceiling)
        .map_err(|error| FileToolError::Io(error.to_string()))?;
    let sha = sha256_hex(&bytes);

    if looks_binary(&bytes) {
        return Ok(FileToolOutcome {
            ok: false,
            model_text: format!(
                "file appears to be binary ({} bytes); not shown. Use a different tool or inspect it directly.",
                bytes.len()
            ),
            data: json!({
                "path": input.path,
                "size": bytes.len(),
                "binary": true,
                "sha256": sha,
                "hint": "binary content is not streamed into the model context",
            }),
            sha256: Some(sha),
            diff: None,
        });
    }

    // Lossy decode is safe here: the binary guard already rejected NUL-bearing /
    // mostly-non-text payloads, so this only ever touches genuine text. We still
    // record whether a strict decode would have failed so the model/UI know the
    // shown text was lossily decoded (e.g. a Windows-1251 file that slipped past
    // the binary heuristic) — the display still happens, just flagged.
    let lossy = std::str::from_utf8(&bytes).is_err();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let all_lines: Vec<&str> = split_keep_lines(&text);
    let total_lines = all_lines.len();

    // 1-based start line; default 1. `limit` caps the number of lines returned.
    let start_line = input.start_line.unwrap_or(1).max(1);
    let start_idx = start_line - 1;
    let selected: Vec<(usize, &str)> = all_lines
        .iter()
        .enumerate()
        .skip(start_idx)
        .take(input.limit.unwrap_or(usize::MAX))
        .map(|(idx, line)| (idx + 1, *line))
        .collect();

    let numbered = render_numbered(&selected);
    let max_bytes = requested_max;

    let end_line = selected.last().map(|(n, _)| *n).unwrap_or(start_idx);
    let mut data = json!({
        "path": input.path,
        "sha256": sha,
        "totalLines": total_lines,
        "startLine": start_line,
        "endLine": end_line,
        "truncated": false,
    });
    if lossy {
        // The shown text was lossily decoded (invalid UTF-8 bytes → U+FFFD).
        data["lossy"] = json!(true);
        data["encoding"] = json!("lossy-utf8");
    }
    if bytes_truncated {
        // The file exceeded the read ceiling; only a prefix was loaded, so the
        // sha and line view describe that prefix, not the whole file.
        data["bytesTruncated"] = json!(true);
        data["truncated"] = json!(true);
        data["readCeilingBytes"] = json!(read_ceiling);
    }

    if numbered.len() > max_bytes {
        // Oversized: spill the full numbered view (when a sink is available) and
        // return a bounded preview plus a reference.
        let preview = take_prefix_on_char_boundary(&numbered, max_bytes);
        let log_ref = match spill {
            Some(spill) => Some(
                spill
                    .spill(tool_call_id, &numbered)
                    .map_err(|error| FileToolError::Io(error.to_string()))?,
            ),
            None => None,
        };
        data["truncated"] = json!(true);
        if let Some(log_ref) = &log_ref {
            data["logRef"] = json!(log_ref);
        }
        let mut model_text = preview;
        model_text.push_str("\n... output truncated ...\n");
        if let Some(log_ref) = &log_ref {
            model_text.push_str(&format!("full content: {log_ref}\n"));
        }
        if bytes_truncated {
            model_text.push_str(&format!(
                "(file exceeds the {read_ceiling}-byte read ceiling; only the leading bytes were read)\n"
            ));
        }
        model_text.push_str(&format!("(total lines: {total_lines})\n"));
        return Ok(FileToolOutcome {
            ok: true,
            model_text,
            data,
            sha256: Some(sha),
            diff: None,
        });
    }

    // The rendered view fits the byte budget, but the file itself may have been
    // size-capped off disk; tell the model so it can page for the remainder.
    let mut model_text = numbered;
    if bytes_truncated {
        model_text.push_str(&format!(
            "\n... file exceeds the {read_ceiling}-byte read ceiling; only the leading bytes were read. Use startLine/limit to page further ...\n"
        ));
    }

    Ok(FileToolOutcome {
        ok: true,
        model_text,
        data,
        sha256: Some(sha),
        diff: None,
    })
}

// ===========================================================================
// write_file
// ===========================================================================

/// Run `write_file`. Creates or fully overwrites a file. Honors `create` /
/// `overwrite` flags and an `expectedSha256` precondition; preserves a leading
/// BOM and the dominant line ending of an existing file; writes atomically.
pub fn write_file(
    arguments: &Value,
    workspace: &Workspace,
    fs: &dyn FileSystem,
) -> Result<FileToolOutcome, FileToolError> {
    let input: WriteFileInput = parse_args(arguments)?;
    let resolved = match resolve_guarded(workspace, &input.path) {
        Ok(path) => path,
        Err(reason) => return Ok(path_failure(&input.path, reason)),
    };

    let existed = fs.exists(&resolved);
    let create = input.create.unwrap_or(true);
    let overwrite = input.overwrite.unwrap_or(false);

    if !existed && !create {
        return Ok(FileToolOutcome::failure(
            format!("`{}` does not exist and create was not requested", input.path),
            json!({ "path": input.path, "status": "missing" }),
        ));
    }

    let old_bytes = if existed {
        Some(fs.read(&resolved).map_err(|error| FileToolError::Io(error.to_string()))?)
    } else {
        None
    };

    if existed {
        let old = old_bytes.as_deref().unwrap_or_default();
        let old_sha = sha256_hex(old);
        if let Some(expected) = &input.expected_sha256 {
            if !expected.eq_ignore_ascii_case(&old_sha) {
                return Ok(FileToolOutcome::failure(
                    format!(
                        "`{}` changed on disk (expected sha256 {expected}, found {old_sha}); not written",
                        input.path
                    ),
                    json!({
                        "path": input.path,
                        "status": "conflict",
                        "expectedSha256": expected,
                        "actualSha256": old_sha,
                    }),
                ));
            }
        } else if !overwrite {
            return Ok(FileToolOutcome::failure(
                format!(
                    "`{}` already exists; pass overwrite or expectedSha256 to replace it",
                    input.path
                ),
                json!({ "path": input.path, "status": "exists", "sha256": old_sha }),
            ));
        }
    }

    // Preserve BOM + EOL style of the existing file so a full overwrite does not
    // silently reformat line endings.
    let old_text = old_bytes
        .as_deref()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
    let final_text = match &old_text {
        Some(old) => match_bom_and_eol(old, &input.content),
        None => input.content.clone(),
    };
    let final_bytes = final_text.as_bytes();

    fs.write_atomic(&resolved, final_bytes)
        .map_err(|error| FileToolError::Io(error.to_string()))?;

    let new_sha = sha256_hex(final_bytes);
    let diff = render_full_diff(old_text.as_deref().unwrap_or(""), &final_text);
    let status = if existed { "modified" } else { "created" };
    let model_text = format!(
        "{status} {} ({} bytes, sha256 {new_sha})",
        input.path,
        final_bytes.len()
    );

    Ok(FileToolOutcome {
        ok: true,
        model_text,
        data: json!({
            "path": input.path,
            "status": status,
            "bytes": final_bytes.len(),
            "sha256": new_sha,
        }),
        sha256: Some(new_sha),
        diff: Some(diff),
    })
}

// ===========================================================================
// edit_file
// ===========================================================================

/// Run `edit_file`. Reads the file, checks an optional `expectedSha256`, applies
/// the pure [`apply_edit`] matcher, and atomically writes the new content
/// verbatim (the matcher preserves EOL/BOM). Expected, deterministic failures
/// (not found / ambiguous / no change) are returned as a non-ok outcome.
pub fn edit_file(
    arguments: &Value,
    workspace: &Workspace,
    fs: &dyn FileSystem,
) -> Result<FileToolOutcome, FileToolError> {
    let input: EditFileInput = parse_args(arguments)?;
    let resolved = match resolve_guarded(workspace, &input.path) {
        Ok(path) => path,
        Err(reason) => return Ok(path_failure(&input.path, reason)),
    };

    if !fs.exists(&resolved) {
        return Ok(FileToolOutcome::failure(
            format!("`{}` does not exist", input.path),
            json!({ "path": input.path, "status": "missing" }),
        ));
    }

    let bytes = fs.read(&resolved).map_err(|error| FileToolError::Io(error.to_string()))?;
    let old_sha = sha256_hex(&bytes);
    if let Some(expected) = &input.expected_sha256 {
        if !expected.eq_ignore_ascii_case(&old_sha) {
            return Ok(FileToolOutcome::failure(
                format!(
                    "`{}` changed on disk (expected sha256 {expected}, found {old_sha}); not edited",
                    input.path
                ),
                json!({
                    "path": input.path,
                    "status": "conflict",
                    "expectedSha256": expected,
                    "actualSha256": old_sha,
                }),
            ));
        }
    }

    // Mutating tools must REFUSE non-UTF-8 content: `apply_edit` rewrites the
    // whole file, so a lossy decode would replace every invalid byte with U+FFFD
    // across the entire file (silent data loss), not just the edited span.
    let content = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_string(),
        Err(_) => {
            return Ok(FileToolOutcome::failure(
                format!(
                    "`{}` is not valid UTF-8; refusing to edit to avoid corrupting non-UTF-8 bytes",
                    input.path
                ),
                json!({ "path": input.path, "status": "not_utf8" }),
            ));
        }
    };
    let replace_all = input.replace_all.unwrap_or(false);
    match apply_edit(&content, &input.old_text, &input.new_text, replace_all) {
        Ok(applied) => {
            fs.write_atomic(&resolved, applied.new_content.as_bytes())
                .map_err(|error| FileToolError::Io(error.to_string()))?;
            let new_sha = sha256_hex(applied.new_content.as_bytes());
            let model_text = format!(
                "edited {} ({} occurrence(s), strategy {:?}, sha256 {new_sha})",
                input.path, applied.occurrences, applied.strategy
            );
            Ok(FileToolOutcome {
                ok: true,
                model_text,
                data: json!({
                    "path": input.path,
                    "status": "modified",
                    "occurrences": applied.occurrences,
                    "strategy": format!("{:?}", applied.strategy),
                    "sha256": new_sha,
                }),
                sha256: Some(new_sha),
                diff: Some(applied.diff),
            })
        }
        Err(error) => {
            let (message, detail) = describe_edit_error(&input.path, &error);
            Ok(FileToolOutcome::failure(message, detail))
        }
    }
}

fn describe_edit_error(path: &str, error: &EditError) -> (String, Value) {
    match error {
        EditError::NotFound => (
            format!("oldText was not found in `{path}`; re-read the file and copy the exact text"),
            json!({ "path": path, "status": "not_found" }),
        ),
        EditError::Ambiguous { count, snippets } => (
            format!(
                "oldText matched {count} locations in `{path}`; add surrounding context or pass replaceAll"
            ),
            json!({
                "path": path,
                "status": "ambiguous",
                "count": count,
                "snippets": snippets,
            }),
        ),
        EditError::NoChange => (
            format!("oldText and newText are identical for `{path}`; nothing to change"),
            json!({ "path": path, "status": "no_change" }),
        ),
    }
}

// ===========================================================================
// apply_patch
// ===========================================================================

/// Run `apply_patch`. Parses the V4A envelope, dry-runs it against a
/// [`FileMap`] built over the workspace (the dry run is the all-or-none content
/// check), and only then applies the plan atomically. A parse or plan failure
/// leaves the disk untouched.
pub fn apply_patch(
    arguments: &Value,
    workspace: &Workspace,
    fs: &dyn FileSystem,
) -> Result<FileToolOutcome, FileToolError> {
    let input: ApplyPatchInput = parse_args(arguments)?;

    let ops = match parse_v4a(&input.patch) {
        Ok(ops) => ops,
        Err(error) => {
            return Ok(FileToolOutcome::failure(
                format!("patch could not be parsed: {error}"),
                json!({ "status": "parse_error", "detail": error.to_string() }),
            ));
        }
    };

    // Guard every referenced path up front: any outside-workspace or sensitive
    // target aborts the whole patch before reading or writing anything.
    let touched = patch_paths(&ops);
    let refs: Vec<&str> = touched.iter().map(String::as_str).collect();
    if let Err(reason) = guard_paths(workspace, &refs) {
        return Ok(FileToolOutcome::failure(
            reason,
            json!({ "status": "denied", "paths": touched }),
        ));
    }

    // Refuse the whole patch up front if any file it must read (an Update source
    // or a Delete target) exists but is not valid UTF-8. The planner + apply
    // rewrite the full file from a lossy decode, which would replace every
    // non-UTF-8 byte with U+FFFD — silent corruption. We abort with no writes
    // rather than corrupt. (Add destinations are written wholesale from the
    // patch body, so they are not checked here.)
    for path in must_exist_paths(&ops) {
        let Ok(resolved) = workspace.resolve(&path) else {
            // Resolution already passed guard_paths above; a failure here is a
            // race and is handled by the planner/apply step.
            continue;
        };
        if !fs.exists(&resolved) {
            continue;
        }
        let bytes = fs
            .read(&resolved)
            .map_err(|error| FileToolError::Io(error.to_string()))?;
        if std::str::from_utf8(&bytes).is_err() {
            return Ok(FileToolOutcome::failure(
                format!(
                    "`{path}` is not valid UTF-8; refusing to apply the patch to avoid corrupting non-UTF-8 bytes"
                ),
                json!({ "status": "not_utf8", "path": path }),
            ));
        }
    }

    // Build a read-only FileMap over the workspace so plan_patch can verify every
    // hunk against current on-disk content.
    let file_map = WorkspaceFileMap::new(workspace, fs);
    let plan = match plan_patch(&ops, &file_map) {
        Ok(plan) => plan,
        Err(error) => {
            return Ok(FileToolOutcome::failure(
                format!("patch did not apply cleanly: {error}"),
                json!({ "status": "plan_error", "detail": error.to_string() }),
            ));
        }
    };

    // Snapshot the prior state of every path the plan will write/move/delete
    // (and, for a move, the destination too) so we can make the apply step
    // all-or-none on disk: plan_patch is all-or-none at the *planning* stage, but
    // a mid-apply IO error (e.g. a later write fails) would otherwise leave
    // earlier files changed. We capture, apply, and on ANY error restore every
    // touched path to its captured state and report failure with no net change.
    let snapshot = match capture_snapshot(workspace, fs, &plan) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Ok(FileToolOutcome::failure(
                format!("could not read current file state before applying patch: {error}"),
                json!({ "status": "io_error", "detail": error.to_string() }),
            ));
        }
    };

    let mut combined_diff = String::new();
    let mut applied_files = Vec::with_capacity(plan.files.len());
    for file in &plan.files {
        if let Err(error) = apply_planned_file(workspace, fs, file) {
            // Roll back everything captured, then report. A rollback failure is
            // appended so the operator knows the disk may be partially restored.
            let rollback = restore_snapshot(fs, &snapshot);
            let mut detail = error.to_string();
            if let Err(rollback_error) = rollback {
                detail = format!(
                    "{detail}; rollback also failed and the workspace may be partially modified: {rollback_error}"
                );
            }
            return Ok(FileToolOutcome::failure(
                format!("patch failed while applying `{}`: {detail}", file.path),
                json!({ "status": "apply_error", "path": file.path, "detail": detail }),
            ));
        }
        let op = planned_kind_name(&file.op);
        if !combined_diff.is_empty() {
            combined_diff.push('\n');
        }
        combined_diff.push_str(&format!("# {op} {}\n", file.path));
        applied_files.push(json!({
            "path": file.path,
            "op": op,
            "added": file.added,
            "removed": file.removed,
        }));
    }

    let model_text = format!(
        "applied patch to {} file(s): {}",
        plan.files.len(),
        plan.files
            .iter()
            .map(|f| format!("{} ({})", f.path, planned_kind_name(&f.op)))
            .collect::<Vec<_>>()
            .join(", ")
    );

    Ok(FileToolOutcome {
        ok: true,
        model_text,
        data: json!({ "status": "applied", "files": applied_files }),
        sha256: None,
        diff: Some(combined_diff),
    })
}

/// Apply one planned file change atomically through the [`FileSystem`] port.
fn apply_planned_file(
    workspace: &Workspace,
    fs: &dyn FileSystem,
    file: &PlannedFile,
) -> Result<(), FileToolError> {
    let source = resolve_for_apply(workspace, &file.path)?;
    match &file.op {
        PlannedKind::Add | PlannedKind::Update => {
            let content = file.new_content.as_deref().unwrap_or("");
            fs.write_atomic(&source, content.as_bytes())
                .map_err(|error| FileToolError::Io(error.to_string()))?;
        }
        PlannedKind::Move { to } => {
            let dest = resolve_for_apply(workspace, to)?;
            let content = file.new_content.as_deref().unwrap_or("");
            // Write the (possibly edited) content to the destination, then drop
            // the source. Writing-then-removing keeps the new content durable
            // even if the source removal races.
            fs.write_atomic(&dest, content.as_bytes())
                .map_err(|error| FileToolError::Io(error.to_string()))?;
            if fs.exists(&source) {
                fs.remove_file(&source)
                    .map_err(|error| FileToolError::Io(error.to_string()))?;
            }
        }
        PlannedKind::Delete => {
            if fs.exists(&source) {
                fs.remove_file(&source)
                    .map_err(|error| FileToolError::Io(error.to_string()))?;
            }
        }
    }
    Ok(())
}

fn planned_kind_name(kind: &PlannedKind) -> &'static str {
    match kind {
        PlannedKind::Add => "add",
        PlannedKind::Update => "update",
        PlannedKind::Move { .. } => "move",
        PlannedKind::Delete => "delete",
    }
}

/// The prior on-disk state of a single path before a patch is applied: either
/// the bytes it held, or that it did not exist. Used to roll a partially-applied
/// patch back to its pre-apply state.
struct PathSnapshot {
    path: PathBuf,
    /// `Some(bytes)` if the file existed; `None` if it did not.
    prior: Option<Vec<u8>>,
}

/// Capture the prior state of every path a plan will write, move, or delete
/// (including a move's destination), so the apply step can be rolled back to a
/// clean state on any mid-apply failure. Reads through the [`FileSystem`] port.
fn capture_snapshot(
    workspace: &Workspace,
    fs: &dyn FileSystem,
    plan: &PatchPlan,
) -> Result<Vec<PathSnapshot>, FileToolError> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for file in &plan.files {
        let source = resolve_for_apply(workspace, &file.path)?;
        if !paths.contains(&source) {
            paths.push(source);
        }
        if let PlannedKind::Move { to } = &file.op {
            let dest = resolve_for_apply(workspace, to)?;
            if !paths.contains(&dest) {
                paths.push(dest);
            }
        }
    }

    let mut snapshot = Vec::with_capacity(paths.len());
    for path in paths {
        let prior = if fs.exists(&path) {
            Some(
                fs.read(&path)
                    .map_err(|error| FileToolError::Io(error.to_string()))?,
            )
        } else {
            None
        };
        snapshot.push(PathSnapshot { path, prior });
    }
    Ok(snapshot)
}

/// Restore every path in `snapshot` to its captured state: rewrite the prior
/// bytes for files that existed, and delete files that did not exist before.
/// Best-effort across all entries — the first error is remembered and returned
/// after attempting the rest, so one stuck path does not strand the others.
fn restore_snapshot(fs: &dyn FileSystem, snapshot: &[PathSnapshot]) -> Result<(), FileToolError> {
    let mut first_error: Option<FileToolError> = None;
    for entry in snapshot {
        let result = match &entry.prior {
            Some(bytes) => fs.write_atomic(&entry.path, bytes),
            None => {
                if fs.exists(&entry.path) {
                    fs.remove_file(&entry.path)
                } else {
                    Ok(())
                }
            }
        };
        if let Err(error) = result {
            if first_error.is_none() {
                first_error = Some(FileToolError::Io(error.to_string()));
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The patch paths that MUST already exist for the patch to apply: the source
/// of every Update and the target of every Delete. (An Add destination need not
/// pre-exist; a Move destination must NOT pre-exist.) Used for the up-front
/// UTF-8 refusal check.
fn must_exist_paths(ops: &[super::patch::PatchOp]) -> Vec<String> {
    use super::patch::PatchOp;
    let mut paths = Vec::new();
    for op in ops {
        match op {
            PatchOp::Update { path, .. } | PatchOp::Delete { path } => paths.push(path.clone()),
            PatchOp::Add { .. } => {}
        }
    }
    paths
}

/// Resolve a path during apply. The path already passed `guard_paths`, so this
/// only converts a resolution error into a [`FileToolError`] for the rare race
/// where the tree changed between guard and apply.
fn resolve_for_apply(workspace: &Workspace, path: &str) -> Result<PathBuf, FileToolError> {
    workspace
        .resolve(path)
        .map_err(|error| FileToolError::InvalidArguments(error.to_string()))
}

/// A [`FileMap`] adapter that reads through the workspace + [`FileSystem`] port.
/// Paths are the patch's own (workspace-relative or absolute) strings; reads
/// that resolve outside the workspace are treated as "missing" (the up-front
/// `guard_paths` already rejected such patches, so this is purely defensive).
struct WorkspaceFileMap<'a> {
    workspace: &'a Workspace,
    fs: &'a dyn FileSystem,
}

impl<'a> WorkspaceFileMap<'a> {
    fn new(workspace: &'a Workspace, fs: &'a dyn FileSystem) -> Self {
        Self { workspace, fs }
    }

    fn resolved(&self, path: &str) -> Option<PathBuf> {
        self.workspace.resolve(path).ok()
    }
}

impl FileMap for WorkspaceFileMap<'_> {
    fn read(&self, path: &str) -> Option<String> {
        let resolved = self.resolved(path)?;
        let bytes = self.fs.read(&resolved).ok()?;
        // Strict decode: a mutating patch rewrites the whole file, so a lossy
        // decode here would corrupt non-UTF-8 bytes. `apply_patch` already aborts
        // up front when a must-exist file is invalid UTF-8, so reaching this with
        // invalid bytes is a race; surfacing it as `None` (looks missing) is the
        // safe fallback — the planner reports it as a clean failure with no write.
        std::str::from_utf8(&bytes).ok().map(str::to_string)
    }

    fn exists(&self, path: &str) -> bool {
        match self.resolved(path) {
            Some(resolved) => self.fs.exists(&resolved),
            None => false,
        }
    }
}

// ===========================================================================
// Shared helpers
// ===========================================================================

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: &Value) -> Result<T, FileToolError> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| FileToolError::InvalidArguments(error.to_string()))
}

/// The full set of paths a patch references (sources and move destinations).
fn patch_paths(ops: &[super::patch::PatchOp]) -> Vec<String> {
    use super::patch::PatchOp;
    let mut paths = Vec::new();
    for op in ops {
        match op {
            PatchOp::Add { path, .. } | PatchOp::Delete { path } => paths.push(path.clone()),
            PatchOp::Update { path, move_to, .. } => {
                paths.push(path.clone());
                if let Some(dest) = move_to {
                    paths.push(dest.clone());
                }
            }
        }
    }
    paths
}

/// Resolve a path and screen it for sensitivity, returning a denial reason on
/// failure (so the handler can emit a non-ok outcome rather than an error).
fn resolve_guarded(workspace: &Workspace, path: &str) -> Result<PathBuf, String> {
    let resolved = workspace
        .resolve(path)
        .map_err(|error: PathError| error.to_string())?;
    if workspace.is_sensitive(&resolved) {
        return Err(format!("`{path}` is a sensitive path and is blocked"));
    }
    Ok(resolved)
}

fn path_failure(path: &str, reason: String) -> FileToolOutcome {
    FileToolOutcome::failure(
        reason.clone(),
        json!({ "path": path, "status": "denied", "reason": reason }),
    )
}

/// Heuristic binary-content guard over the leading [`BINARY_SNIFF_BYTES`] bytes:
/// any NUL byte, or more than 30% non-printable bytes, marks the file binary.
fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let sample = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    let mut non_printable = 0usize;
    for &byte in sample {
        if byte == 0 {
            return true;
        }
        // Printable: tab, LF, CR, and the printable ASCII range; bytes >= 0x80
        // are treated as potentially-UTF8 text (counted as printable) so we do
        // not falsely flag UTF-8 documents.
        let printable =
            matches!(byte, b'\t' | b'\n' | b'\r') || (0x20..=0x7E).contains(&byte) || byte >= 0x80;
        if !printable {
            non_printable += 1;
        }
    }
    non_printable * 100 > sample.len() * 30
}

/// Split text into lines for display, dropping the single trailing empty element
/// produced by a final newline (so a newline-terminated file does not show a
/// phantom blank last line). Each returned line excludes its terminator.
fn split_keep_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    if lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Render selected `(line_number, text)` pairs in `cat -n` style: a 6-wide,
/// right-aligned line number, an arrow, then the line.
fn render_numbered(selected: &[(usize, &str)]) -> String {
    let mut out = String::new();
    for (number, line) in selected {
        out.push_str(&format!("{number:6}\u{2192}{line}\n"));
    }
    out
}

/// Take the longest prefix of `text` not exceeding `max_bytes`, ending on a
/// char boundary so the preview is always valid UTF-8.
fn take_prefix_on_char_boundary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Hex-encode the SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Make `new_content` match the leading BOM and dominant line-ending of
/// `old_content`, so a full overwrite does not silently change either. The
/// caller has already decided to write `new_content`; this only normalizes
/// presentation to the file's prior conventions.
fn match_bom_and_eol(old_content: &str, new_content: &str) -> String {
    let had_bom = old_content.starts_with('\u{FEFF}');
    let uses_crlf = detect_crlf(old_content);

    // Normalize the new content to LF first, then re-apply CRLF if needed.
    let body_had_bom = new_content.starts_with('\u{FEFF}');
    let body = if body_had_bom {
        &new_content['\u{FEFF}'.len_utf8()..]
    } else {
        new_content
    };
    let lf_body: String = body.replace("\r\n", "\n");
    let eol_body = if uses_crlf {
        lf_body.replace('\n', "\r\n")
    } else {
        lf_body
    };

    if had_bom {
        format!("\u{FEFF}{eol_body}")
    } else {
        eol_body
    }
}

/// Whether `text` predominantly uses CRLF line endings (more CRLF than lone LF).
fn detect_crlf(text: &str) -> bool {
    let crlf = text.matches("\r\n").count();
    if crlf == 0 {
        return false;
    }
    let total_lf = text.matches('\n').count();
    let lone_lf = total_lf.saturating_sub(crlf);
    crlf >= lone_lf
}

/// Render a minimal unified-diff-style preview replacing the whole old content
/// with the whole new content (used by `write_file`). Cheap and dependency-free;
/// only meant to feed an approval/UI card.
fn render_full_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = if old.is_empty() {
        Vec::new()
    } else {
        split_keep_lines(old)
    };
    let new_lines: Vec<&str> = if new.is_empty() {
        Vec::new()
    } else {
        split_keep_lines(new)
    };
    let mut out = format!("@@ -1,{} +1,{} @@\n", old_lines.len(), new_lines.len());
    for line in &old_lines {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in &new_lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::filesystem::{FileMetadata, StdFileSystem};
    use std::fs;

    fn temp_workspace(label: &str) -> (Workspace, PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mothership_ft_{label}_{nanos}"));
        fs::create_dir_all(&dir).expect("create workspace");
        let ws = Workspace::new(&dir).expect("workspace");
        (ws, dir)
    }

    fn sha_of(text: &str) -> String {
        sha256_hex(text.as_bytes())
    }

    // ---- read_file --------------------------------------------------------

    #[test]
    fn read_file_numbers_lines_and_hashes() {
        let (ws, dir) = temp_workspace("read_numbered");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"alpha\nbeta\ngamma\n").unwrap();

        let out = read_file(&json!({ "path": "a.txt" }), &ws, &fs, "tc1", None).unwrap();
        assert!(out.ok);
        assert!(out.model_text.contains("\u{2192}alpha"));
        assert!(out.model_text.contains("     1\u{2192}alpha"));
        assert!(out.model_text.contains("     3\u{2192}gamma"));
        assert_eq!(out.data["totalLines"], 3);
        assert_eq!(out.sha256.as_deref(), Some(sha_of("alpha\nbeta\ngamma\n").as_str()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_honors_start_and_limit() {
        let (ws, dir) = temp_workspace("read_offset");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"l1\nl2\nl3\nl4\nl5\n").unwrap();

        let out = read_file(
            &json!({ "path": "a.txt", "startLine": 2, "limit": 2 }),
            &ws,
            &fs,
            "tc2",
            None,
        )
        .unwrap();
        assert!(out.ok);
        assert!(out.model_text.contains("     2\u{2192}l2"));
        assert!(out.model_text.contains("     3\u{2192}l3"));
        assert!(!out.model_text.contains("l1"));
        assert!(!out.model_text.contains("l4"));
        assert_eq!(out.data["startLine"], 2);
        assert_eq!(out.data["endLine"], 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_refuses_binary() {
        let (ws, dir) = temp_workspace("read_binary");
        let fs = StdFileSystem::new();
        fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3, 4, 0, 9, 9]).unwrap();

        let out = read_file(&json!({ "path": "blob.bin" }), &ws, &fs, "tc3", None).unwrap();
        assert!(!out.ok);
        assert!(out.model_text.contains("binary"));
        assert_eq!(out.data["binary"], true);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_spills_when_oversized() {
        let (ws, dir) = temp_workspace("read_spill");
        let fs = StdFileSystem::new();
        let big = "x".repeat(10_000) + "\n";
        fs::write(dir.join("big.txt"), big.as_bytes()).unwrap();

        struct CapturingSpill;
        impl FileToolSpill for CapturingSpill {
            fn spill(&self, _id: &str, _content: &str) -> std::io::Result<String> {
                Ok("spill://ref".to_string())
            }
        }

        let out = read_file(
            &json!({ "path": "big.txt", "maxBytes": 100 }),
            &ws,
            &fs,
            "tc4",
            Some(&CapturingSpill),
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.data["truncated"], true);
        assert_eq!(out.data["logRef"], "spill://ref");
        assert!(out.model_text.contains("output truncated"));
        assert!(out.model_text.len() < 1000);

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- write_file -------------------------------------------------------

    #[test]
    fn write_file_creates_new() {
        let (ws, dir) = temp_workspace("write_create");
        let fs = StdFileSystem::new();

        let out = write_file(
            &json!({ "path": "new.txt", "content": "hello\n" }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.data["status"], "created");
        assert_eq!(fs::read(dir.join("new.txt")).unwrap(), b"hello\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_refuses_existing_without_overwrite() {
        let (ws, dir) = temp_workspace("write_noover");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"old\n").unwrap();

        let out = write_file(
            &json!({ "path": "a.txt", "content": "new\n" }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "exists");
        // File unchanged.
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"old\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_overwrites_with_flag() {
        let (ws, dir) = temp_workspace("write_over");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"old\n").unwrap();

        let out = write_file(
            &json!({ "path": "a.txt", "content": "new\n", "overwrite": true }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.data["status"], "modified");
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"new\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_expected_sha_conflict_blocks_write() {
        let (ws, dir) = temp_workspace("write_sha");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"old\n").unwrap();

        let out = write_file(
            &json!({
                "path": "a.txt",
                "content": "new\n",
                "expectedSha256": "deadbeef",
            }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "conflict");
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"old\n");

        // Correct sha lets it through.
        let good = sha_of("old\n");
        let out = write_file(
            &json!({
                "path": "a.txt",
                "content": "new\n",
                "expectedSha256": good,
            }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"new\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_preserves_crlf_and_bom() {
        let (ws, dir) = temp_workspace("write_eol");
        let fs = StdFileSystem::new();
        // Existing file: BOM + CRLF.
        fs::write(dir.join("a.txt"), "\u{FEFF}one\r\ntwo\r\n".as_bytes()).unwrap();

        let out = write_file(
            &json!({ "path": "a.txt", "content": "alpha\nbeta\n", "overwrite": true }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(out.ok);
        let written = fs::read(dir.join("a.txt")).unwrap();
        assert_eq!(written, "\u{FEFF}alpha\r\nbeta\r\n".as_bytes());

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- edit_file --------------------------------------------------------

    #[test]
    fn edit_file_applies_unique_edit() {
        let (ws, dir) = temp_workspace("edit_ok");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.rs"), b"fn main() {\n    let x = 1;\n}\n").unwrap();

        let out = edit_file(
            &json!({ "path": "a.rs", "oldText": "let x = 1;", "newText": "let x = 2;" }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.data["occurrences"], 1);
        assert!(out.diff.is_some());
        assert_eq!(
            fs::read(dir.join("a.rs")).unwrap(),
            b"fn main() {\n    let x = 2;\n}\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_file_not_found_leaves_file_unchanged() {
        let (ws, dir) = temp_workspace("edit_nf");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.rs"), b"let x = 1;\n").unwrap();

        let out = edit_file(
            &json!({ "path": "a.rs", "oldText": "let y = 9;", "newText": "z" }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "not_found");
        assert_eq!(fs::read(dir.join("a.rs")).unwrap(), b"let x = 1;\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_file_ambiguous_reports_snippets() {
        let (ws, dir) = temp_workspace("edit_amb");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"x = 1\ny = 1\n").unwrap();

        let out = edit_file(
            &json!({ "path": "a.txt", "oldText": "= 1", "newText": "= 9" }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "ambiguous");
        assert_eq!(out.data["count"], 2);
        // Unchanged.
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"x = 1\ny = 1\n");

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- apply_patch ------------------------------------------------------

    #[test]
    fn apply_patch_updates_file() {
        let (ws, dir) = temp_workspace("patch_ok");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"keep\nold\ntail\n").unwrap();

        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
 keep
-old
+new
 tail
*** End Patch
";
        let out = apply_patch(&json!({ "patch": patch }), &ws, &fs).unwrap();
        assert!(out.ok, "{}", out.model_text);
        assert_eq!(out.data["files"][0]["op"], "update");
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"keep\nnew\ntail\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_patch_add_and_delete() {
        let (ws, dir) = temp_workspace("patch_addel");
        let fs = StdFileSystem::new();
        fs::write(dir.join("gone.txt"), b"bye\n").unwrap();

        let patch = "\
*** Begin Patch
*** Add File: new.txt
+fresh
*** Delete File: gone.txt
*** End Patch
";
        let out = apply_patch(&json!({ "patch": patch }), &ws, &fs).unwrap();
        assert!(out.ok, "{}", out.model_text);
        assert!(dir.join("new.txt").exists());
        assert!(!dir.join("gone.txt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_patch_all_or_none_leaves_disk_untouched_on_bad_hunk() {
        let (ws, dir) = temp_workspace("patch_aon");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"keep\nold\ntail\n").unwrap();
        fs::write(dir.join("b.txt"), b"hello\n").unwrap();

        // Second file's hunk will not match -> whole patch must abort, leaving
        // BOTH files (including the first, which would otherwise have applied)
        // untouched.
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
 keep
-old
+new
 tail
*** Update File: b.txt
@@
-nonexistent line
+replacement
*** End Patch
";
        let out = apply_patch(&json!({ "patch": patch }), &ws, &fs).unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "plan_error");
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"keep\nold\ntail\n");
        assert_eq!(fs::read(dir.join("b.txt")).unwrap(), b"hello\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_patch_parse_error_writes_nothing() {
        let (ws, dir) = temp_workspace("patch_parse");
        let fs = StdFileSystem::new();

        let out = apply_patch(&json!({ "patch": "not a patch" }), &ws, &fs).unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "parse_error");

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- non-UTF-8 safety (P1a) -------------------------------------------

    #[test]
    fn edit_file_refuses_non_utf8_and_leaves_file_unchanged() {
        let (ws, dir) = temp_workspace("edit_non_utf8");
        let fs = StdFileSystem::new();
        // 0xFF/0xFE are not valid UTF-8 lead bytes; the rest is plain ASCII so it
        // sails past the binary heuristic (no NUL, mostly printable).
        let original: &[u8] = b"\xff\xfe valid ascii line\n";
        fs::write(dir.join("cp.txt"), original).unwrap();

        let out = edit_file(
            &json!({ "path": "cp.txt", "oldText": "valid ascii line", "newText": "changed" }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(!out.ok, "non-UTF-8 edit must be refused");
        assert_eq!(out.data["status"], "not_utf8");
        // The file must be byte-for-byte unchanged (no lossy rewrite).
        assert_eq!(fs::read(dir.join("cp.txt")).unwrap(), original);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_file_valid_utf8_still_works() {
        let (ws, dir) = temp_workspace("edit_utf8_ok");
        let fs = StdFileSystem::new();
        // Multi-byte UTF-8 must edit fine.
        fs::write(dir.join("u.txt"), "café\nmore\n".as_bytes()).unwrap();

        let out = edit_file(
            &json!({ "path": "u.txt", "oldText": "more", "newText": "même" }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(out.ok, "{}", out.model_text);
        assert_eq!(fs::read(dir.join("u.txt")).unwrap(), "café\nmême\n".as_bytes());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_patch_aborts_on_non_utf8_touched_file_with_no_writes() {
        let (ws, dir) = temp_workspace("patch_non_utf8");
        let fs = StdFileSystem::new();
        let bad: &[u8] = b"\xff\xfe keep this\n";
        fs::write(dir.join("bad.txt"), bad).unwrap();

        // Update the non-UTF-8 file. The up-front UTF-8 check must abort the whole
        // patch before any write happens.
        let patch = "\
*** Begin Patch
*** Add File: fresh.txt
+brand new
*** Update File: bad.txt
@@
-keep this
+changed
*** End Patch
";
        let out = apply_patch(&json!({ "patch": patch }), &ws, &fs).unwrap();
        assert!(!out.ok, "patch touching a non-UTF-8 file must abort");
        assert_eq!(out.data["status"], "not_utf8");
        // No writes: the bad file is untouched AND the Add target was not created.
        assert_eq!(fs::read(dir.join("bad.txt")).unwrap(), bad);
        assert!(!dir.join("fresh.txt").exists(), "no file should have been written");

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- apply_patch on-disk rollback (P2b) -------------------------------

    /// A [`FileSystem`] decorator over [`StdFileSystem`] that lets the first
    /// `write_atomic` succeed and forces the Nth (1-based) write to fail, so we
    /// can exercise the mid-apply rollback path deterministically.
    struct FailOnNthWrite {
        inner: StdFileSystem,
        writes: std::sync::atomic::AtomicUsize,
        fail_on: usize,
    }

    impl FailOnNthWrite {
        fn new(fail_on: usize) -> Self {
            Self {
                inner: StdFileSystem::new(),
                writes: std::sync::atomic::AtomicUsize::new(0),
                fail_on,
            }
        }
    }

    impl FileSystem for FailOnNthWrite {
        fn read(&self, path: &std::path::Path) -> std::io::Result<Vec<u8>> {
            self.inner.read(path)
        }
        fn write_atomic(&self, path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
            let n = self
                .writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if n == self.fail_on {
                return Err(std::io::Error::other("injected write failure"));
            }
            self.inner.write_atomic(path, bytes)
        }
        fn metadata(&self, path: &std::path::Path) -> std::io::Result<FileMetadata> {
            self.inner.metadata(path)
        }
        fn exists(&self, path: &std::path::Path) -> bool {
            self.inner.exists(path)
        }
        fn rename(&self, from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
            self.inner.rename(from, to)
        }
        fn remove_file(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.inner.remove_file(path)
        }
        fn create_dir_all(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.inner.create_dir_all(path)
        }
    }

    #[test]
    fn apply_patch_rolls_back_first_file_when_second_write_fails() {
        let (ws, dir) = temp_workspace("patch_rollback");
        // Two updates; the plan applies them in op order. Fail the SECOND
        // write_atomic so the first file has already been changed on disk.
        fs::write(dir.join("a.txt"), b"keep\nold\ntail\n").unwrap();
        fs::write(dir.join("b.txt"), b"keep\nbee\ntail\n").unwrap();
        let fs = FailOnNthWrite::new(2);

        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
 keep
-old
+new
 tail
*** Update File: b.txt
@@
 keep
-bee
+wasp
 tail
*** End Patch
";
        let out = apply_patch(&json!({ "patch": patch }), &ws, &fs).unwrap();
        assert!(!out.ok, "a mid-apply write failure must be reported");
        assert_eq!(out.data["status"], "apply_error");
        // The first file must be RESTORED to its original content (rollback),
        // and the second must be unchanged too — disk identical to pre-apply.
        assert_eq!(
            fs::read(dir.join("a.txt")).unwrap(),
            b"keep\nold\ntail\n",
            "first file must be rolled back"
        );
        assert_eq!(fs::read(dir.join("b.txt")).unwrap(), b"keep\nbee\ntail\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_patch_happy_path_applies_all_files() {
        let (ws, dir) = temp_workspace("patch_happy_all");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"keep\nold\ntail\n").unwrap();
        fs::write(dir.join("b.txt"), b"keep\nbee\ntail\n").unwrap();

        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
 keep
-old
+new
 tail
*** Update File: b.txt
@@
 keep
-bee
+wasp
 tail
*** End Patch
";
        let out = apply_patch(&json!({ "patch": patch }), &ws, &fs).unwrap();
        assert!(out.ok, "{}", out.model_text);
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"keep\nnew\ntail\n");
        assert_eq!(fs::read(dir.join("b.txt")).unwrap(), b"keep\nwasp\ntail\n");

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- read_file size ceiling (P2c) -------------------------------------

    #[test]
    fn read_file_caps_oversized_read_and_reports_truncation() {
        // A FileSystem double that would PANIC if asked for the whole file via
        // `read`, proving read_file gates on the capped read instead of slurping.
        struct CappedOnlyFs {
            len: usize,
        }
        impl FileSystem for CappedOnlyFs {
            fn read(&self, _path: &std::path::Path) -> std::io::Result<Vec<u8>> {
                panic!("read_file must use read_capped, not read, for a huge file");
            }
            fn read_capped(
                &self,
                _path: &std::path::Path,
                max: usize,
            ) -> std::io::Result<(Vec<u8>, bool)> {
                // Pretend the file is `self.len` bytes of 'a'; hand back only `max`.
                let give = max.min(self.len);
                Ok((vec![b'a'; give], self.len > max))
            }
            fn write_atomic(&self, _p: &std::path::Path, _b: &[u8]) -> std::io::Result<()> {
                Ok(())
            }
            fn metadata(&self, _p: &std::path::Path) -> std::io::Result<FileMetadata> {
                Ok(FileMetadata {
                    len: self.len as u64,
                    is_dir: false,
                })
            }
            fn exists(&self, _p: &std::path::Path) -> bool {
                true
            }
            fn rename(&self, _f: &std::path::Path, _t: &std::path::Path) -> std::io::Result<()> {
                Ok(())
            }
            fn remove_file(&self, _p: &std::path::Path) -> std::io::Result<()> {
                Ok(())
            }
            fn create_dir_all(&self, _p: &std::path::Path) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (ws, dir) = temp_workspace("read_ceiling");
        // Resolve needs the path to exist on disk for the parent containment
        // check; create a tiny placeholder (the double ignores its content).
        fs::write(dir.join("huge.txt"), b"placeholder").unwrap();
        let fs = CappedOnlyFs {
            len: MAX_READ_FILE_BYTES + 5_000_000,
        };

        let out = read_file(&json!({ "path": "huge.txt" }), &ws, &fs, "tc_cap", None).unwrap();
        assert!(out.ok);
        assert_eq!(out.data["truncated"], true);
        assert_eq!(out.data["bytesTruncated"], true);
        assert!(out.model_text.contains("read ceiling"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_flags_lossy_decode_but_still_shows_text() {
        let (ws, dir) = temp_workspace("read_lossy");
        let fs = StdFileSystem::new();
        // Non-UTF-8 bytes that pass the binary heuristic (no NUL, mostly text).
        fs::write(dir.join("cp.txt"), b"\xff\xfe readable ascii\n").unwrap();

        let out = read_file(&json!({ "path": "cp.txt" }), &ws, &fs, "tc_lossy", None).unwrap();
        assert!(out.ok, "lossy text is still shown");
        assert_eq!(out.data["lossy"], true);
        assert!(out.model_text.contains("readable ascii"));

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- classification ---------------------------------------------------

    #[test]
    fn classify_read_is_allow_write_is_ask() {
        let (ws, dir) = temp_workspace("classify");
        let read = classify(FileTool::Read, &json!({ "path": "a.txt" }), &ws).unwrap();
        assert_eq!(read.action, ToolPermissionAction::Allow);
        let write = classify(
            FileTool::Write,
            &json!({ "path": "a.txt", "content": "x" }),
            &ws,
        )
        .unwrap();
        assert_eq!(write.action, ToolPermissionAction::Ask);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_sensitive_path_is_denied() {
        let (ws, dir) = temp_workspace("classify_sensitive");
        let read = classify(FileTool::Read, &json!({ "path": ".env" }), &ws).unwrap();
        assert_eq!(read.action, ToolPermissionAction::Deny);
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- preview_diff -----------------------------------------------------

    #[test]
    fn preview_diff_for_edit_shows_change_without_writing() {
        let (ws, dir) = temp_workspace("preview_edit");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.rs"), b"let x = 1;\n").unwrap();

        let diff = preview_diff(
            FileTool::Edit,
            &json!({ "path": "a.rs", "oldText": "let x = 1;", "newText": "let x = 2;" }),
            &ws,
            &fs,
        )
        .expect("preview");
        assert!(diff.contains("-let x = 1;"));
        assert!(diff.contains("+let x = 2;"));
        // The file is untouched by the preview.
        assert_eq!(fs::read(dir.join("a.rs")).unwrap(), b"let x = 1;\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_diff_for_read_is_none() {
        let (ws, dir) = temp_workspace("preview_read");
        let fs = StdFileSystem::new();
        assert!(preview_diff(FileTool::Read, &json!({ "path": "a.txt" }), &ws, &fs).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_diff_for_apply_patch_lists_files_without_writing() {
        let (ws, dir) = temp_workspace("preview_patch");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"keep\nold\ntail\n").unwrap();
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
 keep
-old
+new
 tail
*** End Patch
";
        let diff = preview_diff(FileTool::ApplyPatch, &json!({ "patch": patch }), &ws, &fs)
            .expect("preview");
        assert!(diff.contains("update a.txt"));
        // Untouched.
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"keep\nold\ntail\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_outside_workspace_is_denied() {
        let (ws, dir) = temp_workspace("classify_outside");
        let read = classify(
            FileTool::Read,
            &json!({ "path": "../../etc/passwd" }),
            &ws,
        )
        .unwrap();
        assert_eq!(read.action, ToolPermissionAction::Deny);
        let _ = fs::remove_dir_all(&dir);
    }
}
