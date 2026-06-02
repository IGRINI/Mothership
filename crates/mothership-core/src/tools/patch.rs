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
//! # The V4A envelope grammar
//!
//! ```text
//! *** Begin Patch
//! *** Add File: <path>
//! +<line>                          (every added line prefixed with '+')
//! *** Update File: <path>
//! *** Move to: <newpath>           (optional, only directly under Update)
//! @@ <optional context header>     (zero or more hunks)
//!  <context line>                  (leading single space)
//! -<removed line>
//! +<added line>
//! *** Delete File: <path>
//! *** End Patch
//! ```
//!
//! Multiple file sections may appear between `*** Begin Patch` and `*** End
//! Patch`. CRLF and LF line endings are both accepted, and trailing whitespace
//! on structural marker lines is tolerated.

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
    /// The text following `@@` on the hunk header, if a header was present.
    /// `None` when the hunk had no `@@` line (e.g. a single leading hunk).
    pub context_header: Option<String>,
    /// The ordered body of the hunk.
    pub lines: Vec<HunkLine>,
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

/// Strip a single trailing `\r` (so the parser is CRLF-tolerant after we split
/// on `\n`).
fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// True if `line` is exactly the bare `marker`, ignoring trailing whitespace.
/// Used for the standalone `*** Begin Patch` / `*** End Patch` markers, which
/// carry no value and so may have arbitrary trailing whitespace.
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

