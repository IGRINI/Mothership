//! Typed handlers for the two first-class, read-only search tools: `list_files`
//! and `search_text`.
//!
//! Both walk the project tree with the ripgrep [`ignore`] walker so they honor
//! `.gitignore`/`.ignore`/hidden filters by default (and can surface ignored
//! entries on request via `includeIgnored`). Both ALWAYS skip
//! [`Workspace::is_sensitive`] paths — even with `includeIgnored:true` — so a
//! credential file is never listed or grepped. Symlinks are never followed
//! (`follow_links(false)`), so the walk cannot escape the workspace.
//!
//! Like the file tools, these handlers never call `std::fs` for byte IO of file
//! contents: `search_text` reads each candidate through the injected
//! [`FileSystem`] port. (The directory *walk* itself uses the [`ignore`] walker,
//! which reads directory entries directly off the real filesystem — the same way
//! `run_command` shells out to the real fs — but it is fully contained to the
//! resolved, workspace-checked walk root and never follows a symlink out.)
//!
//! Both tools are pure reads inside the workspace: [`classify_search`] reports
//! [`ToolPermissionAction::Allow`] within the workspace and `Deny` for a `dir`
//! that escapes or points at a sensitive path.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};

use super::file_tools::{FileToolCapability, FileToolError, FileToolOutcome, FileToolSpill};
use super::filesystem::{FileSystem, PathError, Workspace};
use super::permissions::ToolPermissionAction;

/// Default cap on how many entries `list_files` returns before reporting
/// `truncated`. Keeps a huge tree from flooding the model context.
const DEFAULT_LIST_LIMIT: usize = 1000;

/// Default cap on how many matches `search_text` returns before reporting
/// `truncated`.
const DEFAULT_MAX_MATCHES: usize = 200;

/// Hard ceiling on per-call results regardless of the caller's requested limit,
/// so a request for a very large limit cannot allocate unbounded results.
const MAX_LIST_LIMIT: usize = 50_000;
const MAX_SEARCH_MATCHES: usize = 10_000;

/// Per-file byte ceiling for `search_text` reads (mirrors `read_file`'s raw read
/// ceiling): a single huge file is scanned only up to this prefix.
const MAX_READ_FILE_BYTES: usize = 10 * 1024 * 1024;

/// Cap on a single matched line's rendered length, so a minified/one-line file
/// does not emit a multi-megabyte match line.
const MAX_MATCH_LINE_BYTES: usize = 1024;

/// How many partially-read (capped, > `MAX_READ_FILE_BYTES`) file paths
/// `search_text` lists in `cappedFiles`; the total is reported as `cappedFileCount`.
const MAX_CAPPED_FILES_REPORTED: usize = 50;

/// Budget for the inline model text / event payload before results spill to the
/// output store and the model gets a preview + `logRef` instead. Mirrors the
/// conservative event budget used by the file tools.
const MAX_RESULT_INLINE_BYTES: usize = 64 * 1024;

/// Number of leading bytes sampled for the binary-content heuristic (matches the
/// file-tools heuristic).
const BINARY_SNIFF_BYTES: usize = 4096;

// ===========================================================================
// Inputs
// ===========================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListFilesInput {
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub include_ignored: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchTextInput {
    pub pattern: String,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub regex: Option<bool>,
    #[serde(default)]
    pub ignore_case: Option<bool>,
    #[serde(default)]
    pub max_matches: Option<usize>,
    #[serde(default)]
    pub include_ignored: Option<bool>,
}

// ===========================================================================
// Capability classification (intent → allow/deny)
// ===========================================================================

/// Classify `list_files` into a read [`FileToolCapability`]. The walk root
/// (`dir`, default the workspace root) must resolve inside the workspace and must
/// not be a sensitive path; otherwise the call is denied. Sensitive *entries*
/// inside an allowed root are skipped during the walk, not denied here.
pub fn classify_list_files(
    arguments: &Value,
    workspace: &Workspace,
) -> Result<FileToolCapability, FileToolError> {
    let input: ListFilesInput = parse_args(arguments)?;
    let touched = vec![input.dir.clone().unwrap_or_else(|| ".".to_string())];
    match guard_dir(workspace, input.dir.as_deref()) {
        Ok(_) => Ok(FileToolCapability {
            action: ToolPermissionAction::Allow,
            summary: format!("list files in {}", touched[0]),
            touched_paths: touched,
        }),
        Err(reason) => Ok(deny(reason, touched)),
    }
}

