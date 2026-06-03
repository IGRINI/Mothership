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

use std::path::{Path, PathBuf};

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

/// Default policy ceiling on `write_file` content size. A policy-level refusal
/// (NOT a JSON-schema cap): a legit large write up to this limit still works,
/// while a runaway one is rejected with no side effect. The composition root can
/// override it via [`write_file_with_limit`].
pub const DEFAULT_MAX_WRITE_FILE_BYTES: usize = 10 * 1024 * 1024;

/// Cap on the diff / synthesized result text that a mutating tool puts into the
/// persisted execution event and `ToolExecutionResult` (UI/DB). The model-facing
/// response has its own, larger budget; this only bounds what is stored/streamed
/// so a huge overwrite cannot push megabytes into the event store.
pub const MAX_TOOL_EVENT_BYTES: usize = 64 * 1024;

/// Upper bound on the combined old+new size a `write_file` will render a full
/// line diff for. Beyond this we emit a concise summary instead of building a
/// whole-content diff in memory — so a large NEW content (a new big file, or a
/// small file replaced by huge content) is bounded the same way a large OLD file
/// already is. The displayed diff is separately capped to `MAX_TOOL_EVENT_BYTES`
/// downstream, so a full diff past this size would be discarded anyway.
const MAX_DIFF_INPUT_BYTES: usize = 1024 * 1024;

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
    pub(super) fn failure(model_text: impl Into<String>, data: Value) -> Self {
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

/// The file tools, identified by name, used for classification. Includes the two
/// read-only search tools (`list_files`/`search_text`), which share the same
/// classify/dispatch plumbing but always resolve to a read `Allow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTool {
    Read,
    Write,
    Edit,
    ApplyPatch,
    ListFiles,
    SearchText,
}

