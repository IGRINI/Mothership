//! V4A ("apply_patch") multi-file patch parser and a pure dry-run planner.
//!
//! This module is intentionally **pure**: it depends only on `std`, performs no
//! real filesystem or network I/O, and is fully synchronous. It does two things:
//!
//! 1. [`parse_v4a`] turns a V4A patch envelope (the `*** Begin Patch` / `*** End
//!    Patch` format used by `apply_patch`) into a structured list of [`PatchOp`].
//! 2. [`plan_patch`] dry-runs those ops against an in-memory [`FileMap`] and
//!    returns a [`PatchPlan`] describing the resulting content of every touched
//!    file, or a [`PlanError`] if the patch cannot apply cleanly.
//!
//! The planner is **all-or-none**: the first op that cannot apply aborts the
//! entire plan with `Err`. It never produces a partial plan and never mutates
//! anything — applying the resulting `new_content` is the caller's job.
//!
//! # Relationship to the canonical OpenAI Codex `apply_patch`
//!
//! The grammar, the hunk-location algorithm (the escalating-tier fuzzy
//! `seek_sequence`), the `@@` context-header seek, the `*** End of File` anchor,
//! pure-addition placement, the trailing-empty-line retry, and the
//! trailing-newline normalization are all ported from
//! `codex-rs/apply-patch` so that patches a real Codex/GPT model emits are
//! accepted and applied identically. On top of that canonical core we keep our
//! own better properties:
//!
//! * typed [`PatchError`] / [`PlanError`] carrying **1-based line numbers**,
//! * **all-or-none** [`plan_patch`] (never a partial plan),
//! * an [`ExistsView`] overlay so duplicate targets and move collisions that
//!   only exist mid-patch are detected,
//! * CRLF tolerance, multi-file, and Add/Update/Delete/Move.
//!
//! # The V4A envelope grammar
//!
//! ```text
//! *** Begin Patch
//! *** Environment ID: <id>          (optional preamble; tolerated and ignored)
//! *** Add File: <path>
//! +<line>                           (every added line prefixed with '+')
//! *** Update File: <path>
//! *** Move to: <newpath>            (optional, only directly under Update)
//! @@ <optional context header>      (zero or more hunks; @@ may stack)
//!  <context line>                   (leading single space)
//! -<removed line>
//! +<added line>
//! *** End of File                   (optional; anchors the hunk at EOF)
//! *** Delete File: <path>
//! *** End Patch
//! ```
//!
//! Multiple file sections may appear between `*** Begin Patch` and `*** End
//! Patch`. CRLF and LF line endings are both accepted, and trailing whitespace
//! on structural marker lines is tolerated.
//!
//! # Trailing-newline policy (matches Codex)
//!
//! We replicate Codex's normalization exactly. The current file content is split
//! on `'\n'` and the trailing empty element produced by a final newline is
//! dropped, so line counts match `diff`. After applying all hunks, if the last
//! resulting line is not already empty we append one empty line before joining
//! with `'\n'`. The practical consequence is that **an updated file is always
//! newline-terminated** — even if the original had no final newline. This is the
//! canonical Codex behavior and is intentional; it keeps results identical to a
//! real `apply_patch`. (Add-file bodies are *not* forced to end with a newline;
//! they reproduce the `+` lines verbatim, also matching Codex.)

#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Parsed representation
// ---------------------------------------------------------------------------

/// A single file operation parsed from a V4A patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOp {
    /// Create a new file. `content` is the joined body of the `+`-prefixed lines
    /// (without a trailing newline).
    Add { path: String, content: String },
    /// Modify an existing file via one or more hunks, optionally renaming it.
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
    /// Remove an existing file.
    Delete { path: String },
}

/// A contiguous group of changes within an `Update` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The text following `@@` on the hunk header(s), if any were present, in
    /// order. `None`/empty when the hunk had no `@@` line (e.g. a single leading
    /// hunk). Multiple stacked `@@` lines (each narrowing the location) are
    /// supported and are sought in order before the old block is located. The
    /// canonical Codex format documents stacked `@@` lines; this is a strict
    /// superset of Codex's parser, which keeps only a single context line.
    pub context_headers: Vec<String>,
    /// The ordered body of the hunk.
    pub lines: Vec<HunkLine>,
    /// True if the hunk was terminated by a `*** End of File` marker, meaning its
    /// old block must be anchored at the end of the file (Codex searches the EOF
    /// position first when this is set).
    pub is_end_of_file: bool,
}

impl Hunk {
    /// The single (or last) context header, for compatibility with callers that
    /// expect Codex's one-context-line model. Returns the most specific (last)
    /// `@@` header if any were present.
    pub fn context_header(&self) -> Option<&str> {
        self.context_headers.last().map(String::as_str)
    }
}

/// One line inside a [`Hunk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    /// An unchanged line (used to anchor the hunk against current content).
    Context(String),
    /// A line removed from the current content.
    Removed(String),
    /// A line added to the new content.
    Added(String),
}

// ---------------------------------------------------------------------------
// Parse errors
// ---------------------------------------------------------------------------

/// An error produced while parsing a V4A patch.
///
/// Every variant carries a human-readable message and, where a specific source
/// line is implicated, the 1-based line number it occurred on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchError {
    /// The patch did not start with `*** Begin Patch`.
    MissingBegin,
    /// End of input was reached before `*** End Patch`.
    MissingEnd,
    /// A `*** Add/Update/Delete File:` (or `*** Move to:`) marker had an empty
    /// path after the colon.
    EmptyPath { line: usize, marker: String },
    /// A structural marker (`***` / `@@` / body line) appeared where it was not
    /// valid, or an unrecognized line was found.
    Unexpected { line: usize, detail: String },
    /// A `*** Move to:` marker appeared somewhere other than directly under an
    /// `*** Update File:` section (or appeared more than once).
    MisplacedMoveTo { line: usize },
    /// A body line inside a hunk did not begin with one of ` `, `-`, or `+`.
    BadHunkLine { line: usize, detail: String },
    /// Content appeared after `*** End Patch`, or no file sections were found.
    Structure { line: usize, detail: String },
    /// An `*** Environment ID:` preamble line was present but carried no id.
    EmptyEnvironmentId { line: usize },
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatchError::MissingBegin => {
                write!(f, "patch must begin with `*** Begin Patch`")
            }
            PatchError::MissingEnd => {
                write!(f, "patch is missing the terminating `*** End Patch`")
            }
            PatchError::EmptyPath { line, marker } => {
                write!(f, "line {line}: `{marker}` marker has an empty path")
            }
            PatchError::Unexpected { line, detail } => {
                write!(f, "line {line}: {detail}")
            }
            PatchError::MisplacedMoveTo { line } => write!(
                f,
                "line {line}: `*** Move to:` is only valid directly under `*** Update File:`"
            ),
            PatchError::BadHunkLine { line, detail } => {
                write!(f, "line {line}: {detail}")
            }
            PatchError::Structure { line, detail } => {
                write!(f, "line {line}: {detail}")
            }
            PatchError::EmptyEnvironmentId { line } => {
                write!(f, "line {line}: `*** Environment ID:` must not be empty")
            }
        }
    }
}

impl std::error::Error for PatchError {}

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File:";
const UPDATE_FILE: &str = "*** Update File:";
const DELETE_FILE: &str = "*** Delete File:";
const MOVE_TO: &str = "*** Move to:";
const END_OF_FILE: &str = "*** End of File";
const ENVIRONMENT_ID: &str = "*** Environment ID:";

/// Strip a single trailing `\r` (so the parser is CRLF-tolerant after we split
/// on `\n`).
fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// True if `line` is exactly the bare `marker`, ignoring trailing whitespace.
/// Used for the standalone `*** Begin Patch` / `*** End Patch` / `*** End of
/// File` markers, which carry no value and so may have arbitrary trailing
/// whitespace.
///
/// We never trim *leading* whitespace: a marker must start at column 0.
fn is_bare_marker(line: &str, marker: &str) -> bool {
    line.trim_end() == marker
}

/// If `line` (already CR-stripped) is a `*** <marker>: <value>` line for
/// `marker`, return the trimmed value. Trailing whitespace on the value is
/// tolerated; surrounding spaces after the colon are trimmed.
fn marker_value<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.strip_prefix(marker).map(str::trim)
}

/// True if a CR-stripped line is any `***` structural marker that terminates a
/// file body / hunk region. `*** End of File` is included so it cleanly ends a
/// hunk body. Trailing whitespace is tolerated for the bare markers.
fn is_section_marker(line: &str) -> bool {
    is_bare_marker(line, BEGIN_PATCH)
        || is_bare_marker(line, END_PATCH)
        || is_bare_marker(line, END_OF_FILE)
        || line.starts_with(ADD_FILE)
        || line.starts_with(UPDATE_FILE)
        || line.starts_with(DELETE_FILE)
        || line.starts_with(MOVE_TO)
}

/// True if a CR-stripped line is a git-style "no newline at end of file"
/// annotation. Such lines are emitted by some diff tools and carry no content;
/// like git/codex we skip them wherever they appear inside a body.
fn is_no_newline_marker(line: &str) -> bool {
    line.trim_start().starts_with("\\ No newline at end of file")
}