/// Classify `search_text` into a read [`FileToolCapability`]. Same `dir`
/// containment/sensitivity gate as [`classify_list_files`].
pub fn classify_search_text(
    arguments: &Value,
    workspace: &Workspace,
) -> Result<FileToolCapability, FileToolError> {
    let input: SearchTextInput = parse_args(arguments)?;
    let touched = vec![input.dir.clone().unwrap_or_else(|| ".".to_string())];
    match guard_dir(workspace, input.dir.as_deref()) {
        Ok(_) => Ok(FileToolCapability {
            action: ToolPermissionAction::Allow,
            summary: format!("search for `{}` in {}", input.pattern, touched[0]),
            touched_paths: touched,
        }),
        Err(reason) => Ok(deny(reason, touched)),
    }
}

fn deny(reason: String, touched: Vec<String>) -> FileToolCapability {
    FileToolCapability {
        action: ToolPermissionAction::Deny,
        summary: reason,
        touched_paths: touched,
    }
}

// ===========================================================================
// list_files
// ===========================================================================

/// Run `list_files`. Walks the (contained) `dir` with the [`ignore`] walker,
/// optionally filters entries by a workspace-relative `glob`, always skips
/// sensitive paths, and returns the relative file paths (capped by `limit`).
pub fn list_files(
    arguments: &Value,
    workspace: &Workspace,
    tool_call_id: &str,
    spill: Option<&dyn FileToolSpill>,
) -> Result<FileToolOutcome, FileToolError> {
    let input: ListFilesInput = parse_args(arguments)?;

    let root = match guard_dir(workspace, input.dir.as_deref()) {
        Ok(root) => root,
        Err(reason) => return Ok(path_failure(&input, reason)),
    };

    let matcher = match build_glob_matcher(input.glob.as_deref()) {
        Ok(matcher) => matcher,
        Err(reason) => {
            return Ok(FileToolOutcome::failure(
                reason.clone(),
                json!({ "status": "invalid_glob", "reason": reason }),
            ));
        }
    };

    let include_ignored = input.include_ignored.unwrap_or(false);
    let limit = input
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .min(MAX_LIST_LIMIT);

    let mut paths: Vec<String> = Vec::new();
    let mut scan_truncated = false;
    let walker = build_walker(&root, include_ignored);
    for entry in walker {
        let Ok(entry) = entry else { continue };
        // Files only (skip directories and other non-file entries).
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Some(relative) = workspace_relative(workspace, entry.path()) else {
            continue;
        };
        // Sensitive paths are ALWAYS skipped, even with includeIgnored.
        if super::filesystem::is_sensitive_relative(Path::new(&relative)) {
            continue;
        }
        if let Some(matcher) = &matcher {
            if !matcher.is_match(&relative) {
                continue;
            }
        }
        // Collect up to the hard scan ceiling (bounds memory in a huge tree); we
        // sort and apply `limit` AFTER the walk so `limit` yields a STABLE first-N
        // of the sorted set, not whatever order the walker produced.
        if paths.len() >= MAX_LIST_LIMIT {
            scan_truncated = true;
            break;
        }
        paths.push(relative);
    }

    paths.sort();
    let truncated = scan_truncated || paths.len() > limit;
    paths.truncate(limit);
    let count = paths.len();

    let mut data = json!({
        "dir": input.dir.clone().unwrap_or_else(|| ".".to_string()),
        "count": count,
        "truncated": truncated,
        "includeIgnored": include_ignored,
    });
    if let Some(glob) = &input.glob {
        data["glob"] = json!(glob);
    }

    // Render the full list; spill if it exceeds the inline budget.
    let full_text = paths.join("\n");
    if full_text.len() > MAX_RESULT_INLINE_BYTES {
        let preview = take_prefix_on_char_boundary(&full_text, MAX_RESULT_INLINE_BYTES);
        let log_ref = match spill {
            Some(spill) => Some(
                spill
                    .spill(tool_call_id, &full_text)
                    .map_err(|error| FileToolError::Io(error.to_string()))?,
            ),
            None => None,
        };
        data["truncated"] = json!(true);
        if let Some(log_ref) = &log_ref {
            data["logRef"] = json!(log_ref);
        }
        let mut model_text = format!("{count} file(s):\n{preview}");
        model_text.push_str("\n... list truncated ...\n");
        if let Some(log_ref) = &log_ref {
            model_text.push_str(&format!("full list: {log_ref}\n"));
        }
        return Ok(FileToolOutcome {
            ok: true,
            model_text,
            data,
            sha256: None,
            diff: None,
        });
    }

    let model_text = if count == 0 {
        "no files matched".to_string()
    } else if truncated {
        format!("{count} file(s) (truncated at limit {limit}):\n{full_text}")
    } else {
        format!("{count} file(s):\n{full_text}")
    };

    Ok(FileToolOutcome {
        ok: true,
        model_text,
        data,
        sha256: None,
        diff: None,
    })
}