impl FileTool {
    /// Map a tool name to a [`FileTool`], or `None` if it is not a file tool.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            super::catalog::READ_FILE_TOOL_NAME => Some(FileTool::Read),
            super::catalog::WRITE_FILE_TOOL_NAME => Some(FileTool::Write),
            super::catalog::EDIT_FILE_TOOL_NAME => Some(FileTool::Edit),
            super::catalog::APPLY_PATCH_TOOL_NAME => Some(FileTool::ApplyPatch),
            super::catalog::LIST_FILES_TOOL_NAME => Some(FileTool::ListFiles),
            super::catalog::SEARCH_TEXT_TOOL_NAME => Some(FileTool::SearchText),
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
            // Only the path is needed to classify a write; extract it BORROWED so
            // an oversized `content` is never cloned just to decide permissions
            // (`parse_args` clones the whole argument object, content included).
            let path = arg_str(arguments, "path")?;
            let touched = vec![path.to_string()];
            match guard_paths(workspace, &[path]) {
                Ok(()) => Ok(FileToolCapability {
                    action: ToolPermissionAction::Ask,
                    summary: format!("write {path}"),
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
        // Read-only search tools: classification (read `Allow` within the
        // workspace, `Deny` for an escaping/sensitive `dir`) lives in the search
        // module alongside the handlers.
        FileTool::ListFiles => super::search::classify_list_files(arguments, workspace),
        FileTool::SearchText => super::search::classify_search_text(arguments, workspace),
    }
}

/// Shallow, borrowed schema preflight for malformed calls that should fail before
/// an approval prompt. This does not replace full typed parsing in handlers; it
/// only rejects obvious invalid `write_file` calls without cloning `content`.
pub fn validate_args_shallow(tool: FileTool, arguments: &Value) -> Result<(), FileToolError> {
    if tool != FileTool::Write {
        return Ok(());
    }

    arg_str(arguments, "path")?;
    arg_str(arguments, "content")?;
    optional_bool(arguments, "create")?;
    optional_bool(arguments, "overwrite")?;
    optional_string_any(arguments, &["expectedSha256", "expected_sha256"])?;
    Ok(())
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
    max_write_bytes: usize,
) -> Option<String> {
    match tool {
        // Read-only tools have nothing to preview before approval.
        FileTool::Read | FileTool::ListFiles | FileTool::SearchText => None,
        FileTool::Write => {
            // Limit-aware preview. Check the raw content size FIRST (a borrow, no
            // clone): an oversized write is refused at execution, so never parse,
            // clone, or diff it here — that would blow memory and bypass the
            // bounded-diff gate. Summarize instead.
            let content_len = arguments
                .get("content")
                .and_then(Value::as_str)
                .map(str::len)
                .unwrap_or(0);
            if content_len > max_write_bytes {
                return Some(format!(
                    "# write refused before approval: content is {content_len} bytes, over the {max_write_bytes}-byte write limit"
                ));
            }
            let input: WriteFileInput = parse_args(arguments).ok()?;
            let resolved = resolve_guarded(workspace, &input.path).ok()?;
            // Cap the old-file read: previewing a write over a huge existing file
            // must not load the whole file just to render a diff. The preview is
            // bounded again downstream before it reaches the event/UI.
            let old = if fs.exists(&resolved) {
                fs.read_capped(&resolved, MAX_READ_FILE_BYTES)
                    .ok()
                    .map(|(bytes, _truncated)| String::from_utf8_lossy(&bytes).into_owned())
            } else {
                None
            };
            let new_text = match &old {
                Some(old) => match_bom_and_eol(old, &input.content),
                None => input.content.clone(),
            };
            // Mirror the handler's bounded-diff gate: don't build a full line diff
            // for a large write (it would be summarized at write time anyway).
            let diff_input = old.as_deref().map(str::len).unwrap_or(0) + new_text.len();
            if diff_input > MAX_DIFF_INPUT_BYTES {
                return Some(format!(
                    "# write {} (~{} bytes; diff too large to preview)",
                    input.path,
                    new_text.len()
                ));
            }
            Some(render_full_diff(old.as_deref().unwrap_or(""), &new_text))
        }
        FileTool::Edit => {
            let input: EditFileInput = parse_args(arguments).ok()?;
            let resolved = resolve_guarded(workspace, &input.path).ok()?;
            // Cap the read for the same reason as the Write branch: a preview must
            // not slurp a huge file. `apply_edit` only matches `old_text` for the
            // diff, so a capped prefix is sufficient for the approval card.
            let (bytes, _truncated) = fs.read_capped(&resolved, MAX_READ_FILE_BYTES).ok()?;
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

    // Gate on size first: the RAW read off disk is always hard-bounded to
    // MAX_READ_FILE_BYTES so a huge file is never fully loaded — the caller's
    // `maxBytes` must NOT be able to widen this raw read (otherwise a request for
    // 500 MB would slurp 500 MB into memory). The model's `maxBytes` only governs
    // how much of the read content is rendered inline (computed below, clamped
    // down to the same ceiling). The sha is computed over the bytes actually
    // read; when the read was capped that is a prefix hash, which we surface via
    // `bytesTruncated`.
    let read_ceiling = MAX_READ_FILE_BYTES;
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
    // The model's requested inline cap, clamped DOWN to the hard ceiling: a model
    // cannot ask to inline more than the raw read could ever produce. Content
    // beyond this still spills via `logRef` (read up to the ceiling, render up to
    // `max_bytes`, spill the rest).
    let max_bytes = input
        .max_bytes
        .unwrap_or(DEFAULT_READ_MAX_BYTES)
        .min(MAX_READ_FILE_BYTES);

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
        // sha and line view describe that prefix, not the whole file. Annotate the
        // data so `totalLines` is not mistaken for the whole-file total: the lines
        // counted here are only those in the read prefix, and the read was capped
        // at `read_ceiling` bytes. Content past the cap is NOT reachable via
        // startLine/limit here — those page WITHIN this prefix only.
        data["bytesTruncated"] = json!(true);
        data["truncated"] = json!(true);
        data["readCeilingBytes"] = json!(read_ceiling);
        data["cappedAtBytes"] = json!(read_ceiling);
        data["linesAreFromCappedPrefix"] = json!(true);
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
            model_text.push_str(&capped_window_note(read_ceiling));
        }
        model_text.push_str(&format!("(total lines in read window: {total_lines})\n"));
        return Ok(FileToolOutcome {
            ok: true,
            model_text,
            data,
            sha256: Some(sha),
            diff: None,
        });
    }

    // The rendered view fits the byte budget, but the file itself may have been
    // size-capped off disk. Tell the model the truth: startLine/limit page WITHIN
    // this read window only — they do NOT reach content past the cap. For ranges
    // beyond the cap in very large files, run_command is the tool.
    let mut model_text = numbered;
    if bytes_truncated {
        model_text.push_str(&capped_window_note(read_ceiling));

        // If the requested start fell at/after the last line available in the
        // (capped) window, the model asked for content that is not in the read
        // window. Say so plainly rather than returning a silent empty body — and
        // do not pretend the lines exist past the cap.
        if start_idx >= total_lines {
            data["startBeyondWindow"] = json!(true);
            model_text.push_str(&format!(
                "(requested startLine {start_line} is beyond the {total_lines} line(s) in this read window; lines past the {read_ceiling}-byte cap are not readable here — use run_command for ranges in very large files)\n"
            ));
        }
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

/// Run `write_file`. Creates or fully overwrites a file. Honors `create` and
/// requires a content precondition (`expectedSha256` or a fresh full-file
/// observation) before replacing an existing file; preserves a leading BOM and
/// the dominant line ending of an existing file; writes atomically.
pub fn write_file(
    arguments: &Value,
    workspace: &Workspace,
    fs: &dyn FileSystem,
) -> Result<FileToolOutcome, FileToolError> {
    write_file_with_limit(arguments, workspace, fs, DEFAULT_MAX_WRITE_FILE_BYTES)
}

/// Like [`write_file`] but with an explicit policy ceiling on the content size.
/// `write_file` uses [`DEFAULT_MAX_WRITE_FILE_BYTES`]; the composition root can
/// pass a configured `max_write_bytes` to override it.
pub fn write_file_with_limit(
    arguments: &Value,
    workspace: &Workspace,
    fs: &dyn FileSystem,
    max_write_bytes: usize,
) -> Result<FileToolOutcome, FileToolError> {
    write_file_with_limit_and_observation(arguments, workspace, fs, max_write_bytes, None)
}

/// Like [`write_file_with_limit`], but accepts a fresh full-file observation
/// from the current run as a content precondition for replacing an existing
/// file. This makes blind overwrites impossible even when approval is bypassed:
/// an existing file must match either `expectedSha256` from the call or the
/// SHA-256 captured by a prior complete `read_file`.
pub fn write_file_with_limit_and_observation(
    arguments: &Value,
    workspace: &Workspace,
    fs: &dyn FileSystem,
    max_write_bytes: usize,
    observed_sha256: Option<&str>,
) -> Result<FileToolOutcome, FileToolError> {
    // Policy ceiling, checked on BORROWED fields BEFORE `parse_args` clones the
    // arguments — an oversized write is refused without ever cloning its content.
    let path = arg_str(arguments, "path")?;
    let content_bytes = arg_str(arguments, "content")?.len();
    if content_bytes > max_write_bytes {
        return Ok(FileToolOutcome::failure(
            format!(
                "refused to write `{path}`: content is {content_bytes} bytes, over the {max_write_bytes}-byte write limit"
            ),
            json!({
                "path": path,
                "status": "too_large",
                "contentBytes": content_bytes,
                "maxWriteBytes": max_write_bytes,
            }),
        ));
    }

    let input: WriteFileInput = parse_args(arguments)?;
    let resolved = match resolve_guarded(workspace, &input.path) {
        Ok(path) => path,
        Err(reason) => return Ok(path_failure(&input.path, reason)),
    };

    let existed = fs.exists(&resolved);
    let create = input.create.unwrap_or(true);

    if !existed && !create {
        return Ok(FileToolOutcome::failure(
            format!("`{}` does not exist and create was not requested", input.path),
            json!({ "path": input.path, "status": "missing" }),
        ));
    }

    // For an existing file, hash the WHOLE file by streaming it through the
    // hasher (bounded memory) rather than slurping it into a `Vec` — a write over
    // a multi-gigabyte file must not load it just to compute a conflict sha. The
    // BOM/EOL base + diff are derived separately from a bounded prefix below.
    let old_sha = if existed {
        Some(
            fs.hash_file_sha256(&resolved)
                .map_err(|error| FileToolError::Io(error.to_string()))?,
        )
    } else {
        None
    };

    if existed {
        let old_sha = old_sha.as_deref().unwrap_or_default();
        if let Some(expected) = &input.expected_sha256 {
            if !expected.eq_ignore_ascii_case(old_sha) {
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
        } else if let Some(observed) = observed_sha256 {
            if !observed.eq_ignore_ascii_case(old_sha) {
                return Ok(write_precondition_failure(&input.path, old_sha, Some(observed)));
            }
        } else {
            return Ok(write_precondition_failure(&input.path, old_sha, None));
        }
    }

    // Read a BOUNDED prefix of the existing file for BOM/EOL detection and the
    // diff base. A huge old file is never fully loaded; `old_truncated` records
    // whether the prefix is the whole file (false) or just its leading bytes
    // (true). When truncated, the prefix is NOT a faithful diff base, so we emit
    // a summary instead of a misleading partial line diff below. `old_size` is
    // captured here, BEFORE the overwrite, so the summary can report the old
    // length (stat fails -> `>cap`).
    let (old_text, old_truncated, old_size) = if existed {
        let (bytes, truncated) = fs
            .read_capped(&resolved, MAX_READ_FILE_BYTES)
            .map_err(|error| FileToolError::Io(error.to_string()))?;
        let size = fs
            .metadata(&resolved)
            .map(|meta| meta.len.to_string())
            .unwrap_or_else(|_| ">cap".to_string());
        (
            Some(String::from_utf8_lossy(&bytes).into_owned()),
            truncated,
            Some(size),
        )
    } else {
        (None, false, None)
    };

    // Preserve BOM + EOL style of the existing file so a full overwrite does not
    // silently reformat line endings. (Derived from the bounded prefix, which is
    // sufficient: a file's leading bytes carry its BOM and dominant EOL style.)
    let final_text = match &old_text {
        Some(old) => match_bom_and_eol(old, &input.content),
        None => input.content.clone(),
    };
    let final_bytes = final_text.as_bytes();

    fs.write_atomic(&resolved, final_bytes)
        .map_err(|error| FileToolError::Io(error.to_string()))?;

    let new_sha = sha256_hex(final_bytes);
    // Diff: build a faithful full line diff only when BOTH sides are small enough
    // to diff in memory. We summarize instead when either (a) the old file
    // exceeded the read cap (the prefix is not a faithful base), or (b) the
    // combined old+new size exceeds MAX_DIFF_INPUT_BYTES — e.g. creating a large
    // NEW file or replacing a small file with very large content. Otherwise a huge
    // `content` would be split line-by-line into a diff in memory even though the
    // stored/streamed diff is capped downstream anyway.
    let old_for_diff = old_text.as_deref().unwrap_or("");
    let diff_input_bytes = old_for_diff.len().saturating_add(final_text.len());
    let diff = if old_truncated || diff_input_bytes > MAX_DIFF_INPUT_BYTES {
        let old_part = if existed {
            let old_size = old_size.as_deref().unwrap_or(">cap");
            let old_sha_text = old_sha.as_deref().unwrap_or("");
            format!("old {old_size} bytes (sha {old_sha_text})")
        } else {
            "new file".to_string()
        };
        format!(
            "[large write — full line diff omitted; {old_part} -> {} bytes (sha {new_sha})]",
            final_bytes.len()
        )
    } else {
        render_full_diff(old_for_diff, &final_text)
    };
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

/// Validate the write content precondition without cloning `content` or writing.
/// Returns `Some(outcome)` when the call should be refused before approval /
/// execution, and `None` when the write may continue. The actual handler repeats
/// the same check after approval so a file changed between preview and execution
/// still fails closed.
pub fn check_write_content_precondition(
    arguments: &Value,
    workspace: &Workspace,
    fs: &dyn FileSystem,
    observed_sha256: Option<&str>,
) -> Result<Option<FileToolOutcome>, FileToolError> {
    validate_args_shallow(FileTool::Write, arguments)?;
    let path = arg_str(arguments, "path")?;
    let expected = optional_string_value_any(arguments, &["expectedSha256", "expected_sha256"])?;
    let resolved = match resolve_guarded(workspace, path) {
        Ok(path) => path,
        Err(reason) => return Ok(Some(path_failure(path, reason))),
    };
    if !fs.exists(&resolved) || expected.is_some() {
        return Ok(None);
    }

    let current_sha = fs
        .hash_file_sha256(&resolved)
        .map_err(|error| FileToolError::Io(error.to_string()))?;
    match observed_sha256 {
        Some(observed) if observed.eq_ignore_ascii_case(&current_sha) => Ok(None),
        observed => Ok(Some(write_precondition_failure(
            path,
            &current_sha,
            observed,
        ))),
    }
}

fn write_precondition_failure(
    path: &str,
    actual_sha256: &str,
    observed_sha256: Option<&str>,
) -> FileToolOutcome {
    match observed_sha256 {
        Some(observed) => FileToolOutcome::failure(
            format!(
                "`{path}` changed since it was read (observed sha256 {observed}, found {actual_sha256}); not written"
            ),
            json!({
                "path": path,
                "status": "stale_observation",
                "observedSha256": observed,
                "actualSha256": actual_sha256,
                "required": "expectedSha256 or a fresh complete read_file observation",
            }),
        ),
        None => FileToolOutcome::failure(
            format!(
                "`{path}` already exists; refusing blind overwrite. Read the full file first or pass expectedSha256."
            ),
            json!({
                "path": path,
                "status": "precondition_required",
                "sha256": actual_sha256,
                "required": "expectedSha256 or a fresh complete read_file observation",
            }),
        ),
    }
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

    // Refuse to edit files larger than the read ceiling. Unlike read_file (prefix)
    // and write_file (stream-hash + capped prefix), edit_file must load the WHOLE
    // file to apply a content-addressed replacement, so an unbounded read here would
    // reintroduce the memory problem. Check the size BEFORE reading and point at
    // targeted alternatives. (If metadata is unavailable we fall through and let the
    // read proceed.)
    if let Ok(meta) = fs.metadata(&resolved) {
        if meta.len > MAX_READ_FILE_BYTES as u64 {
            return Ok(FileToolOutcome::failure(
                format!(
                    "`{}` is {} bytes, larger than the {}-byte edit ceiling; use apply_patch for a targeted change, or run_command for very large files",
                    input.path, meta.len, MAX_READ_FILE_BYTES
                ),
                json!({
                    "path": input.path,
                    "status": "too_large",
                    "size": meta.len,
                    "editCeilingBytes": MAX_READ_FILE_BYTES,
                }),
            ));
        }
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

/// The prior on-disk state needed to roll a partially-applied patch back: the
/// per-file snapshots plus the directories that did NOT exist before the apply
/// but will be created by it (so a rollback can remove the orphan dirs the write
/// path's `create_dir_all` left behind).
struct PatchSnapshot {
    files: Vec<PathSnapshot>,
    /// Workspace directories that did not exist pre-apply and would be created by
    /// writing the plan's files. Deduped; removed deepest-first on rollback.
    created_dirs: Vec<PathBuf>,
}

/// Capture the prior state of every path a plan will write, move, or delete
/// (including a move's destination), so the apply step can be rolled back to a
/// clean state on any mid-apply failure. Reads through the [`FileSystem`] port.
/// Also records the directories the apply would create (ancestors of each
/// written/created/move-destination path that do not yet exist, up to but not
/// including the workspace root) so rollback can remove the orphans.
fn capture_snapshot(
    workspace: &Workspace,
    fs: &dyn FileSystem,
    plan: &PatchPlan,
) -> Result<PatchSnapshot, FileToolError> {
    let mut paths: Vec<PathBuf> = Vec::new();
    // Destinations whose parent directories may need to be created: every Add /
    // Update target and every Move destination. (A Delete only removes a file and
    // never creates a directory.)
    let mut write_targets: Vec<PathBuf> = Vec::new();
    for file in &plan.files {
        let source = resolve_for_apply(workspace, &file.path)?;
        if matches!(file.op, PlannedKind::Add | PlannedKind::Update) && !write_targets.contains(&source) {
            write_targets.push(source.clone());
        }
        if !paths.contains(&source) {
            paths.push(source);
        }
        if let PlannedKind::Move { to } = &file.op {
            let dest = resolve_for_apply(workspace, to)?;
            if !write_targets.contains(&dest) {
                write_targets.push(dest.clone());
            }
            if !paths.contains(&dest) {
                paths.push(dest);
            }
        }
    }

    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let prior = if fs.exists(&path) {
            Some(
                fs.read(&path)
                    .map_err(|error| FileToolError::Io(error.to_string()))?,
            )
        } else {
            None
        };
        files.push(PathSnapshot { path, prior });
    }

    let created_dirs = collect_created_dirs(workspace, fs, &write_targets);
    Ok(PatchSnapshot { files, created_dirs })
}

/// Compute the set of directories that do not currently exist but will be created
/// by writing `write_targets`. For each target, walk its ancestor directories up
/// the tree, stopping BEFORE the workspace root (the root and any pre-existing
/// directory are never recorded). Deduped; order is irrelevant here because the
/// caller removes them deepest-first.
fn collect_created_dirs(
    workspace: &Workspace,
    fs: &dyn FileSystem,
    write_targets: &[PathBuf],
) -> Vec<PathBuf> {
    let root = workspace.root();
    let mut created: Vec<PathBuf> = Vec::new();
    for target in write_targets {
        // Ancestors of the file: its parent dir and upward. `ancestors()` yields
        // the path itself first, so skip it (we want directories, not the file).
        for ancestor in target.ancestors().skip(1) {
            // Stop once we reach the workspace root or climb to/above it: the root
            // and everything outside it must never be removed.
            if ancestor == root || !path_below_root(root, ancestor) {
                break;
            }
            if fs.exists(ancestor) {
                // This dir (and therefore all of its ancestors) already exists;
                // nothing above it can be newly created either.
                break;
            }
            let dir = ancestor.to_path_buf();
            if !created.contains(&dir) {
                created.push(dir);
            }
        }
    }
    created
}

/// Whether `candidate` is strictly inside `root` (a descendant, not the root
/// itself). Used to stop the ancestor walk at the workspace boundary.
fn path_below_root(root: &Path, candidate: &Path) -> bool {
    candidate != root && candidate.starts_with(root)
}

/// Restore the captured pre-apply state: first rewrite/delete files, then remove
/// any directories the apply newly created (deepest-first). File restoration is
/// best-effort (the first error is remembered and returned after attempting the
/// rest, so one stuck path does not strand the others). Directory removal ignores
/// errors entirely — a dir left non-empty by something else stays, which is the
/// safe outcome.
fn restore_snapshot(fs: &dyn FileSystem, snapshot: &PatchSnapshot) -> Result<(), FileToolError> {
    let mut first_error: Option<FileToolError> = None;
    for entry in &snapshot.files {
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

    // Remove newly-created directories deepest-first (longest path first) so a
    // child is gone before its parent is attempted. `remove_dir` only deletes an
    // EMPTY dir; errors (including a dir left non-empty by an unrelated file) are
    // ignored so rollback never blows away pre-existing content.
    let mut dirs: Vec<&PathBuf> = snapshot.created_dirs.iter().collect();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.as_os_str().len()));
    for dir in dirs {
        let _ = fs.remove_dir(dir);
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

/// Borrow a required string field from tool arguments without cloning the whole
/// argument object — so a permission check or size gate never has to clone a
/// (potentially huge) sibling field such as `content` just to read a `path`.
fn arg_str<'a>(arguments: &'a Value, field: &str) -> Result<&'a str, FileToolError> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| FileToolError::InvalidArguments(format!("missing or non-string `{field}`")))
}

fn optional_bool(arguments: &Value, field: &str) -> Result<(), FileToolError> {
    match arguments.get(field) {
        Some(value) if !value.is_boolean() => Err(FileToolError::InvalidArguments(format!(
            "`{field}` must be a boolean"
        ))),
        _ => Ok(()),
    }
}

fn optional_string_any(arguments: &Value, fields: &[&str]) -> Result<(), FileToolError> {
    for field in fields {
        if let Some(value) = arguments.get(*field) {
            if !value.is_string() {
                return Err(FileToolError::InvalidArguments(format!(
                    "`{field}` must be a string"
                )));
            }
        }
    }
    Ok(())
}

fn optional_string_value_any<'a>(
    arguments: &'a Value,
    fields: &[&str],
) -> Result<Option<&'a str>, FileToolError> {
    for field in fields {
        if let Some(value) = arguments.get(*field) {
            return value.as_str().map(Some).ok_or_else(|| {
                FileToolError::InvalidArguments(format!("`{field}` must be a string"))
            });
        }
    }
    Ok(None)
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

/// The honest truncation note appended when the raw read hit `read_ceiling`.
/// Crucially it does NOT claim `startLine`/`limit` can page past the cap (they
/// only page within the bytes that were read); it points the model at
/// `run_command` for targeted ranges in a file larger than the ceiling.
fn capped_window_note(read_ceiling: usize) -> String {
    format!(
        "\n... file exceeds the {read_ceiling}-byte read ceiling; only the leading bytes were read. \
startLine/limit page within this window only — for content beyond the cap use run_command \
(e.g. `sed -n 'A,Bp' <file>` or PowerShell `Get-Content <file> -TotalCount N`) to read targeted ranges ...\n"
    )
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
    fn write_file_refuses_oversized_content_without_writing() {
        let (ws, dir) = temp_workspace("write_too_large");
        let fs = StdFileSystem::new();
        let big = "x".repeat(2048);

        // A 1 KiB policy limit refuses 2 KiB of content, with no file written.
        let out =
            write_file_with_limit(&json!({ "path": "big.txt", "content": big }), &ws, &fs, 1024)
                .unwrap();

        assert!(!out.ok, "oversized write must be refused");
        assert_eq!(out.data["status"], "too_large");
        assert_eq!(out.data["contentBytes"], 2048);
        assert_eq!(out.data["maxWriteBytes"], 1024);
        assert!(
            !dir.join("big.txt").exists(),
            "no file may be written when the write is refused"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_refuses_existing_without_content_precondition() {
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
        assert_eq!(out.data["status"], "precondition_required");
        assert_eq!(out.data["required"], "expectedSha256 or a fresh complete read_file observation");
        // File unchanged.
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"old\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_overwrite_flag_does_not_bypass_content_precondition() {
        let (ws, dir) = temp_workspace("write_over");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"old\n").unwrap();

        let out = write_file(
            &json!({ "path": "a.txt", "content": "new\n", "overwrite": true }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "precondition_required");
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"old\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_existing_allows_matching_observation() {
        let (ws, dir) = temp_workspace("write_observed");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"old\n").unwrap();
        let observed = sha_of("old\n");

        let out = write_file_with_limit_and_observation(
            &json!({ "path": "a.txt", "content": "new\n" }),
            &ws,
            &fs,
            DEFAULT_MAX_WRITE_FILE_BYTES,
            Some(&observed),
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.data["status"], "modified");
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"new\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_existing_rejects_stale_observation() {
        let (ws, dir) = temp_workspace("write_stale_observed");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"old\n").unwrap();

        let out = write_file_with_limit_and_observation(
            &json!({ "path": "a.txt", "content": "new\n" }),
            &ws,
            &fs,
            DEFAULT_MAX_WRITE_FILE_BYTES,
            Some("deadbeef"),
        )
        .unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "stale_observation");
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"old\n");

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
        let expected = sha_of("\u{FEFF}one\r\ntwo\r\n");

        let out = write_file(
            &json!({ "path": "a.txt", "content": "alpha\nbeta\n", "expectedSha256": expected }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(out.ok);
        let written = fs::read(dir.join("a.txt")).unwrap();
        assert_eq!(written, "\u{FEFF}alpha\r\nbeta\r\n".as_bytes());

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- write_file bounded old-file read (round-3 Fix B) ----------------

    /// A FileSystem double backed by an in-memory `old` buffer that PANICS if
    /// `read` (the full slurp) is ever called. `read_capped` returns a bounded
    /// prefix and `hash_file_sha256` streams the buffer through a real hasher.
    /// Used to prove `write_file` no longer slurps the whole old file: it must use
    /// `read_capped` + `hash_file_sha256` only. `write_atomic` records the bytes
    /// written so the test can verify the new content.
    struct NoFullReadFs {
        old: Vec<u8>,
        prefix_cap: usize,
        written: std::sync::Mutex<Option<Vec<u8>>>,
    }
    impl NoFullReadFs {
        fn new(old: Vec<u8>, prefix_cap: usize) -> Self {
            Self {
                old,
                prefix_cap,
                written: std::sync::Mutex::new(None),
            }
        }
    }
    impl FileSystem for NoFullReadFs {
        fn read(&self, _p: &std::path::Path) -> std::io::Result<Vec<u8>> {
            panic!("write_file must not slurp the whole old file via read()");
        }
        fn read_capped(
            &self,
            _p: &std::path::Path,
            max: usize,
        ) -> std::io::Result<(Vec<u8>, bool)> {
            // Hand back at most `min(max, prefix_cap)` bytes; report truncation
            // whenever the real file is longer than what we return.
            let give = max.min(self.prefix_cap).min(self.old.len());
            let truncated = self.old.len() > give;
            Ok((self.old[..give].to_vec(), truncated))
        }
        fn hash_file_sha256(&self, _p: &std::path::Path) -> std::io::Result<String> {
            // Real, full-file hash over the stored buffer (delegating to the same
            // hex sha used by the production path).
            Ok(sha256_hex(&self.old))
        }
        fn write_atomic(&self, _p: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
            *self.written.lock().unwrap() = Some(bytes.to_vec());
            Ok(())
        }
        fn metadata(&self, _p: &std::path::Path) -> std::io::Result<FileMetadata> {
            Ok(FileMetadata { len: self.old.len() as u64, is_dir: false })
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
        fn remove_dir(&self, _p: &std::path::Path) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_file_overwrite_does_not_slurp_old_file_and_summarizes_large_diff() {
        let (ws, dir) = temp_workspace("write_no_slurp");
        fs::write(dir.join("big.txt"), b"placeholder").unwrap();
        // A "large" old file: bigger than our small prefix cap so the diff base is
        // truncated and the summary path is taken. `read()` panicking proves
        // write_file never slurps it.
        let old = vec![b'x'; 4096];
        let observed = sha256_hex(&old);
        let fs = NoFullReadFs::new(old, 256);

        let out = write_file_with_limit_and_observation(
            &json!({ "path": "big.txt", "content": "brand new content\n" }),
            &ws,
            &fs,
            DEFAULT_MAX_WRITE_FILE_BYTES,
            Some(&observed),
        )
        .unwrap();
        assert!(out.ok, "{}", out.model_text);
        assert_eq!(out.data["status"], "modified");
        // The new bytes were written.
        assert_eq!(
            fs.written.lock().unwrap().clone().unwrap(),
            b"brand new content\n"
        );
        // The diff is the concise summary (NOT a full line diff over the prefix).
        let diff = out.diff.as_deref().unwrap();
        assert!(
            diff.contains("full line diff omitted"),
            "large overwrite must emit a summary diff, got: {diff}"
        );
        // Old-file-large summary reports the old size (the prefix was truncated).
        assert!(diff.contains("large write") && diff.contains("old "), "got: {diff}");
        // It must NOT contain a hunk header (that would be the full diff path).
        assert!(!diff.contains("@@ -1,"), "summary diff must not be a line diff: {diff}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_conflict_detected_via_streamed_hash() {
        let (ws, dir) = temp_workspace("write_streamed_sha");
        fs::write(dir.join("f.txt"), b"placeholder").unwrap();
        let old = b"the original contents\n".to_vec();
        let correct = sha256_hex(&old);

        // Wrong expectedSha256 -> conflict, detected via the streamed hash, with no
        // write performed.
        let fs = NoFullReadFs::new(old.clone(), 8);
        let out = write_file(
            &json!({
                "path": "f.txt",
                "content": "replacement\n",
                "expectedSha256": "00ff",
            }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "conflict");
        assert_eq!(out.data["actualSha256"], correct);
        assert!(fs.written.lock().unwrap().is_none(), "conflict must not write");

        // Correct expectedSha256 -> write proceeds.
        let fs = NoFullReadFs::new(old, 8);
        let out = write_file(
            &json!({
                "path": "f.txt",
                "content": "replacement\n",
                "expectedSha256": correct,
            }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(out.ok, "{}", out.model_text);
        assert_eq!(fs.written.lock().unwrap().clone().unwrap(), b"replacement\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_small_overwrite_still_produces_full_diff() {
        let (ws, dir) = temp_workspace("write_small_diff");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"old\n").unwrap();
        let expected = sha_of("old\n");

        let out = write_file(
            &json!({ "path": "a.txt", "content": "new\n", "expectedSha256": expected }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(out.ok);
        let diff = out.diff.as_deref().unwrap();
        // Small (sub-cap) overwrite keeps the real line diff.
        assert!(diff.contains("@@ -1,"), "small overwrite should keep a full diff: {diff}");
        assert!(diff.contains("-old"));
        assert!(diff.contains("+new"));
        assert!(!diff.contains("full line diff omitted"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_large_new_content_summarizes_diff() {
        // Creating a NEW file whose content exceeds MAX_DIFF_INPUT_BYTES must NOT
        // build a full line diff in memory (old_truncated is false here) — it gets
        // a concise summary instead, while the file is still written in full.
        let (ws, dir) = temp_workspace("write_large_new");
        let fs = StdFileSystem::new();
        let big = "a".repeat(MAX_DIFF_INPUT_BYTES + 1024);

        let out = write_file(&json!({ "path": "big.txt", "content": big.clone() }), &ws, &fs)
            .unwrap();
        assert!(out.ok);
        let diff = out.diff.as_deref().unwrap();
        assert!(
            diff.contains("full line diff omitted") && diff.contains("new file"),
            "large new content should summarize, not full-diff: {diff}"
        );
        assert!(
            !diff.contains("@@"),
            "large new content must not build a full line diff"
        );
        // The file is still written in full.
        assert_eq!(fs::read(dir.join("big.txt")).unwrap().len(), big.len());

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- edit_file --------------------------------------------------------

    #[test]
    fn edit_file_refuses_files_over_the_read_cap() {
        // edit_file must load the WHOLE file to apply a content-addressed edit, so
        // it refuses files larger than the read ceiling (mirroring read_file) rather
        // than slurping them. A double whose `read`/`read_capped` panic proves the
        // guard returns BEFORE any read.
        struct OversizeFs {
            len: u64,
        }
        impl FileSystem for OversizeFs {
            fn read(&self, _p: &std::path::Path) -> std::io::Result<Vec<u8>> {
                panic!("edit_file must refuse an oversize file before reading it");
            }
            fn read_capped(
                &self,
                _p: &std::path::Path,
                _m: usize,
            ) -> std::io::Result<(Vec<u8>, bool)> {
                panic!("edit_file must not read an oversize file at all");
            }
            fn write_atomic(&self, _p: &std::path::Path, _b: &[u8]) -> std::io::Result<()> {
                Ok(())
            }
            fn metadata(&self, _p: &std::path::Path) -> std::io::Result<FileMetadata> {
                Ok(FileMetadata {
                    len: self.len,
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
            fn remove_dir(&self, _p: &std::path::Path) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (ws, dir) = temp_workspace("edit_oversize");
        // resolve_guarded canonicalizes against the real fs, so the path must exist
        // on disk; the OversizeFs double supplies the (pretend) size.
        fs::write(dir.join("big.rs"), b"placeholder").unwrap();
        let fs = OversizeFs {
            len: MAX_READ_FILE_BYTES as u64 + 1,
        };

        let out = edit_file(
            &json!({ "path": "big.rs", "oldText": "a", "newText": "b" }),
            &ws,
            &fs,
        )
        .unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "too_large");
        assert!(
            out.model_text.contains("apply_patch") && out.model_text.contains("run_command"),
            "should point at targeted alternatives: {}",
            out.model_text
        );

        let _ = fs::remove_dir_all(&dir);
    }

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
        fn remove_dir(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.inner.remove_dir(path)
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
    fn apply_patch_rollback_removes_newly_created_directories() {
        let (ws, dir) = temp_workspace("patch_rollback_dirs");
        // First Add creates `newdir/sub/file.txt` (write #1, succeeds and makes
        // the two nested directories). The second Add's write (#2) is forced to
        // fail, triggering rollback. After rollback the created file AND the two
        // directories it brought into being must be gone — disk shape == before.
        let fs = FailOnNthWrite::new(2);

        let patch = "\
*** Begin Patch
*** Add File: newdir/sub/file.txt
+hello
*** Add File: other.txt
+world
*** End Patch
";
        let out = apply_patch(&json!({ "patch": patch }), &ws, &fs).unwrap();
        assert!(!out.ok, "a mid-apply write failure must be reported");
        assert_eq!(out.data["status"], "apply_error");

        // The created file is gone, and so are the orphan directories.
        assert!(!dir.join("newdir/sub/file.txt").exists(), "created file must be removed");
        assert!(!dir.join("newdir/sub").exists(), "newdir/sub must be removed on rollback");
        assert!(!dir.join("newdir").exists(), "newdir must be removed on rollback");
        // The second target was never written.
        assert!(!dir.join("other.txt").exists());
        // The workspace root itself survives.
        assert!(dir.exists(), "workspace root must never be removed");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_patch_rollback_keeps_preexisting_directories() {
        let (ws, dir) = temp_workspace("patch_rollback_keepdir");
        // `existing/` is a PRE-EXISTING directory (with an unrelated file in it).
        // A patch adds `existing/new.txt` (write #1) then fails on the second
        // write (#2). Rollback must remove the newly-added file but must NOT
        // remove `existing/` — it predated the patch and still holds `keep.txt`.
        fs::create_dir_all(dir.join("existing")).unwrap();
        fs::write(dir.join("existing/keep.txt"), b"keep me\n").unwrap();
        let fs = FailOnNthWrite::new(2);

        let patch = "\
*** Begin Patch
*** Add File: existing/new.txt
+added
*** Add File: other.txt
+world
*** End Patch
";
        let out = apply_patch(&json!({ "patch": patch }), &ws, &fs).unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "apply_error");

        // The newly-added file is rolled back, but the pre-existing directory and
        // its prior content are untouched.
        assert!(!dir.join("existing/new.txt").exists(), "added file must be removed");
        assert!(dir.join("existing").exists(), "pre-existing dir must NOT be removed");
        assert_eq!(
            fs::read(dir.join("existing/keep.txt")).unwrap(),
            b"keep me\n",
            "pre-existing content must survive rollback"
        );

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
            fn remove_dir(&self, _p: &std::path::Path) -> std::io::Result<()> {
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
        // Honest paging text: it must NOT promise startLine/limit reaches content
        // past the cap, and it must point at run_command for out-of-window ranges.
        assert!(
            !out.model_text.contains("page further"),
            "must not over-promise paging past the cap: {}",
            out.model_text
        );
        assert!(
            out.model_text.contains("run_command"),
            "must point to run_command for ranges beyond the cap: {}",
            out.model_text
        );
        // The line count is annotated as coming from the capped prefix only.
        assert_eq!(out.data["linesAreFromCappedPrefix"], true);
        assert_eq!(out.data["cappedAtBytes"], MAX_READ_FILE_BYTES);

        let _ = fs::remove_dir_all(&dir);
    }

    /// A FileSystem double that pretends a file is larger than the cap but whose
    /// readable prefix is a small, multi-line, byte-budget-fitting body. This
    /// drives `read_file`'s NON-spill truncated branch (the rendered view fits
    /// `maxBytes`, yet `bytesTruncated` is true) so the honest paging note and the
    /// "startLine beyond window" note can be asserted directly.
    struct TruncatedPrefixFs {
        prefix: Vec<u8>,
        total_len: usize,
    }
    impl FileSystem for TruncatedPrefixFs {
        fn read(&self, _p: &std::path::Path) -> std::io::Result<Vec<u8>> {
            panic!("read_file must use read_capped, not read, for a large file");
        }
        fn read_capped(
            &self,
            _p: &std::path::Path,
            _max: usize,
        ) -> std::io::Result<(Vec<u8>, bool)> {
            // The prefix always fits `max` (it is tiny); report truncation because
            // the underlying file is `total_len` > the prefix length.
            Ok((self.prefix.clone(), self.total_len > self.prefix.len()))
        }
        fn write_atomic(&self, _p: &std::path::Path, _b: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
        fn metadata(&self, _p: &std::path::Path) -> std::io::Result<FileMetadata> {
            Ok(FileMetadata { len: self.total_len as u64, is_dir: false })
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
        fn remove_dir(&self, _p: &std::path::Path) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn read_file_truncated_but_fitting_view_states_paging_is_window_only() {
        let (ws, dir) = temp_workspace("read_trunc_fit");
        fs::write(dir.join("big.txt"), b"placeholder").unwrap();
        let fs = TruncatedPrefixFs {
            prefix: b"l1\nl2\nl3\n".to_vec(),
            total_len: MAX_READ_FILE_BYTES + 1, // pretend the file is past the cap
        };

        let out = read_file(&json!({ "path": "big.txt" }), &ws, &fs, "tc_tf", None).unwrap();
        assert!(out.ok);
        // The small prefix renders inline (non-spill branch) yet truncation is set.
        assert!(out.data.get("logRef").is_none(), "small prefix should not spill");
        assert_eq!(out.data["bytesTruncated"], true);
        assert!(out.model_text.contains("l1"));
        assert!(out.model_text.contains("l3"));
        // Honest note: not the old misleading "page further"; mentions window-only
        // paging and run_command.
        assert!(
            !out.model_text.contains("page further"),
            "must not say 'page further': {}",
            out.model_text
        );
        assert!(out.model_text.contains("within this window only"));
        assert!(out.model_text.contains("run_command"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_start_beyond_truncated_window_returns_explanatory_note() {
        let (ws, dir) = temp_workspace("read_start_beyond");
        fs::write(dir.join("big.txt"), b"placeholder").unwrap();
        // Prefix has 3 lines; the file is (pretended) larger than the cap.
        let fs = TruncatedPrefixFs {
            prefix: b"l1\nl2\nl3\n".to_vec(),
            total_len: MAX_READ_FILE_BYTES + 1,
        };

        // Ask to start at line 9999, far past the 3 lines available in the window.
        let out = read_file(
            &json!({ "path": "big.txt", "startLine": 9999 }),
            &ws,
            &fs,
            "tc_sb",
            None,
        )
        .unwrap();
        // Still ok:true with an explanatory body — we do not pretend lines exist.
        assert!(out.ok);
        assert_eq!(out.data["startBeyondWindow"], true);
        assert!(
            out.model_text.contains("beyond the 3 line(s) in this read window"),
            "should explain the requested start is past the window: {}",
            out.model_text
        );
        assert!(out.model_text.contains("run_command"));

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

    #[test]
    fn read_file_maxbytes_cannot_widen_the_raw_read() {
        // A model passing an enormous `maxBytes` must NOT widen the raw read off
        // disk: the raw read is hard-bounded to MAX_READ_FILE_BYTES. This double
        // records the `max` it was handed by `read_capped`.
        struct RecordingCapFs {
            last_max: std::sync::atomic::AtomicUsize,
        }
        impl FileSystem for RecordingCapFs {
            fn read(&self, _p: &std::path::Path) -> std::io::Result<Vec<u8>> {
                panic!("read_file must use read_capped, not read");
            }
            fn read_capped(
                &self,
                _p: &std::path::Path,
                max: usize,
            ) -> std::io::Result<(Vec<u8>, bool)> {
                self.last_max
                    .store(max, std::sync::atomic::Ordering::SeqCst);
                // Hand back a tiny file that fits within any cap.
                Ok((b"alpha\nbeta\n".to_vec(), false))
            }
            fn write_atomic(&self, _p: &std::path::Path, _b: &[u8]) -> std::io::Result<()> {
                Ok(())
            }
            fn metadata(&self, _p: &std::path::Path) -> std::io::Result<FileMetadata> {
                Ok(FileMetadata { len: 11, is_dir: false })
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
            fn remove_dir(&self, _p: &std::path::Path) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (ws, dir) = temp_workspace("read_no_widen");
        fs::write(dir.join("f.txt"), b"placeholder").unwrap();
        let fs = RecordingCapFs {
            last_max: std::sync::atomic::AtomicUsize::new(0),
        };

        // A wildly oversized maxBytes must be clamped: the raw read uses the hard
        // ceiling, not 500 MB.
        let out = read_file(
            &json!({ "path": "f.txt", "maxBytes": 500_000_000usize }),
            &ws,
            &fs,
            "tc_no_widen",
            None,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(
            fs.last_max.load(std::sync::atomic::Ordering::SeqCst),
            MAX_READ_FILE_BYTES,
            "the raw read must be capped at MAX_READ_FILE_BYTES, not the model's maxBytes"
        );
        // The small content is still shown normally (no truncation).
        assert_eq!(out.data["truncated"], false);
        assert!(out.model_text.contains("alpha"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_rendered_cap_is_clamped_to_ceiling() {
        // The rendered inline cap (max_bytes) is also clamped DOWN to the hard
        // ceiling. With a small file and a huge maxBytes, the whole file renders
        // inline (it is far below the clamped cap) and nothing spills.
        let (ws, dir) = temp_workspace("read_clamp");
        let fs = StdFileSystem::new();
        fs::write(dir.join("s.txt"), b"one\ntwo\nthree\n").unwrap();

        let out = read_file(
            &json!({ "path": "s.txt", "maxBytes": 999_999_999usize }),
            &ws,
            &fs,
            "tc_clamp",
            None,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.data["truncated"], false);
        assert!(out.model_text.contains("one"));
        assert!(out.model_text.contains("three"));
        // No spill reference: the content fit inline.
        assert!(out.data.get("logRef").is_none());

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
            DEFAULT_MAX_WRITE_FILE_BYTES,
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
        assert!(preview_diff(
            FileTool::Read,
            &json!({ "path": "a.txt" }),
            &ws,
            &fs,
            DEFAULT_MAX_WRITE_FILE_BYTES
        )
        .is_none());
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
        let diff = preview_diff(
            FileTool::ApplyPatch,
            &json!({ "patch": patch }),
            &ws,
            &fs,
            DEFAULT_MAX_WRITE_FILE_BYTES,
        )
        .expect("preview");
        assert!(diff.contains("update a.txt"));
        // Untouched.
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"keep\nold\ntail\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_diff_for_oversized_write_summarizes_without_building_diff() {
        let (ws, dir) = temp_workspace("preview_too_large");
        let fs = StdFileSystem::new();
        let big = "x".repeat(4096);

        // Under a 1 KiB limit, the preview must NOT parse/clone/diff the content —
        // it returns a refusal summary (mirrors the executor's too_large refusal).
        let preview = preview_diff(
            FileTool::Write,
            &json!({ "path": "big.txt", "content": big }),
            &ws,
            &fs,
            1024,
        )
        .expect("summary");
        assert!(
            preview.contains("over the 1024-byte write limit"),
            "preview should summarize the refusal: {preview}"
        );
        assert!(!preview.contains('+'), "no diff lines should be built: {preview}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_diff_for_large_write_summarizes_instead_of_full_diff() {
        let (ws, dir) = temp_workspace("preview_large_diff");
        let fs = StdFileSystem::new();
        // Content under the write limit but over the diff-input gate → summary, not
        // a full line diff (so the preview never builds a multi-MB diff).
        let big = "a\n".repeat(MAX_DIFF_INPUT_BYTES);

        let preview = preview_diff(
            FileTool::Write,
            &json!({ "path": "big.txt", "content": big }),
            &ws,
            &fs,
            50 * 1024 * 1024,
        )
        .expect("summary");
        assert!(
            preview.contains("diff too large to preview"),
            "large write should summarize: {preview}"
        );

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

    #[test]
    fn classify_write_reads_only_path_not_content() {
        // Permission classification must not parse/clone the (possibly huge)
        // content: it succeeds from the path ALONE. With the old full-parse this
        // would error (content is a required field) and the unwrap would panic.
        let (ws, dir) = temp_workspace("classify_write_path_only");
        let capability = classify(FileTool::Write, &json!({ "path": "out.txt" }), &ws).unwrap();
        assert_eq!(capability.action, ToolPermissionAction::Ask);
        assert_eq!(capability.touched_paths, vec!["out.txt".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_write_shallow_rejects_malformed_without_full_parse() {
        let missing_content =
            validate_args_shallow(FileTool::Write, &json!({ "path": "out.txt" }))
                .expect_err("missing content must fail before approval");
        assert!(
            missing_content.to_string().contains("content"),
            "got: {missing_content}"
        );

        let wrong_optional_type = validate_args_shallow(
            FileTool::Write,
            &json!({ "path": "out.txt", "content": "", "overwrite": "yes" }),
        )
        .expect_err("wrong optional type must fail before approval");
        assert!(
            wrong_optional_type.to_string().contains("overwrite"),
            "got: {wrong_optional_type}"
        );
    }
}