// ---------------------------------------------------------------------------
// Heredoc envelope
// ---------------------------------------------------------------------------

/// If `patch` is wrapped in a shell heredoc envelope, return the inner patch
/// body; otherwise return `patch` unchanged.
///
/// Models frequently emit the patch the way the Codex CLI is invoked, e.g.
///
/// ```text
/// apply_patch <<'EOF'        <<'EOF'        <<"EOF"        <<EOF
/// *** Begin Patch            ...            ...            ...
/// ...                        EOF            EOF            EOF
/// *** End Patch
/// EOF
/// ```
///
/// The wrapper is stripped only when the body genuinely contains both
/// `*** Begin Patch` and `*** End Patch`, so a real patch is never mistaken for
/// a heredoc. A malformed wrapper (no terminator, mismatched quotes) is left
/// untouched on purpose — it then surfaces as a normal parse error rather than a
/// guessed result. We never aggressively repair a broken heredoc.
fn strip_heredoc_envelope(patch: &str) -> &str {
    // Skip leading blank lines, then require the first real line to be a heredoc
    // opener (`[apply_patch ]<<['"]?DELIM['"]?`).
    let trimmed = patch.trim_start_matches(['\r', '\n']);
    let Some((first_line, body)) = trimmed.split_once('\n') else {
        return patch;
    };
    let Some(delimiter) = heredoc_delimiter(strip_cr(first_line)) else {
        return patch;
    };

    // The heredoc terminator is a line equal to the delimiter at column 0 (no
    // leading whitespace). Patch content lines are always prefixed (`+`/`-`/
    // space) or are `*** `/`@@` markers, so a bare column-0 delimiter can only be
    // the terminator — never patch content.
    let mut offset = 0usize;
    for line in body.split_inclusive('\n') {
        let content = strip_cr(line.strip_suffix('\n').unwrap_or(line));
        let is_terminator =
            !content.starts_with(|c: char| c.is_whitespace()) && content.trim_end() == delimiter;
        if is_terminator {
            let inner = &body[..offset];
            return if inner.contains(BEGIN_PATCH) && inner.contains(END_PATCH) {
                inner
            } else {
                patch
            };
        }
        offset += line.len();
    }

    // No terminator found: leave the input untouched so the heredoc opener
    // surfaces as a normal `MissingBegin` parse error.
    patch
}

/// Extract the heredoc delimiter token from an opener line, or `None` if the
/// line is not a recognized heredoc opener. Accepts an optional leading
/// `apply_patch` command and optional single/double quotes around the token.
fn heredoc_delimiter(opener: &str) -> Option<&str> {
    let opener = opener.trim();
    let (before, after) = opener.split_once("<<")?;
    let before = before.trim();
    if !before.is_empty() && before != "apply_patch" {
        return None;
    }
    let token = after.trim();
    let token = token
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .or_else(|| token.strip_prefix('"').and_then(|t| t.strip_suffix('"')))
        .unwrap_or(token);
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(token)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a V4A patch envelope into a list of [`PatchOp`].
///
/// Accepts both LF and CRLF line endings and tolerates trailing whitespace on
/// structural markers. Recognizes the optional `*** Environment ID:` preamble
/// (which is consumed and ignored) and the `*** End of File` hunk terminator.
/// Also tolerates a shell heredoc wrapper around the patch (e.g.
/// `apply_patch <<'EOF' … EOF`), stripping it only when the body contains a
/// complete patch. Returns a [`PatchError`] (with a 1-based line number where
/// one applies) on malformed input. Never panics.
pub fn parse_v4a(patch: &str) -> Result<Vec<PatchOp>, PatchError> {
    // Some models wrap the patch in a shell heredoc, mimicking the Codex CLI's
    // `apply_patch <<'EOF' … EOF`. Strip that envelope first; a malformed wrapper
    // is left intact so it surfaces as a normal parse error (we never guess).
    let patch = strip_heredoc_envelope(patch);

    // Split on '\n'; `strip_cr` handles the trailing '\r' of CRLF. We keep blank
    // lines because they are meaningful inside hunks / file bodies.
    let raw_lines: Vec<&str> = patch.split('\n').collect();
    let lines: Vec<&str> = raw_lines.iter().map(|l| strip_cr(l)).collect();

    // Locate `*** Begin Patch` (skipping leading blank lines only).
    let mut idx = 0usize;
    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }
    if idx >= lines.len() || !is_bare_marker(lines[idx], BEGIN_PATCH) {
        return Err(PatchError::MissingBegin);
    }
    idx += 1; // consume Begin Patch

    // Optional `*** Environment ID:` preamble, immediately after Begin Patch.
    // Codex tolerates this remote-execution marker; we accept and discard it but
    // still reject an empty id (matching Codex's validation).
    if idx < lines.len() {
        if let Some(id) = marker_value(lines[idx], ENVIRONMENT_ID) {
            if id.is_empty() {
                return Err(PatchError::EmptyEnvironmentId { line: idx + 1 });
            }
            idx += 1;
        }
    }

    let mut ops: Vec<PatchOp> = Vec::new();
    let mut saw_end = false;

    while idx < lines.len() {
        let line = lines[idx];
        let lineno = idx + 1; // 1-based

        if is_bare_marker(line, END_PATCH) {
            saw_end = true;
            idx += 1;
            break;
        }

        if is_bare_marker(line, BEGIN_PATCH) {
            return Err(PatchError::Unexpected {
                line: lineno,
                detail: "unexpected `*** Begin Patch` inside an open patch".to_string(),
            });
        }

        if let Some(path) = marker_value(line, ADD_FILE) {
            if path.is_empty() {
                return Err(PatchError::EmptyPath {
                    line: lineno,
                    marker: "*** Add File:".to_string(),
                });
            }
            idx += 1;
            let content = parse_add_body(&lines, &mut idx)?;
            ops.push(PatchOp::Add {
                path: path.to_string(),
                content,
            });
            continue;
        }

        if let Some(path) = marker_value(line, UPDATE_FILE) {
            if path.is_empty() {
                return Err(PatchError::EmptyPath {
                    line: lineno,
                    marker: "*** Update File:".to_string(),
                });
            }
            idx += 1;
            let (move_to, hunks) = parse_update_body(&lines, &mut idx)?;
            ops.push(PatchOp::Update {
                path: path.to_string(),
                move_to,
                hunks,
            });
            continue;
        }

        if let Some(path) = marker_value(line, DELETE_FILE) {
            if path.is_empty() {
                return Err(PatchError::EmptyPath {
                    line: lineno,
                    marker: "*** Delete File:".to_string(),
                });
            }
            idx += 1;
            ops.push(PatchOp::Delete {
                path: path.to_string(),
            });
            continue;
        }

        if marker_value(line, MOVE_TO).is_some() {
            return Err(PatchError::MisplacedMoveTo { line: lineno });
        }

        // Any non-marker line at the top level (outside a file section) is
        // invalid. Tolerate a fully blank line as harmless separator.
        if line.trim().is_empty() {
            idx += 1;
            continue;
        }

        return Err(PatchError::Unexpected {
            line: lineno,
            detail: format!("expected a file section marker, found {line:?}"),
        });
    }

    if !saw_end {
        return Err(PatchError::MissingEnd);
    }

    // Anything other than trailing blank lines after `*** End Patch` is an error.
    while idx < lines.len() {
        if !lines[idx].trim().is_empty() {
            return Err(PatchError::Structure {
                line: idx + 1,
                detail: format!("unexpected content after `*** End Patch`: {:?}", lines[idx]),
            });
        }
        idx += 1;
    }

    Ok(ops)
}

/// Parse the body of an `*** Add File:` section: a run of `+`-prefixed lines
/// terminated by the next `***` marker (or end of input). `*idx` is advanced to
/// the terminating marker (or past the end). A git-style `\ No newline at end of
/// file` annotation is tolerated and skipped.
fn parse_add_body(lines: &[&str], idx: &mut usize) -> Result<String, PatchError> {
    let mut body: Vec<&str> = Vec::new();
    while *idx < lines.len() {
        let line = lines[*idx];
        if is_section_marker(line) {
            break;
        }
        if is_no_newline_marker(line) {
            *idx += 1;
            continue;
        }
        let lineno = *idx + 1;
        match line.strip_prefix('+') {
            Some(rest) => body.push(rest),
            None => {
                // Allow a genuinely empty separator line to be treated as an
                // empty added line, matching common emitter behavior where a
                // blank added line may be written as "" rather than "+".
                if line.is_empty() {
                    body.push("");
                } else {
                    return Err(PatchError::Unexpected {
                        line: lineno,
                        detail: format!(
                            "lines inside `*** Add File:` must start with `+`, found {line:?}"
                        ),
                    });
                }
            }
        }
        *idx += 1;
    }
    Ok(body.join("\n"))
}