// ===========================================================================
// search_text
// ===========================================================================

/// One matched line in a file, returned by `search_text`.
struct TextMatch {
    path: String,
    line_number: usize,
    line: String,
}

/// Run `search_text`. Compiles `pattern` (literal by default, regex when
/// `regex:true`), walks the (contained) `dir` like `list_files`, reads each
/// non-binary candidate bounded through the [`FileSystem`] port, and collects
/// matching lines up to `maxMatches`.
pub fn search_text(
    arguments: &Value,
    workspace: &Workspace,
    fs: &dyn FileSystem,
    tool_call_id: &str,
    spill: Option<&dyn FileToolSpill>,
) -> Result<FileToolOutcome, FileToolError> {
    let input: SearchTextInput = parse_args(arguments)?;

    let root = match guard_dir(workspace, input.dir.as_deref()) {
        Ok(root) => root,
        Err(reason) => return Ok(search_path_failure(&input, reason)),
    };

    let matcher = match build_glob_matcher(input.glob.as_deref()) {
        Ok(matcher) => matcher,
        Err(reason) => {
            return Ok(FileToolOutcome::failure(
                reason.clone(),
                json!({ "status": "invalid_glob", "reason": reason }),
            ));
        }
    };

    // Build ONE regex code path: a literal pattern is escaped so it matches
    // verbatim; a `regex:true` pattern is used as-is. A compile error is a clean
    // non-ok outcome with the underlying message.
    let use_regex = input.regex.unwrap_or(false);
    let ignore_case = input.ignore_case.unwrap_or(false);
    let pattern_source = if use_regex {
        input.pattern.clone()
    } else {
        regex::escape(&input.pattern)
    };
    let regex = match RegexBuilder::new(&pattern_source)
        .case_insensitive(ignore_case)
        .build()
    {
        Ok(regex) => regex,
        Err(error) => {
            return Ok(FileToolOutcome::failure(
                format!("invalid regex pattern: {error}"),
                json!({ "status": "invalid_regex", "detail": error.to_string() }),
            ));
        }
    };

    let include_ignored = input.include_ignored.unwrap_or(false);
    let max_matches = input
        .max_matches
        .unwrap_or(DEFAULT_MAX_MATCHES)
        .min(MAX_SEARCH_MATCHES);

    let mut matches: Vec<TextMatch> = Vec::new();
    // Distinct matched paths — counted via a set so a file whose matches fill the
    // buffer (and trigger the `break 'walk`) is still counted as a matched file.
    let mut matched_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Files that exceeded the per-file read ceiling and were searched only up to the
    // prefix (so a match past the cap may have been missed — surfaced as `partial`).
    let mut capped_count: usize = 0;
    let mut capped_files: Vec<String> = Vec::new();
    let mut truncated = false;
    let walker = build_walker(&root, include_ignored);

    'walk: for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Some(relative) = workspace_relative(workspace, entry.path()) else {
            continue;
        };
        if super::filesystem::is_sensitive_relative(Path::new(&relative)) {
            continue;
        }
        if let Some(matcher) = &matcher {
            if !matcher.is_match(&relative) {
                continue;
            }
        }

        // Read bounded through the injected port; skip on IO error (e.g. a file
        // removed mid-walk) rather than aborting the whole search.
        let Ok((bytes, capped)) = fs.read_capped(entry.path(), MAX_READ_FILE_BYTES) else {
            continue;
        };
        if looks_binary(&bytes) {
            continue;
        }
        if capped {
            capped_count += 1;
            if capped_files.len() < MAX_CAPPED_FILES_REPORTED && !capped_files.contains(&relative) {
                capped_files.push(relative.clone());
            }
        }
        let text = String::from_utf8_lossy(&bytes);

        for (index, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                // Count the file the moment a line matches — BEFORE the max_matches
                // break — so a file whose matches fill the buffer is still counted.
                matched_paths.insert(relative.clone());
                if matches.len() >= max_matches {
                    truncated = true;
                    break 'walk;
                }
                matches.push(TextMatch {
                    path: relative.clone(),
                    line_number: index + 1,
                    line: clamp_line(line),
                });
            }
        }
    }

    let count = matches.len();
    let files_with_matches = matched_paths.len();
    let partial = capped_count > 0;
    let match_values: Vec<Value> = matches
        .iter()
        .map(|m| {
            json!({
                "path": m.path,
                "lineNumber": m.line_number,
                "line": m.line,
            })
        })
        .collect();

    let mut data = json!({
        "dir": input.dir.clone().unwrap_or_else(|| ".".to_string()),
        "pattern": input.pattern,
        "regex": use_regex,
        "ignoreCase": ignore_case,
        "count": count,
        "filesWithMatches": files_with_matches,
        "truncated": truncated,
        "includeIgnored": include_ignored,
        "partial": partial,
    });
    if let Some(glob) = &input.glob {
        data["glob"] = json!(glob);
    }
    if partial {
        data["readCeilingBytes"] = json!(MAX_READ_FILE_BYTES);
        data["cappedFileCount"] = json!(capped_count);
        data["cappedFiles"] = json!(capped_files);
    }

    let mut summary = format!("{count} match(es) in {files_with_matches} file(s)");
    if partial {
        summary.push_str(&format!(
            "; note: {capped_count} file(s) exceeded the {}-byte per-file search cap and were searched only up to that prefix (matches beyond it were not found — use run_command for a full search of very large files)",
            MAX_READ_FILE_BYTES
        ));
    }
    let full_text = render_matches(&matches);
    if full_text.len() > MAX_RESULT_INLINE_BYTES {
        let preview = take_prefix_on_char_boundary(&full_text, MAX_RESULT_INLINE_BYTES);
        let log_ref = match spill {
            Some(spill) => Some(
                spill
                    .spill(tool_call_id, &full_text)
                    .map_err(|error| FileToolError::Io(error.to_string()))?,
            ),
            None => None,
        };
        data["truncated"] = json!(true);
        if let Some(log_ref) = &log_ref {
            data["logRef"] = json!(log_ref);
        }
        data["matches"] = json!(match_values);
        let mut model_text = format!("{summary}:\n{preview}");
        model_text.push_str("\n... results truncated ...\n");
        if let Some(log_ref) = &log_ref {
            model_text.push_str(&format!("full results: {log_ref}\n"));
        }
        return Ok(FileToolOutcome {
            ok: true,
            model_text,
            data,
            sha256: None,
            diff: None,
        });
    }

    data["matches"] = json!(match_values);
    let model_text = if count == 0 {
        format!("{summary} (no matches)")
    } else if truncated {
        format!("{summary} (truncated at {max_matches}):\n{full_text}")
    } else {
        format!("{summary}:\n{full_text}")
    };

    Ok(FileToolOutcome {
        ok: true,
        model_text,
        data,
        sha256: None,
        diff: None,
    })
}