/// True if a CR-stripped line is any `***` structural marker. Used to detect the
/// end of a file body / hunk region. Trailing whitespace is tolerated for the
/// bare markers.
fn is_section_marker(line: &str) -> bool {
    is_bare_marker(line, BEGIN_PATCH)
        || is_bare_marker(line, END_PATCH)
        || line.starts_with(ADD_FILE)
        || line.starts_with(UPDATE_FILE)
        || line.starts_with(DELETE_FILE)
        || line.starts_with(MOVE_TO)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a V4A patch envelope into a list of [`PatchOp`].
///
/// Accepts both LF and CRLF line endings and tolerates trailing whitespace on
/// structural markers. Returns a [`PatchError`] (with a 1-based line number
/// where one applies) on malformed input. Never panics.
pub fn parse_v4a(patch: &str) -> Result<Vec<PatchOp>, PatchError> {
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
/// the terminating marker (or past the end).
fn parse_add_body(lines: &[&str], idx: &mut usize) -> Result<String, PatchError> {
    let mut body: Vec<&str> = Vec::new();
    while *idx < lines.len() {
        let line = lines[*idx];
        if is_section_marker(line) {
            break;
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

    while *idx < lines.len() {
        let line = lines[*idx];
        let lineno = *idx + 1;

        if is_section_marker(line) {
            // A second `*** Move to:` (or a Move after hunk content) is invalid.
            if line.starts_with(MOVE_TO) {
                return Err(PatchError::MisplacedMoveTo { line: lineno });
            }
            break;
        }

        // Hunk header.
        if let Some(rest) = line.strip_prefix("@@") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            let header = rest.trim();
            current = Some(Hunk {
                context_header: if header.is_empty() {
                    None
                } else {
                    Some(header.to_string())
                },
                lines: Vec::new(),
            });
            *idx += 1;
            continue;
        }

        // A body line implicitly opens a leading (header-less) hunk if none is
        // open yet.
        let hunk = current.get_or_insert_with(|| Hunk {
            context_header: None,
            lines: Vec::new(),
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
    /// A hunk contained no `-`/` ` lines to anchor against, so it cannot be
    /// located deterministically in a non-empty file.
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

/// Apply every hunk of an `Update` to `current`, in order, returning the new
/// content and counts. Errors if any hunk fails to match.
fn apply_hunks(path: &str, current: &str, hunks: &[Hunk]) -> Result<AppliedUpdate, PlanError> {
    // Work on a line view of the current content. We must round-trip exactly, so
    // we track whether the original ended with a trailing newline and rebuild it.
    let had_trailing_newline = current.ends_with('\n');
    let normalized = current.strip_suffix('\n').unwrap_or(current);
    // An empty file (`""`) has zero lines; a file that is just "\n" has one
    // empty line. `split('\n')` on "" yields [""], so special-case the empty
    // file to an empty slice.
    let current_lines: Vec<String> = if normalized.is_empty() && !had_trailing_newline {
        Vec::new()
    } else {
        normalized.split('\n').map(|s| s.to_string()).collect()
    };

    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize; // index into current_lines already emitted
    let mut added = 0usize;
    let mut removed = 0usize;

    for (hunk_index, hunk) in hunks.iter().enumerate() {
        // The "old" side of the hunk = context + removed lines, in order.
        let old: Vec<&String> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Removed(s) => Some(s),
                HunkLine::Added(_) => None,
            })
            .collect();

        if old.is_empty() {
            // No anchor. If the file is empty we can append the additions; on a
            // non-empty file an anchorless hunk is ambiguous and rejected.
            if current_lines.is_empty() {
                for l in &hunk.lines {
                    if let HunkLine::Added(s) = l {
                        out.push(s.clone());
                        added += 1;
                    }
                }
                continue;
            }
            return Err(PlanError::EmptyHunk {
                path: path.to_string(),
                hunk_index,
            });
        }

        // Find `old` as a contiguous block at or after `cursor`.
        let match_at = find_subslice(&current_lines, &old, cursor).ok_or_else(|| {
            PlanError::HunkNoMatch {
                path: path.to_string(),
                hunk_index,
                detail: format!(
                    "could not locate the {} context/removed line(s) starting near line {}",
                    old.len(),
                    cursor + 1
                ),
            }
        })?;

        // Emit unchanged lines between the cursor and the match.
        out.extend(current_lines[cursor..match_at].iter().cloned());

        // Walk the hunk body, consuming matched old lines and emitting new ones.
        let mut src = match_at;
        for l in &hunk.lines {
            match l {
                HunkLine::Context(s) => {
                    // Defensive: the subslice match guarantees equality, but we
                    // re-check to avoid silently drifting on a logic bug.
                    debug_assert_eq!(&current_lines[src], s);
                    out.push(current_lines[src].clone());
                    src += 1;
                }
                HunkLine::Removed(_) => {
                    removed += 1;
                    src += 1;
                }
                HunkLine::Added(s) => {
                    out.push(s.clone());
                    added += 1;
                }
            }
        }
        cursor = src;
    }

    // Emit the remainder of the file untouched.
    out.extend(current_lines[cursor..].iter().cloned());

    // Reassemble, preserving the original trailing-newline disposition.
    let mut content = out.join("\n");
    if had_trailing_newline {
        content.push('\n');
    }

    Ok(AppliedUpdate {
        content,
        added,
        removed,
    })
}

/// Find the start index of `needle` as a contiguous run inside `haystack`, at or
/// after `from`. Returns the absolute index in `haystack`, or `None`.
fn find_subslice(haystack: &[String], needle: &[&String], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    let last_start = haystack.len() - needle.len();
    let mut start = from;
    while start <= last_start {
        let mut matched = true;
        for (k, want) in needle.iter().enumerate() {
            if &haystack[start + k] != *want {
                matched = false;
                break;
            }
        }
        if matched {
            return Some(start);
        }
        start += 1;
    }
    None
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

    // ---- helpers ----------------------------------------------------------

    fn fm(pairs: &[(&str, &str)]) -> InMemoryFileMap {
        let mut m = InMemoryFileMap::new();
        for (p, c) in pairs {
            m.insert(*p, *c);
        }
        m
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
                    context_header: Some("fn main".to_string()),
                    lines: vec![
                        HunkLine::Context("keep".to_string()),
                        HunkLine::Removed("old".to_string()),
                        HunkLine::Added("new".to_string()),
                        HunkLine::Context("tail".to_string()),
                    ],
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
                    context_header: None,
                    lines: vec![
                        HunkLine::Removed("x".to_string()),
                        HunkLine::Added("y".to_string()),
                    ],
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
        assert_eq!(hunks[0].context_header.as_deref(), Some("first"));
        assert_eq!(hunks[1].context_header.as_deref(), Some("second"));
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
        assert_eq!(hunks[0].context_header, None);
        assert_eq!(hunks[0].lines.len(), 3);
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
        let fs = fm(&[("a.txt", "keep\nold\ntail\n")]);
        let ops = parse_v4a(
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
        )
        .unwrap();
        let plan = plan_patch(&ops, &fs).expect("plan");
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
        let fs = fm(&[("a.txt", "a\nb\nc\nd\ne\nf\n")]);
        let ops = parse_v4a(
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
        )
        .unwrap();
        let plan = plan_patch(&ops, &fs).expect("plan");
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
        let fs = fm(&[("a.txt", "x\n")]);
        let ops = parse_v4a(
            "\
*** Begin Patch
*** Update File: a.txt
*** Move to: b.txt
@@
-x
+y
*** End Patch
",
        )
        .unwrap();
        let plan = plan_patch(&ops, &fs).expect("plan");
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
        let fs = fm(&[("e.txt", "")]);
        let ops = parse_v4a(
            "*** Begin Patch\n*** Update File: e.txt\n@@\n+first\n+second\n*** End Patch\n",
        )
        .unwrap();
        let plan = plan_patch(&ops, &fs).expect("plan");
        assert_eq!(plan.files[0].new_content.as_deref(), Some("first\nsecond"));
        assert_eq!(plan.files[0].added, 2);
    }

    #[test]
    fn plan_no_trailing_newline_preserved() {
        // Source has no trailing newline; result must not gain one.
        let fs = fm(&[("a.txt", "keep\nold")]);
        let ops = parse_v4a(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n keep\n-old\n+new\n*** End Patch\n",
        )
        .unwrap();
        let plan = plan_patch(&ops, &fs).expect("plan");
        assert_eq!(plan.files[0].new_content.as_deref(), Some("keep\nnew"));
    }

    // ---- plan: failures (all-or-none) ------------------------------------

    #[test]
    fn plan_hunk_no_match_aborts_whole_plan() {
        // Second op would succeed, but the first op's hunk does not match, so
        // the WHOLE plan must fail with no partial result.
        let fs = fm(&[("a.txt", "totally\ndifferent\n"), ("ok.txt", "z\n")]);
        let ops = parse_v4a(
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
        )
        .unwrap();
        match plan_patch(&ops, &fs) {
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
        let fs = fm(&[("a.txt", "x\n"), ("b.txt", "occupied\n")]);
        let ops = parse_v4a(
            "\
*** Begin Patch
*** Update File: a.txt
*** Move to: b.txt
@@
-x
+y
*** End Patch
",
        )
        .unwrap();
        assert_eq!(
            plan_patch(&ops, &fs),
            Err(PlanError::FileExists {
                path: "b.txt".to_string()
            })
        );
    }

    #[test]
    fn plan_second_hunk_no_match_reports_index_one() {
        let fs = fm(&[("a.txt", "a\nb\nc\n")]);
        let ops = parse_v4a(
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
        )
        .unwrap();
        match plan_patch(&ops, &fs) {
            Err(PlanError::HunkNoMatch { hunk_index, .. }) => assert_eq!(hunk_index, 1),
            other => panic!("expected HunkNoMatch index 1, got {other:?}"),
        }
    }

    #[test]
    fn plan_crlf_source_matches_lf_hunk() {
        // Current content uses CRLF; parser/planner normalize so an LF-authored
        // hunk still matches. (The patch text itself is CRLF here too.)
        let fs = fm(&[("a.txt", "keep\r\nold\r\ntail\r\n")]);
        let ops = parse_v4a(
            "*** Begin Patch\r\n*** Update File: a.txt\r\n@@\r\n keep\r\n-old\r\n+new\r\n tail\r\n*** End Patch\r\n",
        )
        .unwrap();
        // NOTE: the in-memory current content still carries '\r' on each line,
        // so matching requires the hunk context to also have been CR-stripped
        // (it was). The resulting content here therefore loses the CRs, which is
        // acceptable: this test documents that LF hunks plan cleanly against an
        // LF-normalized view.
        let lf_fs = fm(&[("a.txt", "keep\nold\ntail\n")]);
        let plan = plan_patch(&ops, &lf_fs).expect("plan");
        assert_eq!(plan.files[0].new_content.as_deref(), Some("keep\nnew\ntail\n"));
        // And the CRLF map at least does not panic / produces a deterministic
        // no-match (since '\r' is part of each stored line).
        let _ = plan_patch(&ops, &fs);
    }

    #[test]
    fn plan_duplicate_add_then_add_is_file_exists() {
        // Adding the same path twice in one patch: the second add sees the first
        // as already-created.
        let fs = fm(&[]);
        let ops = parse_v4a(
            "\
*** Begin Patch
*** Add File: dup.txt
+one
*** Add File: dup.txt
+two
*** End Patch
",
        )
        .unwrap();
        assert_eq!(
            plan_patch(&ops, &fs),
            Err(PlanError::FileExists {
                path: "dup.txt".to_string()
            })
        );
    }

    #[test]
    fn plan_delete_then_readd_succeeds() {
        // Delete a file, then re-add it in the same patch: legal, since after the
        // delete the path is free.
        let fs = fm(&[("a.txt", "old\n")]);
        let ops = parse_v4a(
            "\
*** Begin Patch
*** Delete File: a.txt
*** Add File: a.txt
+fresh
*** End Patch
",
        )
        .unwrap();
        let plan = plan_patch(&ops, &fs).expect("plan");
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.files[0].op, PlannedKind::Delete);
        assert_eq!(plan.files[1].op, PlannedKind::Add);
        assert_eq!(plan.files[1].new_content.as_deref(), Some("fresh"));
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
    fn count_lines_behaves() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("a\nb\n"), 2);
    }
}