/// Parse the body of an `*** Update File:` section: an optional `*** Move to:`
/// immediately, then zero or more hunks. `*idx` is advanced to the terminating
/// `***` marker (or past the end).
fn parse_update_body(
    lines: &[&str],
    idx: &mut usize,
) -> Result<(Option<String>, Vec<Hunk>), PatchError> {
    let mut move_to: Option<String> = None;

    // Optional `*** Move to:` must come first, before any hunk content.
    if *idx < lines.len() {
        let line = lines[*idx];
        if let Some(dest) = marker_value(line, MOVE_TO) {
            if dest.is_empty() {
                return Err(PatchError::EmptyPath {
                    line: *idx + 1,
                    marker: "*** Move to:".to_string(),
                });
            }
            move_to = Some(dest.to_string());
            *idx += 1;
        }
    }

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    // True once the open hunk has any body line. A `@@` line seen *before* any
    // body line stacks onto the current hunk's headers; a `@@` seen *after* body
    // content opens a new hunk.
    let mut current_has_body = false;

    while *idx < lines.len() {
        let line = lines[*idx];
        let lineno = *idx + 1;

        // `*** End of File` terminates the *current* hunk (anchoring it at EOF)
        // and continues; any other section marker ends the whole update body.
        if is_bare_marker(line, END_OF_FILE) {
            if let Some(h) = current.as_mut() {
                h.is_end_of_file = true;
            } else {
                // An EOF marker with no preceding hunk content opens an empty,
                // EOF-anchored hunk (harmless; planning will treat it as such).
                current = Some(Hunk {
                    context_headers: Vec::new(),
                    lines: Vec::new(),
                    is_end_of_file: true,
                });
            }
            *idx += 1;
            // Flush the EOF-terminated hunk so a following `@@` starts fresh.
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current_has_body = false;
            continue;
        }

        if is_section_marker(line) {
            // A second `*** Move to:` (or a Move after hunk content) is invalid.
            if line.starts_with(MOVE_TO) {
                return Err(PatchError::MisplacedMoveTo { line: lineno });
            }
            break;
        }

        // A git-style "no newline" annotation carries no content; skip it.
        if is_no_newline_marker(line) {
            *idx += 1;
            continue;
        }

        // Hunk header.
        if let Some(rest) = line.strip_prefix("@@") {
            let header = rest.trim();
            match current.as_mut() {
                // Stacked `@@` lines (no body in between) narrow the same hunk.
                Some(h) if !current_has_body => {
                    if !header.is_empty() {
                        h.context_headers.push(header.to_string());
                    }
                }
                // Either no open hunk, or the open hunk already has body lines:
                // start a new hunk.
                _ => {
                    if let Some(h) = current.take() {
                        hunks.push(h);
                    }
                    let mut headers = Vec::new();
                    if !header.is_empty() {
                        headers.push(header.to_string());
                    }
                    current = Some(Hunk {
                        context_headers: headers,
                        lines: Vec::new(),
                        is_end_of_file: false,
                    });
                    current_has_body = false;
                }
            }
            *idx += 1;
            continue;
        }

        // A body line implicitly opens a leading (header-less) hunk if none is
        // open yet.
        let hunk = current.get_or_insert_with(|| Hunk {
            context_headers: Vec::new(),
            lines: Vec::new(),
            is_end_of_file: false,
        });

        // Classify the body line by its first byte.
        let first = line.as_bytes().first().copied();
        match first {
            Some(b' ') => hunk.lines.push(HunkLine::Context(line[1..].to_string())),
            Some(b'-') => hunk.lines.push(HunkLine::Removed(line[1..].to_string())),
            Some(b'+') => hunk.lines.push(HunkLine::Added(line[1..].to_string())),
            None => {
                // A genuinely empty line denotes an empty *context* line.
                hunk.lines.push(HunkLine::Context(String::new()));
            }
            Some(_) => {
                return Err(PatchError::BadHunkLine {
                    line: lineno,
                    detail: format!(
                        "hunk line must start with ' ', '-', or '+', found {line:?}"
                    ),
                });
            }
        }
        current_has_body = true;
        *idx += 1;
    }

    if let Some(h) = current.take() {
        hunks.push(h);
    }

    Ok((move_to, hunks))
}

// ---------------------------------------------------------------------------
// FileMap: the in-memory view of current file contents
// ---------------------------------------------------------------------------

/// A read-only, in-memory view of the files a patch may touch. The planner uses
/// only these two operations; it never touches a real filesystem.
pub trait FileMap {
    /// Return the current content of `path`, or `None` if it does not exist.
    fn read(&self, path: &str) -> Option<String>;
    /// Whether `path` currently exists.
    fn exists(&self, path: &str) -> bool;
}

/// A trivial [`FileMap`] backed by a `HashMap<String, String>`. Handy for tests
/// and simple call sites.
#[derive(Debug, Default, Clone)]
pub struct InMemoryFileMap {
    files: HashMap<String, String>,
}

impl InMemoryFileMap {
    /// Create an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite a file's content, returning `self` for chaining.
    pub fn with_file(mut self, path: impl Into<String>, content: impl Into<String>) -> Self {
        self.files.insert(path.into(), content.into());
        self
    }

    /// Insert or overwrite a file's content.
    pub fn insert(&mut self, path: impl Into<String>, content: impl Into<String>) {
        self.files.insert(path.into(), content.into());
    }
}

impl FileMap for InMemoryFileMap {
    fn read(&self, path: &str) -> Option<String> {
        self.files.get(path).cloned()
    }

    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }
}

// ---------------------------------------------------------------------------
// Plan representation
// ---------------------------------------------------------------------------

/// The kind of change planned for a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedKind {
    /// File will be created.
    Add,
    /// File will be edited in place.
    Update,
    /// File will be edited and renamed to `to`.
    Move { to: String },
    /// File will be deleted.
    Delete,
}

/// The planned outcome for a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// The file's current path (the source path for a move).
    pub path: String,
    /// What will happen to it.
    pub op: PlannedKind,
    /// The full new content after applying the patch. `None` for a `Delete`.
    pub new_content: Option<String>,
    /// Number of lines added.
    pub added: usize,
    /// Number of lines removed.
    pub removed: usize,
}

/// The complete dry-run plan: one [`PlannedFile`] per op, in op order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchPlan {
    pub files: Vec<PlannedFile>,
}