// ===========================================================================
// Shared helpers
// ===========================================================================

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: &Value) -> Result<T, FileToolError> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| FileToolError::InvalidArguments(error.to_string()))
}

/// Resolve and screen the walk root `dir` (default the workspace root): it must
/// resolve inside the workspace and must not itself be a sensitive path. Returns
/// the resolved absolute root, or a human-readable denial reason.
fn guard_dir(workspace: &Workspace, dir: Option<&str>) -> Result<PathBuf, String> {
    let resolved = match dir {
        Some(dir) => workspace
            .resolve(dir)
            .map_err(|error: PathError| error.to_string())?,
        None => workspace.root().to_path_buf(),
    };
    if workspace.is_sensitive(&resolved) {
        return Err(format!(
            "`{}` is a sensitive path and is blocked",
            dir.unwrap_or(".")
        ));
    }
    Ok(resolved)
}

/// Build a [`GlobMatcher`] from an optional glob, rejecting a glob that could
/// enable escape (absolute, or one with a leading `..`). Matching is performed
/// against workspace-RELATIVE paths only, so even without this guard a glob
/// could not reach outside; the guard makes the rejection explicit. Returns
/// `Ok(None)` when no glob was supplied.
fn build_glob_matcher(glob: Option<&str>) -> Result<Option<GlobMatcher>, String> {
    let Some(glob) = glob else {
        return Ok(None);
    };
    let trimmed = glob.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let normalized = trimmed.replace('\\', "/");
    if normalized.starts_with('/')
        || normalized.starts_with("../")
        || normalized == ".."
        || normalized.contains("/../")
        || Path::new(trimmed).is_absolute()
    {
        return Err(format!(
            "glob `{glob}` must be a relative pattern that stays inside the project"
        ));
    }
    let compiled =
        Glob::new(&normalized).map_err(|error| format!("invalid glob `{glob}`: {error}"))?;
    Ok(Some(compiled.compile_matcher()))
}

/// Build the [`ignore`] walker for `root`. By default standard filters
/// (`.gitignore`/`.ignore`/hidden/parents) are ON; when `include_ignored` is set
/// they are turned OFF so ignored/hidden entries surface — but symlinks are NEVER
/// followed, so the walk cannot escape the workspace regardless.
///
/// The VCS `.git` directory is ALWAYS pruned (even with `include_ignored`): its
/// internals (`objects`, `refs`, …) are never useful tool results, and
/// `.git/config` is a sensitive path. Pruning the whole subtree also avoids
/// walking thousands of object files only to filter them out per-entry.
fn build_walker(root: &Path, include_ignored: bool) -> ignore::Walk {
    let mut builder = WalkBuilder::new(root);
    builder.follow_links(false);
    if include_ignored {
        builder
            .standard_filters(false)
            .hidden(false)
            .parents(false)
            .ignore(false)
            .git_global(false)
            .git_ignore(false)
            .git_exclude(false);
    }
    builder.filter_entry(|entry| {
        // Prune the `.git` directory subtree. (Only a top-level component named
        // `.git` matters here; a file merely *named* `.git` deeper in the tree is
        // not a VCS dir, but pruning any `.git`-named directory is the safe,
        // conventional choice and never hides real source.)
        !(entry.file_type().is_some_and(|ft| ft.is_dir())
            && entry.file_name() == std::ffi::OsStr::new(".git"))
    });
    builder.build()
}