/// An error produced while planning (dry-running) a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// An `Update`/`Delete` referenced a file that does not exist.
    FileNotFound { path: String },
    /// An `Add` (or a move destination) targeted a path that already exists.
    FileExists { path: String },
    /// A hunk's context/removed lines could not be located in the current
    /// content. `hunk_index` is 0-based within that file's `Update`.
    HunkNoMatch {
        path: String,
        hunk_index: usize,
        detail: String,
    },
    /// The same path was targeted by more than one op in a single patch.
    DuplicatePath { path: String },
    /// A hunk contained no `-`/` ` lines to anchor against *and* nothing pinned a
    /// location (no `@@` header, not end-of-file, file not empty), so it cannot
    /// be located deterministically.
    EmptyHunk { path: String, hunk_index: usize },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::FileNotFound { path } => write!(f, "file not found: {path}"),
            PlanError::FileExists { path } => write!(f, "file already exists: {path}"),
            PlanError::HunkNoMatch {
                path,
                hunk_index,
                detail,
            } => write!(
                f,
                "hunk #{hunk_index} for {path} did not match current content: {detail}"
            ),
            PlanError::DuplicatePath { path } => {
                write!(f, "path targeted by more than one operation: {path}")
            }
            PlanError::EmptyHunk { path, hunk_index } => write!(
                f,
                "hunk #{hunk_index} for {path} has no context or removed lines to anchor on"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

// ---------------------------------------------------------------------------
// Planner (dry run)
// ---------------------------------------------------------------------------

/// Dry-run `ops` against `fs` and return a [`PatchPlan`].
///
/// This is the content check. It is **all-or-none**: the first op that cannot
/// apply cleanly aborts the entire plan and returns `Err`. On success, every
/// touched file is described by a [`PlannedFile`] carrying its full
/// `new_content` (except deletes). Nothing is written — applying the plan is the
/// caller's responsibility.
///
/// Each op is checked against the **original** `fs` state plus a running view of
/// paths already claimed within this same patch (to reject duplicate targets and
/// to honor moves). Hunks within a single `Update` are matched in order.
pub fn plan_patch(ops: &[PatchOp], fs: &dyn FileMap) -> Result<PatchPlan, PlanError> {
    let mut files: Vec<PlannedFile> = Vec::with_capacity(ops.len());

    // Track paths consumed/produced within this patch so we can detect both
    // duplicate targets and collisions that only exist mid-patch (e.g. moving
    // onto a path created earlier in the same patch).
    let mut existing: ExistsView = ExistsView::new(fs);

    for op in ops {
        match op {
            PatchOp::Add { path, content } => {
                if existing.exists(path) {
                    return Err(PlanError::FileExists { path: path.clone() });
                }
                existing.claim_create(path);
                let added = count_lines(content);
                files.push(PlannedFile {
                    path: path.clone(),
                    op: PlannedKind::Add,
                    new_content: Some(content.clone()),
                    added,
                    removed: 0,
                });
            }

            PatchOp::Delete { path } => {
                if !existing.exists(path) {
                    return Err(PlanError::FileNotFound { path: path.clone() });
                }
                let removed = existing
                    .read(path)
                    .as_deref()
                    .map(count_lines)
                    .unwrap_or(0);
                existing.claim_delete(path);
                files.push(PlannedFile {
                    path: path.clone(),
                    op: PlannedKind::Delete,
                    new_content: None,
                    added: 0,
                    removed,
                });
            }

            PatchOp::Update {
                path,
                move_to,
                hunks,
            } => {
                let current = match existing.read(path) {
                    Some(c) => c,
                    None => return Err(PlanError::FileNotFound { path: path.clone() }),
                };

                let applied = apply_hunks(path, &current, hunks)?;

                // If renaming, the destination must be free (in fs and in the
                // mid-patch view), and must differ from the source.
                let planned_kind = match move_to {
                    Some(dest) => {
                        if dest == path {
                            // A no-op move: treat as a plain in-place update.
                            PlannedKind::Update
                        } else {
                            if existing.exists(dest) {
                                return Err(PlanError::FileExists { path: dest.clone() });
                            }
                            existing.claim_create(dest);
                            PlannedKind::Move { to: dest.clone() }
                        }
                    }
                    None => PlannedKind::Update,
                };

                // The source path is consumed when moved away.
                if let PlannedKind::Move { .. } = planned_kind {
                    existing.claim_delete(path);
                } else {
                    // In-place update: content changes but path stays present.
                    existing.set(path, applied.content.clone());
                }

                files.push(PlannedFile {
                    path: path.clone(),
                    op: planned_kind,
                    new_content: Some(applied.content),
                    added: applied.added,
                    removed: applied.removed,
                });
            }
        }
    }

    Ok(PatchPlan { files })
}

/// Outcome of applying all hunks of one `Update`.
struct AppliedUpdate {
    content: String,
    added: usize,
    removed: usize,
}

/// A single scheduled edit: replace `old_len` lines starting at `start` with
/// `new_lines`. Mirrors Codex's `(start_index, old_len, new_lines)` triple.
struct Replacement {
    start: usize,
    old_len: usize,
    new_lines: Vec<String>,
}

/// Apply every hunk of an `Update` to `current`, in order, returning the new
/// content and counts. Errors if any hunk fails to match.
///
/// This mirrors the canonical Codex algorithm
/// (`compute_replacements` + `apply_replacements` +
/// `derive_new_contents_from_chunks`): we compute the set of replacements
/// (locating each hunk forward from a running cursor, honoring `@@` headers and
/// the EOF anchor, with the trailing-empty-line retry), then apply them in
/// descending index order so earlier edits do not shift later ones.
fn apply_hunks(path: &str, current: &str, hunks: &[Hunk]) -> Result<AppliedUpdate, PlanError> {
    // Split into lines exactly as Codex does: split on '\n' and drop the trailing
    // empty element produced by a final newline, so line counts match `diff` and
    // an EOF-anchored search lands correctly.
    let mut original_lines: Vec<String> = current.split('\n').map(String::from).collect();
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }

    let mut replacements: Vec<Replacement> = Vec::new();
    let mut cursor = 0usize; // line index to continue searching from
    let mut added = 0usize;
    let mut removed = 0usize;

    for (hunk_index, hunk) in hunks.iter().enumerate() {
        // 1. Seek each stacked `@@` context header forward from the cursor and
        //    advance past it. This disambiguates a block that repeats and mirrors
        //    Codex's lib.rs (which seeks the single context line then sets
        //    line_index = idx + 1). Multiple headers are sought in order.
        for header in &hunk.context_headers {
            let want = vec![header.clone()];
            match seek_sequence(&original_lines, &want, cursor, /*eof*/ false) {
                Some(idx) => cursor = idx + 1,
                None => {
                    return Err(PlanError::HunkNoMatch {
                        path: path.to_string(),
                        hunk_index,
                        detail: format!("could not find context header {header:?}"),
                    });
                }
            }
        }

        // 2. Build the old side (context + removed, in order) and the new side
        //    (context + added, in order), tracking per-line add/remove counts.
        let old_lines: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Removed(s) => Some(s.clone()),
                HunkLine::Added(_) => None,
            })
            .collect();
        let new_lines: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Added(s) => Some(s.clone()),
                HunkLine::Removed(_) => None,
            })
            .collect();
        let hunk_added = hunk
            .lines
            .iter()
            .filter(|l| matches!(l, HunkLine::Added(_)))
            .count();
        let hunk_removed = hunk
            .lines
            .iter()
            .filter(|l| matches!(l, HunkLine::Removed(_)))
            .count();

        if old_lines.is_empty() {
            // Pure addition (no old anchor lines).
            //
            // * If a `@@` header pinned a location, insert right after it (this is
            //   our improvement over Codex, which always inserts at EOF; the
            //   header already advanced `cursor`). This makes additions land where
            //   the patch indicated.
            // * Otherwise insert at end-of-file, just before a trailing empty line
            //   if one exists — exactly Codex's behavior.
            //
            // Either way the additions must exist somewhere; an anchorless,
            // header-less, EOF-less, empty-side hunk against content with no
            // insertion target is only ambiguous if there is genuinely nowhere to
            // put it. We always have a target (the file end), so this never fails.
            let insertion_idx = if !hunk.context_headers.is_empty() {
                cursor
            } else if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            // A genuinely empty hunk (no headers, no body, not even an EOF marker)
            // that targets a non-empty file with no pinned location is rejected as
            // ambiguous, preserving our stricter contract.
            if hunk.lines.is_empty()
                && hunk.context_headers.is_empty()
                && !hunk.is_end_of_file
                && !original_lines.is_empty()
            {
                return Err(PlanError::EmptyHunk {
                    path: path.to_string(),
                    hunk_index,
                });
            }
            added += hunk_added;
            replacements.push(Replacement {
                start: insertion_idx,
                old_len: 0,
                new_lines,
            });
            // Only a header-pinned pure addition advances the cursor (so a
            // following hunk continues after the inserted block). A header-less
            // addition is scheduled at EOF and must NOT move the cursor — Codex
            // leaves `line_index` untouched here, and the sort + reverse-apply
            // reorders the EOF insertion relative to later replacements.
            if !hunk.context_headers.is_empty() {
                cursor = insertion_idx;
            }
            continue;
        }

        // 3. Locate the old block. `is_end_of_file` makes the search try the EOF
        //    position first. If the block ends with an empty sentinel line (the
        //    file's terminating newline) and the search fails, retry without it,
        //    dropping the matching trailing empty from the new side too. This is
        //    Codex's lib.rs trailing-empty-line retry.
        let mut pattern: &[String] = &old_lines;
        let mut new_slice: Vec<String> = new_lines.clone();
        let mut found = seek_sequence(&original_lines, pattern, cursor, hunk.is_end_of_file);

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice.pop();
            }
            found = seek_sequence(&original_lines, pattern, cursor, hunk.is_end_of_file);
        }

        let start_idx = found.ok_or_else(|| PlanError::HunkNoMatch {
            path: path.to_string(),
            hunk_index,
            detail: format!(
                "could not locate the {} context/removed line(s) starting near line {}",
                old_lines.len(),
                cursor + 1
            ),
        })?;

        added += hunk_added;
        removed += hunk_removed;
        replacements.push(Replacement {
            start: start_idx,
            old_len: pattern.len(),
            new_lines: new_slice,
        });
        cursor = start_idx + pattern.len();
    }

    // Apply replacements in descending start order (Codex sorts ascending then
    // iterates in reverse) so earlier edits don't shift later indices.
    replacements.sort_by_key(|r| r.start);
    let mut out = original_lines;
    for r in replacements.iter().rev() {
        for _ in 0..r.old_len {
            if r.start < out.len() {
                out.remove(r.start);
            }
        }
        for (offset, line) in r.new_lines.iter().enumerate() {
            out.insert(r.start + offset, line.clone());
        }
    }

    // Trailing-newline normalization (Codex): ensure the file ends with a
    // newline by appending an empty final line unless one is already present,
    // then join with '\n'. See the module-level "Trailing-newline policy" note.
    if !out.last().is_some_and(String::is_empty) {
        out.push(String::new());
    }
    let content = out.join("\n");

    Ok(AppliedUpdate {
        content,
        added,
        removed,
    })
}

/// Locate the sequence `pattern` within `lines` at or after `start`, returning
/// the starting index or `None`. Ported from Codex `seek_sequence`.
///
/// Matching escalates through tiers of decreasing strictness so real model
/// patches that drift on whitespace or smart punctuation still locate cleanly:
///   1. exact equality,
///   2. ignore trailing whitespace (`trim_end`),
///   3. ignore leading *and* trailing whitespace (`trim`),
///   4. Unicode-punctuation normalization (typographic dashes / quotes /
///      non-breaking and exotic spaces folded to their ASCII equivalents).
///
/// When `eof` is true the search is anchored to the file's tail: only the final
/// candidate position (`lines.len() - pattern.len()`) is probed — through every
/// tier — so a hunk marked end-of-file matches the file's end and nowhere else.
/// There is no fallback to a forward scan in that case. Otherwise the scan runs
/// forward from `start`. (This mirrors Codex's `seek_sequence`.)
///
/// Defensive cases: an empty `pattern` returns `Some(start)`; a `pattern` longer
/// than `lines` returns `None` (no panic).
fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof && lines.len() >= pattern.len() {
        lines.len() - pattern.len()
    } else {
        start
    };
    let last = lines.len().saturating_sub(pattern.len());
    let window = || search_start..=last;

    // Tier 1: exact match.
    if let Some(i) = window().find(|&i| lines[i..i + pattern.len()] == *pattern) {
        return Some(i);
    }
    // Tier 2: ignore trailing whitespace.
    if let Some(i) = window()
        .find(|&i| (0..pattern.len()).all(|k| lines[i + k].trim_end() == pattern[k].trim_end()))
    {
        return Some(i);
    }
    // Tier 3: ignore leading and trailing whitespace.
    if let Some(i) =
        window().find(|&i| (0..pattern.len()).all(|k| lines[i + k].trim() == pattern[k].trim()))
    {
        return Some(i);
    }
    // Tier 4: Unicode-punctuation normalization (typographic dashes/quotes/odd
    // spaces folded to ASCII), mirroring `git apply`'s tolerance.
    window().find(|&i| {
        (0..pattern.len()).all(|k| normalise(&lines[i + k]) == normalise(&pattern[k]))
    })
}

/// Fold common Unicode punctuation to ASCII so ASCII-authored diffs can match
/// source lines that contain typographic characters. Also `trim`s.
fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            // Various dash / hyphen code-points → ASCII '-'.
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            // Fancy single quotes → '\''.
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            // Fancy double quotes → '"'.
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            // Non-breaking and other odd spaces → normal space.
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// Count the number of lines a piece of content contributes. Empty string → 0
/// lines; otherwise the number of `\n`-separated segments (a trailing newline
/// does not add a phantom empty line).
fn count_lines(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    let body = content.strip_suffix('\n').unwrap_or(content);
    body.split('\n').count()
}

/// A mutable "does this path exist?" view layered over a borrowed [`FileMap`].
///
/// The base `FileMap` is immutable; this overlay records creates/deletes/edits
/// that happen *within* the patch so later ops in the same patch see a coherent
/// world (duplicate targets, moving onto a just-created file, etc.).
struct ExistsView<'a> {
    base: &'a dyn FileMap,
    /// Overlay: `Some(content)` = present (possibly edited), `None` = deleted.
    /// Absent from the map = "defer to base".
    overlay: HashMap<String, Option<String>>,
}

impl<'a> ExistsView<'a> {
    fn new(base: &'a dyn FileMap) -> Self {
        Self {
            base,
            overlay: HashMap::new(),
        }
    }

    fn exists(&self, path: &str) -> bool {
        match self.overlay.get(path) {
            Some(Some(_)) => true,
            Some(None) => false,
            None => self.base.exists(path),
        }
    }

    fn read(&self, path: &str) -> Option<String> {
        match self.overlay.get(path) {
            Some(slot) => slot.clone(),
            None => self.base.read(path),
        }
    }

    fn claim_create(&mut self, path: &str) {
        // Newly created files have no content we need to track for later reads
        // within the same patch beyond "exists"; store an empty marker. If a
        // later op reads it, it would read "" — acceptable for the create case.
        self.overlay.insert(path.to_string(), Some(String::new()));
    }

    fn claim_delete(&mut self, path: &str) {
        self.overlay.insert(path.to_string(), None);
    }