/// The workspace-relative, forward-slash path for an absolute entry path, or
/// `None` if it is the root itself or somehow not under the root.
fn workspace_relative(workspace: &Workspace, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(workspace.root()).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn path_failure(input: &ListFilesInput, reason: String) -> FileToolOutcome {
    FileToolOutcome::failure(
        reason.clone(),
        json!({
            "dir": input.dir.clone().unwrap_or_else(|| ".".to_string()),
            "status": "denied",
            "reason": reason,
        }),
    )
}

fn search_path_failure(input: &SearchTextInput, reason: String) -> FileToolOutcome {
    FileToolOutcome::failure(
        reason.clone(),
        json!({
            "dir": input.dir.clone().unwrap_or_else(|| ".".to_string()),
            "status": "denied",
            "reason": reason,
        }),
    )
}

/// Render matches as `path:lineNumber:line` rows.
fn render_matches(matches: &[TextMatch]) -> String {
    let mut out = String::new();
    for m in matches {
        out.push_str(&format!("{}:{}:{}\n", m.path, m.line_number, m.line));
    }
    out
}

/// Clamp a single matched line to [`MAX_MATCH_LINE_BYTES`] on a char boundary so
/// a minified/one-line file cannot emit a huge match line.
fn clamp_line(line: &str) -> String {
    if line.len() <= MAX_MATCH_LINE_BYTES {
        return line.to_string();
    }
    let mut clamped = take_prefix_on_char_boundary(line, MAX_MATCH_LINE_BYTES);
    clamped.push_str(" …[line truncated]");
    clamped
}

/// Take the longest prefix of `text` not exceeding `max_bytes`, ending on a char
/// boundary so the result is always valid UTF-8.
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

/// Heuristic binary-content guard over the leading [`BINARY_SNIFF_BYTES`] bytes:
/// any NUL byte, or more than 30% non-printable bytes, marks the content binary.
/// Mirrors the file-tools heuristic so `search_text` skips the same files
/// `read_file` would refuse.
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
        let printable =
            matches!(byte, b'\t' | b'\n' | b'\r') || (0x20..=0x7E).contains(&byte) || byte >= 0x80;
        if !printable {
            non_printable += 1;
        }
    }
    non_printable * 100 > sample.len() * 30
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::filesystem::StdFileSystem;
    use std::fs;

    fn temp_workspace(label: &str) -> (Workspace, PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mothership_search_{label}_{nanos}"));
        fs::create_dir_all(&dir).expect("create workspace");
        let ws = Workspace::new(&dir).expect("workspace");
        (ws, dir)
    }

    // ---- list_files -------------------------------------------------------

    #[test]
    fn list_files_matches_glob() {
        let (ws, dir) = temp_workspace("list_glob");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), b"fn main() {}\n").unwrap();
        fs::write(dir.join("src/lib.rs"), b"// lib\n").unwrap();
        fs::write(dir.join("README.md"), b"# readme\n").unwrap();

        let out = list_files(&json!({ "glob": "**/*.rs" }), &ws, "tc", None).unwrap();
        assert!(out.ok);
        assert!(out.model_text.contains("src/main.rs"));
        assert!(out.model_text.contains("src/lib.rs"));
        assert!(!out.model_text.contains("README.md"));
        assert_eq!(out.data["count"], 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_files_skips_gitignored_by_default() {
        let (ws, dir) = temp_workspace("list_gitignore");
        // The `ignore` walker only honors .gitignore inside a git repo, so mark
        // the temp dir as one (real Mothership projects are git repos).
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".gitignore"), b"ignored.txt\nbuild/\n").unwrap();
        fs::write(dir.join("kept.txt"), b"keep\n").unwrap();
        fs::write(dir.join("ignored.txt"), b"nope\n").unwrap();
        fs::create_dir_all(dir.join("build")).unwrap();
        fs::write(dir.join("build/artifact.txt"), b"art\n").unwrap();

        let out = list_files(&json!({}), &ws, "tc", None).unwrap();
        assert!(out.ok);
        assert!(out.model_text.contains("kept.txt"));
        assert!(!out.model_text.contains("ignored.txt"));
        assert!(!out.model_text.contains("build/artifact.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_files_include_ignored_surfaces_ignored_but_still_skips_sensitive() {
        let (ws, dir) = temp_workspace("list_include_ignored");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".gitignore"), b"ignored.txt\n.env\n").unwrap();
        fs::write(dir.join("kept.txt"), b"keep\n").unwrap();
        fs::write(dir.join("ignored.txt"), b"surfaced\n").unwrap();
        // A sensitive file that is ALSO gitignored: must stay hidden even with
        // includeIgnored.
        fs::write(dir.join(".env"), b"SECRET=1\n").unwrap();

        // First confirm the default DOES hide the ignored file (so the
        // includeIgnored assertion below is meaningful).
        let default = list_files(&json!({}), &ws, "tc", None).unwrap();
        assert!(
            !default.model_text.contains("ignored.txt"),
            "default walk must hide gitignored files: {}",
            default.model_text
        );

        let out = list_files(&json!({ "includeIgnored": true }), &ws, "tc", None).unwrap();
        assert!(out.ok);
        // Ignored file is now surfaced.
        assert!(
            out.model_text.contains("ignored.txt"),
            "includeIgnored should surface gitignored files: {}",
            out.model_text
        );
        // Sensitive file is STILL skipped.
        assert!(
            !out.model_text.contains(".env"),
            "sensitive .env must never be listed: {}",
            out.model_text
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_files_never_surfaces_git_internals_even_with_include_ignored() {
        let (ws, dir) = temp_workspace("list_git_pruned");
        fs::create_dir_all(dir.join(".git/objects")).unwrap();
        fs::write(dir.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        fs::write(dir.join(".git/objects/blob"), b"obj\n").unwrap();
        fs::write(dir.join("src.rs"), b"fn x() {}\n").unwrap();

        let out = list_files(&json!({ "includeIgnored": true }), &ws, "tc", None).unwrap();
        assert!(out.ok);
        assert!(out.model_text.contains("src.rs"));
        assert!(
            !out.model_text.contains(".git/"),
            ".git internals must be pruned even with includeIgnored: {}",
            out.model_text
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_files_rejects_dir_outside_workspace() {
        let (ws, dir) = temp_workspace("list_dir_escape");
        let out = list_files(&json!({ "dir": "../.." }), &ws, "tc", None).unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "denied");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_files_rejects_escaping_glob() {
        let (ws, dir) = temp_workspace("list_glob_escape");
        fs::write(dir.join("a.txt"), b"x\n").unwrap();

        // An escaping glob is rejected outright.
        let out = list_files(&json!({ "glob": "../**/*" }), &ws, "tc", None).unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "invalid_glob");

        // An absolute glob is rejected too.
        let out = list_files(&json!({ "glob": "/etc/*" }), &ws, "tc", None).unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "invalid_glob");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_files_limit_truncates() {
        let (ws, dir) = temp_workspace("list_limit");
        for i in 0..10 {
            fs::write(dir.join(format!("f{i}.txt")), b"x\n").unwrap();
        }

        let out = list_files(&json!({ "limit": 3 }), &ws, "tc", None).unwrap();
        assert!(out.ok);
        assert_eq!(out.data["count"], 3);
        assert_eq!(out.data["truncated"], true);
        // `limit` returns the STABLE first-N of the SORTED set (f0,f1,f2), not the
        // walker's arbitrary first-N. (Regression: sort used to run AFTER the limit.)
        assert!(out.model_text.contains("f0.txt"));
        assert!(out.model_text.contains("f1.txt"));
        assert!(out.model_text.contains("f2.txt"));
        assert!(!out.model_text.contains("f3.txt"));
        assert!(!out.model_text.contains("f9.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- search_text ------------------------------------------------------

    #[test]
    fn search_text_literal_default() {
        let (ws, dir) = temp_workspace("search_literal");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"hello world\nfoo.bar baz\n").unwrap();

        // A literal dot must match a literal dot, not "any char".
        let out = search_text(&json!({ "pattern": "foo.bar" }), &ws, &fs, "tc", None).unwrap();
        assert!(out.ok);
        assert_eq!(out.data["count"], 1);
        assert_eq!(out.data["matches"][0]["lineNumber"], 2);
        assert!(out.data["matches"][0]["line"]
            .as_str()
            .unwrap()
            .contains("foo.bar"));

        // The literal "fooXbar" must NOT match (proving the dot was escaped).
        let out = search_text(&json!({ "pattern": "fooXbar" }), &ws, &fs, "tc", None).unwrap();
        assert!(out.ok);
        assert_eq!(out.data["count"], 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_text_regex_mode() {
        let (ws, dir) = temp_workspace("search_regex");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"foo123\nbar\nbaz456\n").unwrap();

        let out = search_text(
            &json!({ "pattern": r"\w+\d+", "regex": true }),
            &ws,
            &fs,
            "tc",
            None,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.data["count"], 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_text_ignore_case() {
        let (ws, dir) = temp_workspace("search_case");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"Hello\nGOODBYE\n").unwrap();

        // Case-sensitive (default): "hello" does not match "Hello".
        let out = search_text(&json!({ "pattern": "hello" }), &ws, &fs, "tc", None).unwrap();
        assert_eq!(out.data["count"], 0);

        // Case-insensitive: it matches.
        let out = search_text(
            &json!({ "pattern": "hello", "ignoreCase": true }),
            &ws,
            &fs,
            "tc",
            None,
        )
        .unwrap();
        assert_eq!(out.data["count"], 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_text_skips_binary_files() {
        let (ws, dir) = temp_workspace("search_binary");
        let fs = StdFileSystem::new();
        // Binary file containing the search needle plus NUL bytes.
        fs::write(dir.join("blob.bin"), [b'n', b'e', b'e', b'd', 0, 0, 0]).unwrap();
        fs::write(dir.join("text.txt"), b"need\n").unwrap();

        let out = search_text(&json!({ "pattern": "need" }), &ws, &fs, "tc", None).unwrap();
        assert!(out.ok);
        // Only the text file matched; the binary file was skipped.
        assert_eq!(out.data["count"], 1);
        assert_eq!(out.data["matches"][0]["path"], "text.txt");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_text_gitignore_default_and_include_ignored() {
        let (ws, dir) = temp_workspace("search_gitignore");
        let fs = StdFileSystem::new();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".gitignore"), b"ignored.txt\n").unwrap();
        fs::write(dir.join("kept.txt"), b"needle here\n").unwrap();
        fs::write(dir.join("ignored.txt"), b"needle here\n").unwrap();

        // Default: ignored file is skipped.
        let out = search_text(&json!({ "pattern": "needle" }), &ws, &fs, "tc", None).unwrap();
        assert_eq!(out.data["count"], 1);
        assert_eq!(out.data["matches"][0]["path"], "kept.txt");

        // includeIgnored: both files are searched.
        let out = search_text(
            &json!({ "pattern": "needle", "includeIgnored": true }),
            &ws,
            &fs,
            "tc",
            None,
        )
        .unwrap();
        assert_eq!(out.data["count"], 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_text_always_skips_sensitive() {
        let (ws, dir) = temp_workspace("search_sensitive");
        let fs = StdFileSystem::new();
        fs::write(dir.join(".env"), b"SECRET=needle\n").unwrap();
        fs::write(dir.join("ok.txt"), b"needle\n").unwrap();

        // Even with includeIgnored, the sensitive .env is never searched.
        let out = search_text(
            &json!({ "pattern": "needle", "includeIgnored": true }),
            &ws,
            &fs,
            "tc",
            None,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.data["count"], 1);
        assert_eq!(out.data["matches"][0]["path"], "ok.txt");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_text_max_matches_truncates() {
        let (ws, dir) = temp_workspace("search_max");
        let fs = StdFileSystem::new();
        let body = "hit\n".repeat(20);
        fs::write(dir.join("a.txt"), body.as_bytes()).unwrap();

        let out = search_text(
            &json!({ "pattern": "hit", "maxMatches": 5 }),
            &ws,
            &fs,
            "tc",
            None,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.data["count"], 5);
        assert_eq!(out.data["truncated"], true);
        // The single file is still counted though its matches filled the buffer and
        // triggered the walk break (regression: filesWithMatches was 0 before the fix).
        assert_eq!(out.data["filesWithMatches"], 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_text_flags_capped_files_as_partial() {
        // A file that exceeds the per-file read ceiling is searched only up to its
        // prefix; the result must honestly flag `partial` + list the capped file
        // instead of silently returning a false negative for matches past the cap.
        struct CappedFs;
        impl FileSystem for CappedFs {
            fn read(&self, _p: &Path) -> std::io::Result<Vec<u8>> {
                Ok(b"hit here\n".to_vec())
            }
            fn read_capped(&self, _p: &Path, _max: usize) -> std::io::Result<(Vec<u8>, bool)> {
                // Pretend the file is larger than the cap: prefix + capped=true.
                Ok((b"hit here\n".to_vec(), true))
            }
            fn write_atomic(&self, _p: &Path, _b: &[u8]) -> std::io::Result<()> {
                Ok(())
            }
            fn metadata(
                &self,
                _p: &Path,
            ) -> std::io::Result<crate::tools::filesystem::FileMetadata> {
                Ok(crate::tools::filesystem::FileMetadata {
                    len: MAX_READ_FILE_BYTES as u64 + 1,
                    is_dir: false,
                })
            }
            fn exists(&self, _p: &Path) -> bool {
                true
            }
            fn rename(&self, _f: &Path, _t: &Path) -> std::io::Result<()> {
                Ok(())
            }
            fn remove_file(&self, _p: &Path) -> std::io::Result<()> {
                Ok(())
            }
            fn create_dir_all(&self, _p: &Path) -> std::io::Result<()> {
                Ok(())
            }
            fn remove_dir(&self, _p: &Path) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (ws, dir) = temp_workspace("search_capped");
        // A real file so the `ignore` walker discovers it; the double supplies the
        // (pretend-capped) content.
        fs::write(dir.join("big.txt"), b"placeholder").unwrap();
        let out = search_text(&json!({ "pattern": "hit" }), &ws, &CappedFs, "tc", None).unwrap();
        assert!(out.ok);
        assert_eq!(out.data["count"], 1);
        assert_eq!(out.data["partial"], true);
        assert_eq!(out.data["cappedFileCount"], 1);
        assert!(out.data["cappedFiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "big.txt"));
        assert!(out.model_text.contains("per-file search cap"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_text_bad_regex_is_not_ok() {
        let (ws, dir) = temp_workspace("search_badregex");
        let fs = StdFileSystem::new();
        fs::write(dir.join("a.txt"), b"x\n").unwrap();

        let out = search_text(
            &json!({ "pattern": "(unclosed", "regex": true }),
            &ws,
            &fs,
            "tc",
            None,
        )
        .unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "invalid_regex");
        assert!(out.model_text.contains("invalid regex"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_text_dir_containment_rejected() {
        let (ws, dir) = temp_workspace("search_dir_escape");
        let out = search_text(
            &json!({ "pattern": "x", "dir": "../.." }),
            &ws,
            &StdFileSystem::new(),
            "tc",
            None,
        )
        .unwrap();
        assert!(!out.ok);
        assert_eq!(out.data["status"], "denied");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_text_glob_filter() {
        let (ws, dir) = temp_workspace("search_glob");
        let fs = StdFileSystem::new();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), b"needle\n").unwrap();
        fs::write(dir.join("b.txt"), b"needle\n").unwrap();

        let out = search_text(
            &json!({ "pattern": "needle", "glob": "**/*.rs" }),
            &ws,
            &fs,
            "tc",
            None,
        )
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.data["count"], 1);
        assert_eq!(out.data["matches"][0]["path"], "src/a.rs");

        let _ = fs::remove_dir_all(&dir);
    }
}