    fn set(&mut self, path: &str, content: String) {
        self.overlay.insert(path.to_string(), Some(content));
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- heredoc envelope --------------------------------------------------

    #[test]
    fn plain_patch_without_heredoc_still_parses() {
        let ops = parse_v4a("*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch\n").unwrap();
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn quoted_heredoc_wrapper_is_stripped() {
        let single =
            "<<'EOF'\n*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch\nEOF\n";
        assert_eq!(parse_v4a(single).unwrap().len(), 1);

        let double =
            "<<\"EOF\"\n*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch\nEOF\n";
        assert_eq!(parse_v4a(double).unwrap().len(), 1);
    }

    #[test]
    fn bare_and_command_heredoc_wrappers_are_stripped() {
        // `<<EOF` (no quotes)
        let bare = "<<EOF\n*** Begin Patch\n*** Delete File: x.txt\n*** End Patch\nEOF\n";
        assert_eq!(parse_v4a(bare).unwrap().len(), 1);

        // `apply_patch <<'PATCH'` — Codex CLI form with a custom delimiter.
        let cmd = "apply_patch <<'PATCH'\n*** Begin Patch\n*** Delete File: x.txt\n*** End Patch\nPATCH\n";
        assert_eq!(parse_v4a(cmd).unwrap().len(), 1);
    }

    #[test]
    fn mismatched_heredoc_is_left_as_parse_error() {
        // Opener present but no terminator line equal to the delimiter → not
        // stripped → first line is not `*** Begin Patch` → MissingBegin. We never
        // guess at a broken wrapper.
        let patch =
            "<<'EOF'\n*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch\nWRONGDELIM\n";
        assert!(matches!(parse_v4a(patch), Err(PatchError::MissingBegin)));
    }

    #[test]
    fn heredoc_with_environment_id_still_parses() {
        let patch = "apply_patch <<'EOF'\n*** Begin Patch\n*** Environment ID: abc123\n*** Add File: a.txt\n+x\n*** End Patch\nEOF\n";
        assert_eq!(parse_v4a(patch).unwrap().len(), 1);
    }

    #[test]
    fn heredoc_terminator_is_not_confused_with_indented_content() {
        // A context/added line that merely contains the delimiter word is not a
        // terminator (content lines are prefixed / indented, never column 0).
        let patch = "<<EOF\n*** Begin Patch\n*** Add File: s.sh\n+echo EOF\n+ EOF\n*** End Patch\nEOF\n";
        let ops = parse_v4a(patch).unwrap();
        assert_eq!(ops.len(), 1);
    }

    // ---- helpers ----------------------------------------------------------

    fn fm(pairs: &[(&str, &str)]) -> InMemoryFileMap {
        let mut m = InMemoryFileMap::new();
        for (p, c) in pairs {
            m.insert(*p, *c);
        }
        m
    }

    /// Convenience: parse + plan a single-file update and return its new content.
    fn plan_one(patch: &str, files: &[(&str, &str)]) -> Result<PatchPlan, PlanError> {
        let ops = parse_v4a(patch).expect("parse");
        plan_patch(&ops, &fm(files))
    }

    // ---- parse: add -------------------------------------------------------

    #[test]
    fn parse_add_file() {
        let patch = "\
*** Begin Patch
*** Add File: src/hello.rs
+fn main() {
+    println!(\"hi\");
+}
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        assert_eq!(
            ops,
            vec![PatchOp::Add {
                path: "src/hello.rs".to_string(),
                content: "fn main() {\n    println!(\"hi\");\n}".to_string(),
            }]
        );
    }

    #[test]
    fn parse_add_empty_file() {
        let patch = "*** Begin Patch\n*** Add File: empty.txt\n*** End Patch\n";
        let ops = parse_v4a(patch).expect("parse");
        assert_eq!(
            ops,
            vec![PatchOp::Add {
                path: "empty.txt".to_string(),
                content: String::new(),
            }]
        );
    }

    // ---- parse: delete ----------------------------------------------------

    #[test]
    fn parse_delete_file() {
        let patch = "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch\n";
        let ops = parse_v4a(patch).expect("parse");
        assert_eq!(
            ops,
            vec![PatchOp::Delete {
                path: "old.txt".to_string()
            }]
        );
    }

    // ---- parse: update ----------------------------------------------------

    #[test]
    fn parse_update_single_hunk() {
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@ fn main
 keep
-old
+new
 tail
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        assert_eq!(
            ops,
            vec![PatchOp::Update {
                path: "a.txt".to_string(),
                move_to: None,
                hunks: vec![Hunk {
                    context_headers: vec!["fn main".to_string()],
                    lines: vec![
                        HunkLine::Context("keep".to_string()),
                        HunkLine::Removed("old".to_string()),
                        HunkLine::Added("new".to_string()),
                        HunkLine::Context("tail".to_string()),
                    ],
                    is_end_of_file: false,
                }],
            }]
        );
    }

    #[test]
    fn parse_update_with_move() {
        let patch = "\
*** Begin Patch
*** Update File: a.txt
*** Move to: b.txt
@@
-x
+y
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        assert_eq!(
            ops,
            vec![PatchOp::Update {
                path: "a.txt".to_string(),
                move_to: Some("b.txt".to_string()),
                hunks: vec![Hunk {
                    context_headers: vec![],
                    lines: vec![
                        HunkLine::Removed("x".to_string()),
                        HunkLine::Added("y".to_string()),
                    ],
                    is_end_of_file: false,
                }],
            }]
        );
    }

    #[test]
    fn parse_update_multi_hunk() {
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@ first
 a
-b
+B
@@ second
 d
-e
+E
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        let PatchOp::Update { hunks, .. } = &ops[0] else {
            panic!("expected update");
        };
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].context_header(), Some("first"));
        assert_eq!(hunks[1].context_header(), Some("second"));
    }

    #[test]
    fn parse_update_header_less_leading_hunk() {
        // Body lines with no preceding @@ form a single header-less hunk.
        let patch = "\
*** Begin Patch
*** Update File: a.txt
 ctx
-gone
+here
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        let PatchOp::Update { hunks, .. } = &ops[0] else {
            panic!("expected update");
        };
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].context_headers.is_empty());
        assert_eq!(hunks[0].lines.len(), 3);
    }

    #[test]
    fn parse_update_stacked_context_headers() {
        // Multiple @@ lines with no body between them stack onto one hunk.
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@ class BaseClass
@@     def method():
 ctx
-old
+new
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        let PatchOp::Update { hunks, .. } = &ops[0] else {
            panic!("expected update");
        };
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0].context_headers,
            vec!["class BaseClass".to_string(), "def method():".to_string()]
        );
    }

    #[test]
    fn parse_end_of_file_marker() {
        // `*** End of File` must be recognized (was previously hard-rejected) and
        // set is_end_of_file on the hunk.
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
 last
+appended
*** End of File
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        let PatchOp::Update { hunks, .. } = &ops[0] else {
            panic!("expected update");
        };
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].is_end_of_file);
        assert_eq!(
            hunks[0].lines,
            vec![
                HunkLine::Context("last".to_string()),
                HunkLine::Added("appended".to_string()),
            ]
        );
    }

    // ---- parse: multi-file ------------------------------------------------

    #[test]
    fn parse_multi_file() {
        let patch = "\
*** Begin Patch
*** Add File: new.txt
+hello
*** Update File: mid.txt
@@
-a
+b
*** Delete File: gone.txt
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[0], PatchOp::Add { .. }));
        assert!(matches!(ops[1], PatchOp::Update { .. }));
        assert!(matches!(ops[2], PatchOp::Delete { .. }));
    }

    // ---- parse: CRLF ------------------------------------------------------

    #[test]
    fn parse_crlf_tolerant() {
        let patch =
            "*** Begin Patch\r\n*** Add File: c.txt\r\n+line1\r\n+line2\r\n*** End Patch\r\n";
        let ops = parse_v4a(patch).expect("parse");
        assert_eq!(
            ops,
            vec![PatchOp::Add {
                path: "c.txt".to_string(),
                content: "line1\nline2".to_string(),
            }]
        );
    }

    #[test]
    fn parse_trailing_whitespace_on_markers() {
        // Trailing spaces after markers and after the path are tolerated.
        let patch = "*** Begin Patch   \n*** Delete File: x.txt  \n*** End Patch  \n";
        let ops = parse_v4a(patch).expect("parse");
        assert_eq!(
            ops,
            vec![PatchOp::Delete {
                path: "x.txt".to_string()
            }]
        );
    }

    // ---- parse: tolerance (Environment ID, no-newline) --------------------

    #[test]
    fn parse_environment_id_preamble_is_ignored() {
        let patch = "\
*** Begin Patch
*** Environment ID: remote-123
*** Add File: hello.txt
+hi
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        assert_eq!(
            ops,
            vec![PatchOp::Add {
                path: "hello.txt".to_string(),
                content: "hi".to_string(),
            }]
        );
    }

    #[test]
    fn parse_empty_environment_id_is_error() {
        let patch = "*** Begin Patch\n*** Environment ID:   \n*** End Patch\n";
        match parse_v4a(patch) {
            Err(PatchError::EmptyEnvironmentId { line }) => assert_eq!(line, 2),
            other => panic!("expected EmptyEnvironmentId, got {other:?}"),
        }
    }

    #[test]
    fn parse_tolerates_no_newline_marker_in_hunk() {
        // git-style "\ No newline at end of file" lines are skipped inside hunks.
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-old
\\ No newline at end of file
+new
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        let PatchOp::Update { hunks, .. } = &ops[0] else {
            panic!("expected update");
        };
        assert_eq!(
            hunks[0].lines,
            vec![
                HunkLine::Removed("old".to_string()),
                HunkLine::Added("new".to_string()),
            ]
        );
    }

    #[test]
    fn parse_tolerates_no_newline_marker_in_add_body() {
        let patch = "\
*** Begin Patch
*** Add File: a.txt
+only line
\\ No newline at end of file
*** End Patch
";
        let ops = parse_v4a(patch).expect("parse");
        assert_eq!(
            ops,
            vec![PatchOp::Add {
                path: "a.txt".to_string(),
                content: "only line".to_string(),
            }]
        );
    }

    // ---- parse: malformed -------------------------------------------------

    #[test]
    fn parse_missing_begin() {
        let patch = "*** Add File: a.txt\n+x\n*** End Patch\n";
        assert_eq!(parse_v4a(patch), Err(PatchError::MissingBegin));
    }

    #[test]
    fn parse_missing_end() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+x\n";
        assert_eq!(parse_v4a(patch), Err(PatchError::MissingEnd));
    }

    #[test]
    fn parse_empty_path() {
        let patch = "*** Begin Patch\n*** Add File:\n+x\n*** End Patch\n";
        match parse_v4a(patch) {
            Err(PatchError::EmptyPath { line, marker }) => {
                assert_eq!(line, 2);
                assert_eq!(marker, "*** Add File:");
            }
            other => panic!("expected EmptyPath, got {other:?}"),
        }
    }

    #[test]
    fn parse_bad_add_body_line() {
        // A non-`+` (and non-empty) line inside an Add section is an error,
        // reported with its line number.
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+ok\nNOT-PLUS\n*** End Patch\n";
        match parse_v4a(patch) {
            Err(PatchError::Unexpected { line, .. }) => assert_eq!(line, 4),
            other => panic!("expected Unexpected at line 4, got {other:?}"),
        }
    }

    #[test]
    fn parse_bad_hunk_line() {
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\nBADLINE\n*** End Patch\n";
        match parse_v4a(patch) {
            Err(PatchError::BadHunkLine { line, .. }) => assert_eq!(line, 4),
            other => panic!("expected BadHunkLine at line 4, got {other:?}"),
        }
    }

    #[test]
    fn parse_misplaced_move_to() {
        // Move-to at the top level (not under an Update) is rejected.
        let patch = "*** Begin Patch\n*** Move to: b.txt\n*** End Patch\n";
        match parse_v4a(patch) {
            Err(PatchError::MisplacedMoveTo { line }) => assert_eq!(line, 2),
            other => panic!("expected MisplacedMoveTo, got {other:?}"),
        }
    }

    #[test]
    fn parse_double_move_to() {
        let patch = "\
*** Begin Patch
*** Update File: a.txt
*** Move to: b.txt
*** Move to: c.txt
*** End Patch
";
        match parse_v4a(patch) {
            Err(PatchError::MisplacedMoveTo { line }) => assert_eq!(line, 4),
            other => panic!("expected MisplacedMoveTo at line 4, got {other:?}"),
        }
    }

    #[test]
    fn parse_content_after_end() {
        let patch = "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch\ngarbage\n";
        match parse_v4a(patch) {
            Err(PatchError::Structure { line, .. }) => assert_eq!(line, 4),
            other => panic!("expected Structure error, got {other:?}"),
        }
    }

    // ---- plan: success ----------------------------------------------------

    #[test]
    fn plan_update_hunk_matches() {
        let plan = plan_one(
            "\
*** Begin Patch
*** Update File: a.txt
@@
 keep
-old
+new
 tail
*** End Patch
",
            &[("a.txt", "keep\nold\ntail\n")],
        )
        .expect("plan");
        assert_eq!(plan.files.len(), 1);
        let f = &plan.files[0];
        assert_eq!(f.path, "a.txt");
        assert_eq!(f.op, PlannedKind::Update);
        assert_eq!(f.new_content.as_deref(), Some("keep\nnew\ntail\n"));
        assert_eq!(f.added, 1);
        assert_eq!(f.removed, 1);
    }

    #[test]
    fn plan_update_multi_hunk_in_order() {
        let plan = plan_one(
            "\
*** Begin Patch
*** Update File: a.txt
@@
 a
-b
+B
@@
 e
-f
+F
*** End Patch
",
            &[("a.txt", "a\nb\nc\nd\ne\nf\n")],
        )
        .expect("plan");
        let f = &plan.files[0];
        assert_eq!(f.new_content.as_deref(), Some("a\nB\nc\nd\ne\nF\n"));
        assert_eq!(f.added, 2);
        assert_eq!(f.removed, 2);
    }

    #[test]
    fn plan_add_new_file() {
        let fs = fm(&[]);
        let ops = parse_v4a("*** Begin Patch\n*** Add File: n.txt\n+one\n+two\n*** End Patch\n")
            .unwrap();
        let plan = plan_patch(&ops, &fs).expect("plan");
        let f = &plan.files[0];
        assert_eq!(f.op, PlannedKind::Add);
        assert_eq!(f.new_content.as_deref(), Some("one\ntwo"));
        assert_eq!(f.added, 2);
        assert_eq!(f.removed, 0);
    }

    #[test]
    fn plan_delete_existing() {
        let fs = fm(&[("d.txt", "x\ny\n")]);
        let ops = parse_v4a("*** Begin Patch\n*** Delete File: d.txt\n*** End Patch\n").unwrap();
        let plan = plan_patch(&ops, &fs).expect("plan");
        let f = &plan.files[0];
        assert_eq!(f.op, PlannedKind::Delete);
        assert_eq!(f.new_content, None);
        assert_eq!(f.removed, 2);
    }

    #[test]
    fn plan_move_renames_and_edits() {
        let plan = plan_one(
            "\
*** Begin Patch
*** Update File: a.txt
*** Move to: b.txt
@@
-x
+y
*** End Patch
",
            &[("a.txt", "x\n")],
        )
        .expect("plan");
        let f = &plan.files[0];
        assert_eq!(f.path, "a.txt");
        assert_eq!(f.op, PlannedKind::Move { to: "b.txt".to_string() });
        assert_eq!(f.new_content.as_deref(), Some("y\n"));
        assert_eq!(f.added, 1);
        assert_eq!(f.removed, 1);
    }

    #[test]
    fn plan_add_to_empty_file_via_anchorless_hunk() {
        // Updating an empty file with an additions-only hunk appends content.
        // Codex's trailing-newline policy makes this newline-terminated.
        let plan = plan_one(
            "*** Begin Patch\n*** Update File: e.txt\n@@\n+first\n+second\n*** End Patch\n",
            &[("e.txt", "")],
        )
        .expect("plan");
        assert_eq!(plan.files[0].new_content.as_deref(), Some("first\nsecond\n"));
        assert_eq!(plan.files[0].added, 2);
    }

    #[test]
    fn plan_trailing_newline_added_to_match_codex() {
        // Source has no trailing newline; per Codex policy the *updated* result
        // gains one. (Documented in the module-level trailing-newline note.)
        let plan = plan_one(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n keep\n-old\n+new\n*** End Patch\n",
            &[("a.txt", "keep\nold")],
        )
        .expect("plan");
        assert_eq!(plan.files[0].new_content.as_deref(), Some("keep\nnew\n"));
    }

    // ---- plan: EOF anchoring + pure-addition (ported from codex) ----------

    #[test]
    fn plan_insert_at_eof_marker() {
        // Ported from codex test_unified_diff_insert_at_eof: a `+`-only hunk
        // terminated by `*** End of File` appends at end-of-file.
        let plan = plan_one(
            "\
*** Begin Patch
*** Update File: insert.txt
@@
+quux
*** End of File
*** End Patch
",
            &[("insert.txt", "foo\nbar\nbaz\n")],
        )
        .expect("plan");
        assert_eq!(
            plan.files[0].new_content.as_deref(),
            Some("foo\nbar\nbaz\nquux\n")
        );
        assert_eq!(plan.files[0].added, 1);
    }

    #[test]
    fn plan_interleaved_changes_with_eof_append() {
        // Ported from codex test_update_file_hunk_interleaved_changes.
        let plan = plan_one(
            "\
*** Begin Patch
*** Update File: interleaved.txt
@@
 a
-b
+B
@@
 c
 d
-e
+E
@@
 f
+g
*** End of File
*** End Patch
",
            &[("interleaved.txt", "a\nb\nc\nd\ne\nf\n")],
        )
        .expect("plan");
        assert_eq!(
            plan.files[0].new_content.as_deref(),
            Some("a\nB\nc\nd\nE\nf\ng\n")
        );
    }

    #[test]
    fn plan_pure_addition_chunk_followed_by_removal() {
        // Ported from codex test_pure_addition_chunk_followed_by_removal: a
        // header-less pure-addition hunk is scheduled at EOF, then the sort +
        // reverse-apply lands it after the replacement of the earlier block.
        let plan = plan_one(
            "\
*** Begin Patch
*** Update File: panic.txt
@@
+after-context
+second-line
@@
 line1
-line2
-line3
+line2-replacement
*** End Patch
",
            &[("panic.txt", "line1\nline2\nline3\n")],
        )
        .expect("plan");
        assert_eq!(
            plan.files[0].new_content.as_deref(),
            Some("line1\nline2-replacement\nafter-context\nsecond-line\n")
        );
    }

    #[test]
    fn plan_pure_addition_on_nonempty_file_with_header_inserts_there() {
        // Requirement #4: a pure-addition hunk whose old side is empty but which
        // is pinned by a `@@` header inserts right after the located header line
        // (our improvement over Codex's EOF-only insertion).
        let plan = plan_one(
            "\
*** Begin Patch
*** Update File: a.txt
@@ anchor
+inserted
*** End Patch
",
            &[("a.txt", "top\nanchor\nbottom\n")],
        )
        .expect("plan");
        assert_eq!(
            plan.files[0].new_content.as_deref(),
            Some("top\nanchor\ninserted\nbottom\n")
        );
        assert_eq!(plan.files[0].added, 1);
    }

    // ---- plan: fuzzy / whitespace / unicode (ported from codex) -----------

    #[test]
    fn plan_fuzzy_trailing_whitespace_drift() {
        // The file line has trailing whitespace the patch omits; tier-2 rstrip
        // matching locates it.
        let plan = plan_one(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-foo\n+bar\n*** End Patch\n",
            &[("a.txt", "foo   \n")],
        )
        .expect("plan");
        assert_eq!(plan.files[0].new_content.as_deref(), Some("bar\n"));
    }

    #[test]
    fn plan_fuzzy_leading_whitespace_drift() {
        // The file line is indented; the patch's removed line is not. Tier-3 trim
        // matching locates it.
        let plan = plan_one(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-foo\n+bar\n*** End Patch\n",
            &[("a.txt", "    foo\n")],
        )
        .expect("plan");
        assert_eq!(plan.files[0].new_content.as_deref(), Some("bar\n"));
    }

    #[test]
    fn plan_fuzzy_unicode_dash_normalization() {
        // Ported from codex test_update_line_with_unicode_dash. File contains EN
        // DASH (U+2013) and NON-BREAKING HYPHEN (U+2011); patch uses ASCII.
        let original = "import asyncio  # local import \u{2013} avoids top\u{2011}level dep\n";
        let plan = plan_one(
            "\
*** Begin Patch
*** Update File: unicode.py
@@
-import asyncio  # local import - avoids top-level dep
+import asyncio  # HELLO
*** End Patch
",
            &[("unicode.py", original)],
        )
        .expect("plan");
        assert_eq!(
            plan.files[0].new_content.as_deref(),
            Some("import asyncio  # HELLO\n")
        );
    }

    // ---- plan: repeated-block disambiguation via @@ -----------------------

    #[test]
    fn plan_repeated_block_disambiguated_by_header() {
        // The block `value = 1` appears twice. Without the `@@ fn second` header
        // a forward search would hit the first occurrence; the header advances the
        // cursor so the SECOND block is the one edited.
        let file = "\
fn first() {
    value = 1
}
fn second() {
    value = 1
}
";
        let plan = plan_one(
            "\
*** Begin Patch
*** Update File: a.txt
@@ fn second() {
     value = 1
-}
+    extra()
+}
*** End Patch
",
            &[("a.txt", file)],
        )
        .expect("plan");
        assert_eq!(
            plan.files[0].new_content.as_deref(),
            Some(
                "fn first() {\n    value = 1\n}\nfn second() {\n    value = 1\n    extra()\n}\n"
            )
        );
    }

    #[test]
    fn plan_missing_context_header_is_no_match() {
        // A `@@` header that does not occur in the file fails to locate.
        let res = plan_one(
            "\
*** Begin Patch
*** Update File: a.txt
@@ nonexistent header
 a
-b
+B
*** End Patch
",
            &[("a.txt", "a\nb\nc\n")],
        );
        match res {
            Err(PlanError::HunkNoMatch { hunk_index, .. }) => assert_eq!(hunk_index, 0),
            other => panic!("expected HunkNoMatch, got {other:?}"),
        }
    }

    // ---- plan: trailing-empty-line retry ----------------------------------

    #[test]
    fn plan_trailing_empty_line_retry() {
        // The old side ends with an empty line representing the file's final
        // newline. Since `original_lines` drops that sentinel, the first search
        // fails; the retry without the trailing empty locates the block.
        let plan = plan_one(
            "\
*** Begin Patch
*** Update File: a.txt
@@
 foo
-bar
+baz

*** End Patch
",
            &[("a.txt", "foo\nbar\n")],
        )
        .expect("plan");
        assert_eq!(plan.files[0].new_content.as_deref(), Some("foo\nbaz\n"));
    }

    // ---- plan: failures (all-or-none) ------------------------------------

    #[test]
    fn plan_hunk_no_match_aborts_whole_plan() {
        // Second op would succeed, but the first op's hunk does not match, so
        // the WHOLE plan must fail with no partial result.
        let res = plan_one(
            "\
*** Begin Patch
*** Update File: a.txt
@@
 keep
-old
+new
*** Delete File: ok.txt
*** End Patch
",
            &[("a.txt", "totally\ndifferent\n"), ("ok.txt", "z\n")],
        );
        match res {
            Err(PlanError::HunkNoMatch {
                path, hunk_index, ..
            }) => {
                assert_eq!(path, "a.txt");
                assert_eq!(hunk_index, 0);
            }
            other => panic!("expected HunkNoMatch, got {other:?}"),
        }
    }

    #[test]
    fn plan_add_over_existing_is_file_exists() {
        let fs = fm(&[("a.txt", "already\n")]);
        let ops =
            parse_v4a("*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch\n").unwrap();
        assert_eq!(
            plan_patch(&ops, &fs),
            Err(PlanError::FileExists {
                path: "a.txt".to_string()
            })
        );
    }

    #[test]
    fn plan_delete_missing_is_file_not_found() {
        let fs = fm(&[]);
        let ops =
            parse_v4a("*** Begin Patch\n*** Delete File: nope.txt\n*** End Patch\n").unwrap();
        assert_eq!(
            plan_patch(&ops, &fs),
            Err(PlanError::FileNotFound {
                path: "nope.txt".to_string()
            })
        );
    }

    #[test]
    fn plan_update_missing_is_file_not_found() {
        let fs = fm(&[]);
        let ops = parse_v4a(
            "*** Begin Patch\n*** Update File: nope.txt\n@@\n-a\n+b\n*** End Patch\n",
        )
        .unwrap();
        assert_eq!(
            plan_patch(&ops, &fs),
            Err(PlanError::FileNotFound {
                path: "nope.txt".to_string()
            })
        );
    }

    #[test]
    fn plan_move_onto_existing_is_file_exists() {
        let res = plan_one(
            "\
*** Begin Patch
*** Update File: a.txt
*** Move to: b.txt
@@
-x
+y
*** End Patch
",
            &[("a.txt", "x\n"), ("b.txt", "occupied\n")],
        );
        assert_eq!(
            res,
            Err(PlanError::FileExists {
                path: "b.txt".to_string()
            })
        );
    }

    #[test]
    fn plan_second_hunk_no_match_reports_index_one() {
        let res = plan_one(
            "\
*** Begin Patch
*** Update File: a.txt
@@
 a
-b
+B
@@
 zzz
-nope
+x
*** End Patch
",
            &[("a.txt", "a\nb\nc\n")],
        );
        match res {
            Err(PlanError::HunkNoMatch { hunk_index, .. }) => assert_eq!(hunk_index, 1),
            other => panic!("expected HunkNoMatch index 1, got {other:?}"),
        }
    }

    #[test]
    fn plan_crlf_source_matches_lf_hunk() {
        // Current content uses CRLF; the fuzzy rstrip tier absorbs the trailing
        // '\r' on each stored line so an LF-authored hunk still matches, on both
        // an LF map and a CRLF map.
        let ops = parse_v4a(
            "*** Begin Patch\r\n*** Update File: a.txt\r\n@@\r\n keep\r\n-old\r\n+new\r\n tail\r\n*** End Patch\r\n",
        )
        .unwrap();
        let lf_fs = fm(&[("a.txt", "keep\nold\ntail\n")]);
        let plan = plan_patch(&ops, &lf_fs).expect("plan lf");
        assert_eq!(plan.files[0].new_content.as_deref(), Some("keep\nnew\ntail\n"));

        // CRLF map: the stored lines carry '\r'; matching now succeeds via the
        // rstrip tier (it absorbs the trailing '\r'). Note: any line that passes
        // *through* a hunk as context or added is re-emitted verbatim from the
        // patch text (LF-only), so the '\r' on `keep`/`tail` is dropped here.
        // Only lines that lie entirely OUTSIDE every replaced region keep their
        // original CRLF ending. This matches Codex's replacement model.
        let crlf_fs = fm(&[("a.txt", "keep\r\nold\r\ntail\r\n")]);
        let plan = plan_patch(&ops, &crlf_fs).expect("plan crlf");
        assert_eq!(plan.files[0].new_content.as_deref(), Some("keep\nnew\ntail\n"));
    }

    #[test]
    fn plan_crlf_untouched_lines_keep_their_ending() {
        // A CRLF file where the edited region does NOT cover the surrounding
        // lines: those lines lie outside every replacement and so keep their
        // original '\r\n'. Only the replaced line is rewritten LF-only.
        let ops = parse_v4a(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch\n",
        )
        .unwrap();
        let crlf_fs = fm(&[("a.txt", "head\r\nold\r\ntail\r\n")]);
        let plan = plan_patch(&ops, &crlf_fs).expect("plan crlf");
        assert_eq!(
            plan.files[0].new_content.as_deref(),
            Some("head\r\nnew\ntail\r\n")
        );
    }

    #[test]
    fn plan_duplicate_add_then_add_is_file_exists() {
        // Adding the same path twice in one patch: the second add sees the first
        // as already-created.
        let res = plan_one(
            "\
*** Begin Patch
*** Add File: dup.txt
+one
*** Add File: dup.txt
+two
*** End Patch
",
            &[],
        );
        assert_eq!(
            res,
            Err(PlanError::FileExists {
                path: "dup.txt".to_string()
            })
        );
    }

    #[test]
    fn plan_delete_then_readd_succeeds() {
        // Delete a file, then re-add it in the same patch: legal, since after the
        // delete the path is free.
        let plan = plan_one(
            "\
*** Begin Patch
*** Delete File: a.txt
*** Add File: a.txt
+fresh
*** End Patch
",
            &[("a.txt", "old\n")],
        )
        .expect("plan");
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.files[0].op, PlannedKind::Delete);
        assert_eq!(plan.files[1].op, PlannedKind::Add);
        assert_eq!(plan.files[1].new_content.as_deref(), Some("fresh"));
    }

    // ---- multi-file end-to-end (ported from codex) ------------------------

    #[test]
    fn plan_combined_add_delete_update_move() {
        // Ported in spirit from codex test_apply_patch_hunks_accept_*: a single
        // patch that adds, deletes, updates, and moves+updates.
        let plan = plan_one(
            "\
*** Begin Patch
*** Add File: relative-add.txt
+relative add
*** Delete File: relative-delete.txt
*** Update File: relative-update.txt
@@
-relative old
+relative new
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print(\"Hi\")
+print(\"Hello, world!\")
*** End Patch
",
            &[
                ("relative-delete.txt", "delete relative\n"),
                ("relative-update.txt", "relative old\n"),
                (
                    "src/app.py",
                    "def greet():\n    print(\"Hi\")\nprint(\"Hi\")\n",
                ),
            ],
        )
        .expect("plan");
        assert_eq!(plan.files.len(), 4);
        assert_eq!(plan.files[0].op, PlannedKind::Add);
        assert_eq!(plan.files[0].new_content.as_deref(), Some("relative add"));
        assert_eq!(plan.files[1].op, PlannedKind::Delete);
        assert_eq!(plan.files[2].op, PlannedKind::Update);
        assert_eq!(
            plan.files[2].new_content.as_deref(),
            Some("relative new\n")
        );
        assert_eq!(
            plan.files[3].op,
            PlannedKind::Move {
                to: "src/main.py".to_string()
            }
        );
        // The `@@ def greet():` header advances the cursor past line 0, so the
        // search starts at line 1. The patch's removed line `print("Hi")` has no
        // leading space. `seek_sequence` tries an EXACT match across all positions
        // first, so it locates the bare `print("Hi")` at line 2 (the indented line
        // at line 1 would only match the looser trim tier, which never runs once an
        // exact hit is found). The bare occurrence is thus the one replaced — this
        // is exactly Codex's behavior.
        assert_eq!(
            plan.files[3].new_content.as_deref(),
            Some("def greet():\n    print(\"Hi\")\nprint(\"Hello, world!\")\n")
        );
    }

    // ---- robustness -------------------------------------------------------

    #[test]
    fn parse_does_not_panic_on_utf8_and_odd_input() {
        let weird = "*** Begin Patch\n*** Add File: ☃.txt\n+héllo · wörld\n*** End Patch\n";
        let ops = parse_v4a(weird).expect("parse");
        assert_eq!(
            ops,
            vec![PatchOp::Add {
                path: "☃.txt".to_string(),
                content: "héllo · wörld".to_string(),
            }]
        );
    }

    #[test]
    fn empty_input_is_missing_begin() {
        assert_eq!(parse_v4a(""), Err(PatchError::MissingBegin));
    }

    #[test]
    fn seek_sequence_tiers_and_guards() {
        let lines: Vec<String> = ["foo", "bar", "baz"].iter().map(|s| s.to_string()).collect();
        // exact
        assert_eq!(
            seek_sequence(&lines, &["bar".to_string(), "baz".to_string()], 0, false),
            Some(1)
        );
        // pattern longer than input → None (no panic)
        assert_eq!(
            seek_sequence(
                &["x".to_string()],
                &["a".to_string(), "b".to_string()],
                0,
                false
            ),
            None
        );
        // empty pattern → Some(start)
        assert_eq!(seek_sequence(&lines, &[], 2, false), Some(2));
        // eof flag searches the tail first
        assert_eq!(
            seek_sequence(&lines, &["baz".to_string()], 0, true),
            Some(2)
        );
    }

    #[test]
    fn count_lines_behaves() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("a\nb\n"), 2);
    }
}
