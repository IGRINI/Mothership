//! Pure string-replacement matcher for the `edit_file` tool.
//!
//! This module is intentionally self-contained and dependency-free (std only):
//! no filesystem access, no async, and no coupling to the rest of the `tools`
//! module. It decides *what* the replacement should be and produces the new
//! content plus a simple unified-diff preview; the caller owns reading/writing
//! files, end-of-line normalization, and BOM handling.
//!
//! ## Matching strategy (strict, not aggressive fuzzy)
//!
//! 1. **Exact.** Count verbatim occurrences of `old_text` in `content`.
//!    - `replace_all = false`: exactly one match is replaced; zero matches fall
//!      through to stage 2; more than one is [`EditError::Ambiguous`].
//!    - `replace_all = true`: one or more matches are all replaced; zero falls
//!      through to stage 2.
//! 2. **Whitespace/indentation-normalized fallback.** `old_text` is matched as a
//!    contiguous run of lines, comparing each line with leading and trailing
//!    whitespace removed. This only tolerates indentation/trailing-whitespace
//!    drift (the common case where a model reproduces a block with the wrong
//!    indent); a clearly different string still does not match. The same
//!    uniqueness rules as stage 1 apply, and the *original* matched span is what
//!    gets replaced (the normalized form is never written back).
//!
//! If neither stage matches, the result is [`EditError::NotFound`]. If
//! `old_text == new_text` the result is [`EditError::NoChange`] regardless of
//! how many times the text occurs.
//!
//! The matcher operates on `content` verbatim: CRLF/LF line endings and any BOM
//! in the surrounding content are preserved byte-for-byte, and all slicing is
//! done on valid UTF-8 boundaries so multibyte content never panics.

#![allow(dead_code)]

use std::fmt;

/// Maximum number of context snippets emitted with an [`EditError::Ambiguous`]
/// error, so a UI can show *where* the ambiguous matches are without being
/// flooded when `old_text` appears very many times.
const MAX_AMBIGUOUS_SNIPPETS: usize = 5;

/// A successfully computed edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditApplied {
    /// The full new file content after applying the replacement(s).
    pub new_content: String,
    /// How many occurrences were replaced.
    pub occurrences: usize,
    /// Which matching strategy produced the replacement.
    pub strategy: EditStrategy,
    /// A simple unified-diff-style preview of the change.
    pub diff: String,
}

/// Which matching strategy located the text that was replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditStrategy {
    /// `old_text` was found verbatim.
    Exact,
    /// `old_text` was found only after normalizing per-line leading/trailing
    /// whitespace (indentation drift); the original span was still replaced.
    WhitespaceNormalized,
}

/// Why an edit could not be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// `old_text` was not found by any strategy.
    NotFound,
    /// `old_text` matched more than once and `replace_all` was `false`, so the
    /// target is ambiguous. `snippets` holds a few short context excerpts (one
    /// per match, capped at [`MAX_AMBIGUOUS_SNIPPETS`]) to help disambiguate.
    Ambiguous { count: usize, snippets: Vec<String> },
    /// `old_text` is identical to `new_text`; applying the edit would be a no-op.
    NoChange,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::NotFound => f.write_str("old_text was not found in the content"),
            EditError::Ambiguous { count, .. } => write!(
                f,
                "old_text matched {count} locations; pass replace_all or add surrounding context to disambiguate"
            ),
            EditError::NoChange => f.write_str("old_text and new_text are identical; nothing to change"),
        }
    }
}

impl std::error::Error for EditError {}

/// Apply a string replacement to `content`.
///
/// See the [module documentation](self) for the full matching semantics. In
/// short: try an exact match first, then a narrow whitespace/indentation
/// normalized fallback, requiring a unique match unless `replace_all` is set.
///
/// `content` is treated verbatim — line endings and BOM are preserved.
pub fn apply_edit(
    content: &str,
    old_text: &str,
    new_text: &str,
    replace_all: bool,
) -> Result<EditApplied, EditError> {
    if old_text == new_text {
        return Err(EditError::NoChange);
    }

    // Stage 1: exact substring matching.
    let exact = find_exact(content, old_text);
    if !exact.is_empty() {
        if replace_all {
            return Ok(build_result(
                content,
                &exact,
                new_text,
                EditStrategy::Exact,
            ));
        }
        return match exact.len() {
            1 => Ok(build_result(content, &exact, new_text, EditStrategy::Exact)),
            count => Err(EditError::Ambiguous {
                count,
                snippets: snippets_for(content, &exact),
            }),
        };
    }

    // Stage 2: whitespace/indentation-normalized fallback (line based).
    let normalized = find_whitespace_normalized(content, old_text);
    if !normalized.is_empty() {
        if replace_all {
            return Ok(build_result(
                content,
                &normalized,
                new_text,
                EditStrategy::WhitespaceNormalized,
            ));
        }
        return match normalized.len() {
            1 => Ok(build_result(
                content,
                &normalized,
                new_text,
                EditStrategy::WhitespaceNormalized,
            )),
            count => Err(EditError::Ambiguous {
                count,
                snippets: snippets_for(content, &normalized),
            }),
        };
    }

    Err(EditError::NotFound)
}

/// A matched span in `content`, expressed as a byte range `[start, end)`.
///
/// `start`/`end` are always valid UTF-8 boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

/// Find every non-overlapping exact occurrence of `needle` in `haystack`.
///
/// An empty `needle` never matches (there is nothing to replace).
fn find_exact(haystack: &str, needle: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    if needle.is_empty() {
        return spans;
    }
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        spans.push(Span { start, end });
        // Advance past this match to keep matches non-overlapping. `end` is on a
        // char boundary because `needle` is itself valid UTF-8.
        from = end;
        if from >= haystack.len() {
            break;
        }
    }
    spans
}

/// A single physical line of `content`, retaining its exact original byte span
/// (including any trailing `\r` and the surrounding text needed to rebuild the
/// file verbatim).
#[derive(Debug, Clone, Copy)]
struct Line {
    /// Byte offset of the first character of the line content.
    start: usize,
    /// Byte offset just past the line content, before its line terminator.
    content_end: usize,
    /// Byte offset just past the line terminator (== next line `start`, or the
    /// end of `content` for the last line). Used so a matched block's span
    /// covers exactly the original bytes including its terminators.
    end: usize,
}

/// Split `content` into physical lines, recording exact byte spans.
///
/// Lines are terminated by `\n`; a preceding `\r` is treated as part of the
/// terminator (so `content_end` excludes it). This makes per-line trimming
/// CRLF-aware while leaving the original bytes untouched for reconstruction.
fn split_lines(content: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let bytes = content.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut content_end = i;
            if content_end > start && bytes[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            lines.push(Line {
                start,
                content_end,
                end: i + 1,
            });
            start = i + 1;
        }
        i += 1;
    }
    // Trailing segment with no final newline (or the empty tail after a final
    // newline is intentionally *not* emitted, matching `str::lines`).
    if start < bytes.len() {
        let mut content_end = bytes.len();
        if content_end > start && bytes[content_end - 1] == b'\r' {
            content_end -= 1;
        }
        lines.push(Line {
            start,
            content_end,
            end: bytes.len(),
        });
    }
    lines
}

/// The whitespace-normalized form of a line: leading and trailing whitespace
/// removed. Interior content (including interior whitespace) is preserved, so
/// only indentation/trailing drift is tolerated — not arbitrary differences.
fn normalize_line<'a>(content: &'a str, line: &Line) -> &'a str {
    content[line.start..line.content_end].trim()
}

/// Find contiguous runs of lines in `content` whose whitespace-normalized form
/// matches `old_text`'s whitespace-normalized lines, returning the original
/// byte spans. Matches are non-overlapping.
///
/// Returns an empty vector when the normalized `old_text` is empty (i.e. it is
/// blank/whitespace only), so a blank pattern never matches arbitrarily.
fn find_whitespace_normalized(content: &str, old_text: &str) -> Vec<Span> {
    let needle_lines: Vec<&str> = normalized_nonblank_lines(old_text);
    if needle_lines.is_empty() {
        return Vec::new();
    }

    let content_lines = split_lines(content);
    // Pre-compute normalized forms once. We keep blank lines here (unlike the
    // needle) so the block must line up positionally with the source; blank
    // lines in the source simply won't equal a non-blank needle line.
    let content_norm: Vec<&str> = content_lines
        .iter()
        .map(|l| normalize_line(content, l))
        .collect();

    let mut spans = Vec::new();
    let window = needle_lines.len();
    if window == 0 || content_lines.len() < window {
        return spans;
    }

    let mut i = 0usize;
    while i + window <= content_lines.len() {
        let is_match = (0..window).all(|k| content_norm[i + k] == needle_lines[k]);
        if is_match {
            let start = content_lines[i].start;
            // The span ends at the *content end* of the last matched line so we
            // do not swallow that line's terminator (and any blank tail). This
            // mirrors exact substring replacement of a multi-line block that
            // does not itself include a trailing newline.
            let end = content_lines[i + window - 1].content_end;
            spans.push(Span { start, end });
            i += window; // non-overlapping
        } else {
            i += 1;
        }
    }
    spans
}

/// Normalized, non-blank lines of `text` (blank lines dropped). Used for the
/// needle so leading/trailing blank lines in `old_text` don't force the source
/// to contain matching blanks, while still requiring the meaningful lines to
/// line up exactly (modulo indentation).
fn normalized_nonblank_lines(text: &str) -> Vec<&str> {
    let lines = split_lines(text);
    lines
        .iter()
        .map(|l| text[l.start..l.content_end].trim())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Build the new content and diff by replacing every `span` (assumed sorted and
/// non-overlapping) with `new_text`.
fn build_result(
    content: &str,
    spans: &[Span],
    new_text: &str,
    strategy: EditStrategy,
) -> EditApplied {
    let mut new_content = String::with_capacity(content.len());
    let mut cursor = 0usize;
    for span in spans {
        new_content.push_str(&content[cursor..span.start]);
        new_content.push_str(new_text);
        cursor = span.end;
    }
    new_content.push_str(&content[cursor..]);

    let first = spans.first().copied().unwrap_or(Span { start: 0, end: 0 });
    let diff = render_diff(&content[first.start..first.end], new_text);

    EditApplied {
        new_content,
        occurrences: spans.len(),
        strategy,
        diff,
    }
}

/// Produce up to [`MAX_AMBIGUOUS_SNIPPETS`] short context excerpts, one per
/// span, each being the (trimmed) physical line containing the span's start.
fn snippets_for(content: &str, spans: &[Span]) -> Vec<String> {
    let lines = split_lines(content);
    spans
        .iter()
        .take(MAX_AMBIGUOUS_SNIPPETS)
        .map(|span| snippet_at(content, &lines, span.start))
        .collect()
}

/// The trimmed text of the physical line containing byte offset `at`, prefixed
/// with a 1-based line number for human readability.
fn snippet_at(content: &str, lines: &[Line], at: usize) -> String {
    for (idx, line) in lines.iter().enumerate() {
        // A span can start exactly at `end` of the previous line only when it is
        // an empty line; using [start, end) here keeps each offset on one line.
        if at >= line.start && at < line.end {
            let text = content[line.start..line.content_end].trim();
            return format!("line {}: {}", idx + 1, text);
        }
    }
    // Offset at the very end of the file (e.g. content does not end in newline
    // and the match is the trailing segment): attribute to the last line.
    if let Some((idx, line)) = lines.iter().enumerate().next_back() {
        let text = content[line.start..line.content_end].trim();
        return format!("line {}: {}", idx + 1, text);
    }
    String::new()
}

/// Render a minimal unified-diff-style string for replacing `old` with `new`.
///
/// Every line of `old` is emitted with a `-` prefix and every line of `new`
/// with a `+` prefix, under a single `@@` hunk header. This is dependency-free
/// and intended only to give a UI enough to show +/- lines; it is not a
/// byte-exact patch format.
fn render_diff(old: &str, new: &str) -> String {
    let old_lines = diff_lines(old);
    let new_lines = diff_lines(new);

    let mut out = String::new();
    out.push_str(&format!(
        "@@ -1,{} +1,{} @@\n",
        old_lines.len(),
        new_lines.len()
    ));
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

/// Split a fragment into lines for diff rendering, stripping a trailing `\r`
/// from each so CRLF fragments render cleanly. An empty fragment yields a single
/// empty line so the diff still shows a removed/added blank.
fn diff_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    // `split` on a string with a trailing newline yields a trailing empty
    // element; drop it so a fragment ending in "\n" doesn't render a spurious
    // extra blank line. Keep a genuinely empty fragment as one empty line.
    if lines.len() > 1 {
        if let Some(last) = lines.last() {
            if last.is_empty() {
                lines.pop();
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(content: &str, old: &str, new: &str, all: bool) -> EditApplied {
        apply_edit(content, old, new, all).expect("edit should apply")
    }

    #[test]
    fn exact_single_match_replaces() {
        let content = "fn main() {\n    let x = 1;\n}\n";
        let res = applied(content, "let x = 1;", "let x = 2;", false);
        assert_eq!(res.strategy, EditStrategy::Exact);
        assert_eq!(res.occurrences, 1);
        assert_eq!(res.new_content, "fn main() {\n    let x = 2;\n}\n");
    }

    #[test]
    fn exact_single_match_with_replace_all_false_when_unique() {
        let content = "alpha beta gamma";
        let res = applied(content, "beta", "BETA", false);
        assert_eq!(res.new_content, "alpha BETA gamma");
        assert_eq!(res.occurrences, 1);
        assert_eq!(res.strategy, EditStrategy::Exact);
    }

    #[test]
    fn exact_multiple_without_replace_all_is_ambiguous() {
        let content = "x = 1\ny = 1\nz = 1\n";
        let err = apply_edit(content, "= 1", "= 9", false).unwrap_err();
        match err {
            EditError::Ambiguous { count, snippets } => {
                assert_eq!(count, 3);
                assert_eq!(snippets.len(), 3);
                assert_eq!(snippets[0], "line 1: x = 1");
                assert_eq!(snippets[1], "line 2: y = 1");
                assert_eq!(snippets[2], "line 3: z = 1");
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_snippets_are_capped() {
        // Ten matches, but snippets cap at MAX_AMBIGUOUS_SNIPPETS (5).
        let content = "a\na\na\na\na\na\na\na\na\na\n";
        let err = apply_edit(content, "a", "b", false).unwrap_err();
        match err {
            EditError::Ambiguous { count, snippets } => {
                assert_eq!(count, 10);
                assert_eq!(snippets.len(), MAX_AMBIGUOUS_SNIPPETS);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn replace_all_replaces_every_occurrence() {
        let content = "x = 1\ny = 1\nz = 1\n";
        let res = applied(content, "= 1", "= 9", true);
        assert_eq!(res.occurrences, 3);
        assert_eq!(res.strategy, EditStrategy::Exact);
        assert_eq!(res.new_content, "x = 9\ny = 9\nz = 9\n");
    }

    #[test]
    fn single_line_dedent_is_handled_by_exact_substring() {
        // A purely dedented single line is always an exact *substring* of the
        // indented source, so stage 1 matches it and only the substring is
        // replaced — the original indentation is preserved for free. This is the
        // most useful outcome and means stage 2 is reserved for cases substring
        // matching genuinely cannot reach (multiline blocks, trailing drift).
        let content = "fn f() {\n    return 42;\n}\n";
        let old = "return 42;"; // dedented relative to the 4-space source line
        let res = applied(content, old, "return 7;", false);
        assert_eq!(res.strategy, EditStrategy::Exact);
        assert_eq!(res.occurrences, 1);
        assert_eq!(res.new_content, "fn f() {\n    return 7;\n}\n");
    }

    #[test]
    fn single_line_trailing_drift_falls_through_to_whitespace_stage() {
        // old_text has trailing whitespace the source line lacks, so it is NOT an
        // exact substring; stage 2 trims trailing whitespace and matches. The
        // matched span is the whole original line, replaced verbatim by new_text.
        let content = "fn f() {\n    return 42;\n}\n";
        let old = "    return 42;   "; // trailing spaces -> not a substring
        let res = applied(content, old, "    return 7;", false);
        assert_eq!(res.strategy, EditStrategy::WhitespaceNormalized);
        assert_eq!(res.occurrences, 1);
        assert_eq!(res.new_content, "fn f() {\n    return 7;\n}\n");
    }

    #[test]
    fn whitespace_normalized_multiline_block_maps_to_original_offsets() {
        let content = "class A:\n    def run(self):\n        do_a()\n        do_b()\n";
        // Model reproduced the body with no indentation at all. Because the
        // interior newlines + indentation differ, this is not an exact substring,
        // so stage 2 matches the three-line block.
        let old = "def run(self):\ndo_a()\ndo_b()";
        // new_text supplies the desired indentation; the matched span covers the
        // original lines *including* their leading indentation, so new_text fully
        // dictates the replacement bytes.
        let new = "    def run(self):\n        do_x()";
        let res = applied(content, old, new, false);
        assert_eq!(res.strategy, EditStrategy::WhitespaceNormalized);
        assert_eq!(res.occurrences, 1);
        assert_eq!(
            res.new_content,
            "class A:\n    def run(self):\n        do_x()\n"
        );
    }

    #[test]
    fn whitespace_normalized_span_covers_original_indentation_of_first_line() {
        // Multiline block whose first line is indented in the source. The matched
        // span starts at column 0 of that line, so the original indentation is
        // part of what gets replaced — new_text alone determines the output.
        let content = "    keep_above\n        a()\n        b()\n    keep_below\n";
        let old = "a()\nb()"; // dedented two-line block, not an exact substring
        let res = applied(content, old, "        c()\n        d()", false);
        assert_eq!(res.strategy, EditStrategy::WhitespaceNormalized);
        assert_eq!(
            res.new_content,
            "    keep_above\n        c()\n        d()\n    keep_below\n"
        );
    }

    #[test]
    fn clearly_different_text_does_not_fuzzy_match() {
        let content = "let total = compute_sum(values);\n";
        // Same shape, but a genuinely different identifier. Must NOT match.
        let err = apply_edit(content, "let total = compute_average(values);", "x", false)
            .unwrap_err();
        assert_eq!(err, EditError::NotFound);
    }

    #[test]
    fn interior_whitespace_difference_does_not_match() {
        // Only leading/trailing whitespace is normalized; interior spacing change
        // is a real difference and must not match.
        let content = "let  x = 1;\n"; // two spaces after let
        let err = apply_edit(content, "let x = 1;", "let x = 2;", false).unwrap_err();
        assert_eq!(err, EditError::NotFound);
    }

    #[test]
    fn whitespace_normalized_multiple_matches_are_ambiguous() {
        // Two-line blocks at different indentation. The interior newline prevents
        // an exact substring match, so stage 2 runs and finds the block twice.
        let content = "  a()\n  b()\nmid\n      a()\n      b()\n";
        let old = "a()\nb()"; // dedented, multiline -> stage 2
        let err = apply_edit(content, old, "x()\ny()", false).unwrap_err();
        match err {
            EditError::Ambiguous { count, .. } => assert_eq!(count, 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn whitespace_normalized_replace_all() {
        // Genuine stage-2 replace_all over two differently-indented blocks.
        let content = "  a()\n  b()\n      a()\n      b()\n";
        let old = "a()\nb()";
        let res = applied(content, old, "x()\ny()", true);
        assert_eq!(res.strategy, EditStrategy::WhitespaceNormalized);
        assert_eq!(res.occurrences, 2);
        // Each matched block's original indentation is inside the replaced span,
        // so new_text (no indent) replaces it wholesale at both sites.
        assert_eq!(res.new_content, "x()\ny()\nx()\ny()\n");
    }

    #[test]
    fn exact_substring_ambiguity_takes_precedence_over_stage_two() {
        // "item" is an exact substring twice; stage 1 reports the ambiguity and we
        // never silently fall through to a whitespace-normalized interpretation.
        let content = "    item\nitem\n";
        let err = apply_edit(content, "item", "ITEM", false).unwrap_err();
        match err {
            EditError::Ambiguous { count, .. } => assert_eq!(count, 2),
            other => panic!("expected Ambiguous from stage 1, got {other:?}"),
        }
    }

    #[test]
    fn exact_single_substring_replaced_preserving_surrounding_indent() {
        // Single exact substring inside an indented line: only the substring is
        // replaced, indentation preserved. (Contrast with stage 2, which replaces
        // whole line spans.)
        let content = "        value = compute();\n";
        let res = applied(content, "compute()", "compute(x)", false);
        assert_eq!(res.strategy, EditStrategy::Exact);
        assert_eq!(res.new_content, "        value = compute(x);\n");
    }

    #[test]
    fn no_change_when_old_equals_new() {
        let content = "hello world\n";
        let err = apply_edit(content, "hello", "hello", false).unwrap_err();
        assert_eq!(err, EditError::NoChange);
        // Also true even if it would have been ambiguous.
        let content2 = "a a a";
        let err2 = apply_edit(content2, "a", "a", true).unwrap_err();
        assert_eq!(err2, EditError::NoChange);
    }

    #[test]
    fn not_found_returns_not_found() {
        let content = "the quick brown fox";
        let err = apply_edit(content, "lazy dog", "cat", false).unwrap_err();
        assert_eq!(err, EditError::NotFound);
    }

    #[test]
    fn empty_old_text_does_not_match() {
        let content = "anything";
        let err = apply_edit(content, "", "x", false).unwrap_err();
        // Empty needle never matches exactly and normalizes to no lines.
        assert_eq!(err, EditError::NotFound);
    }

    #[test]
    fn multibyte_content_is_safe_and_correct() {
        // Mix of emoji and CJK; replacing a multibyte substring must not panic
        // and must preserve surrounding bytes.
        let content = "héllo 🌍 世界 — done\n";
        let res = applied(content, "🌍 世界", "🌎 world", false);
        assert_eq!(res.strategy, EditStrategy::Exact);
        assert_eq!(res.new_content, "héllo 🌎 world — done\n");
    }

    #[test]
    fn multibyte_whitespace_normalized_does_not_panic() {
        // Stage-2 path with multibyte content. A trailing-space drift in old_text
        // forces stage 2 (the dedented form alone would be an exact substring).
        // Byte offsets land on multibyte boundaries and must not panic.
        let content = "    let 名前 = \"🌍\";\n";
        let old = "let 名前 = \"🌍\";   "; // trailing spaces -> not a substring
        let res = applied(content, old, "    let 名前 = \"🌎\";", false);
        assert_eq!(res.strategy, EditStrategy::WhitespaceNormalized);
        assert_eq!(res.new_content, "    let 名前 = \"🌎\";\n");
    }

    #[test]
    fn multibyte_multiline_whitespace_normalized() {
        // Multiline stage-2 block containing multibyte characters.
        let content = "区分:\n    α()\n    β()\n";
        let old = "区分:\nα()\nβ()"; // dedented body -> stage 2 (interior newlines)
        let res = applied(content, old, "区分:\n    γ()", false);
        assert_eq!(res.strategy, EditStrategy::WhitespaceNormalized);
        assert_eq!(res.new_content, "区分:\n    γ()\n");
    }

    #[test]
    fn crlf_line_endings_are_preserved_exact_path() {
        let content = "line1\r\nlet x = 1;\r\nline3\r\n";
        let res = applied(content, "let x = 1;", "let x = 2;", false);
        assert_eq!(res.strategy, EditStrategy::Exact);
        // Surrounding CRLFs must be byte-for-byte intact.
        assert_eq!(res.new_content, "line1\r\nlet x = 2;\r\nline3\r\n");
        assert!(res.new_content.contains("\r\n"));
    }

    #[test]
    fn crlf_line_endings_are_preserved_whitespace_path() {
        // Stage 2 over CRLF content. A multiline block forces stage 2 (the single
        // dedented line would otherwise be an exact substring). The matched span
        // must exclude the CRLF terminators so the surrounding CRLFs survive and
        // the trailing \r of the last matched line is not swallowed.
        let content = "a\r\n        one\r\n        two\r\nb\r\n";
        let old = "one\r\ntwo"; // dedented two-line block
        let res = applied(content, old, "        ONE\r\n        TWO", false);
        assert_eq!(res.strategy, EditStrategy::WhitespaceNormalized);
        assert_eq!(res.new_content, "a\r\n        ONE\r\n        TWO\r\nb\r\n");
        // The terminator after the last matched line ("two") must still be CRLF.
        assert!(res.new_content.contains("TWO\r\nb"));
    }

    #[test]
    fn bom_is_preserved() {
        // A leading BOM is just content; it must be carried through untouched.
        let content = "\u{FEFF}fn main() { let x = 1; }\n";
        let res = applied(content, "let x = 1;", "let x = 2;", false);
        assert!(res.new_content.starts_with('\u{FEFF}'));
        assert_eq!(res.new_content, "\u{FEFF}fn main() { let x = 2; }\n");
    }

    #[test]
    fn diff_contains_minus_and_plus_lines() {
        let content = "let x = 1;\n";
        let res = applied(content, "let x = 1;", "let x = 2;", false);
        assert!(res.diff.contains("-let x = 1;"), "diff was: {}", res.diff);
        assert!(res.diff.contains("+let x = 2;"), "diff was: {}", res.diff);
        assert!(res.diff.starts_with("@@"), "diff was: {}", res.diff);
    }

    #[test]
    fn diff_handles_multiline_change() {
        let content = "a\nb\nc\n";
        // Replace the exact two-line block "a\nb".
        let res = applied(content, "a\nb", "x\ny\nz", false);
        let diff = &res.diff;
        assert!(diff.contains("-a"), "diff: {diff}");
        assert!(diff.contains("-b"), "diff: {diff}");
        assert!(diff.contains("+x"), "diff: {diff}");
        assert!(diff.contains("+y"), "diff: {diff}");
        assert!(diff.contains("+z"), "diff: {diff}");
        // Hunk header reflects the line counts (2 removed, 3 added).
        assert!(diff.contains("@@ -1,2 +1,3 @@"), "diff: {diff}");
    }

    #[test]
    fn diff_does_not_corrupt_crlf_fragment_rendering() {
        let content = "x\r\nlet a = 1;\r\ny\r\n";
        let res = applied(content, "let a = 1;", "let a = 2;", false);
        // The diff lines themselves should be clean (no stray \r in the shown
        // fragment lines); they are LF-joined for display.
        assert!(res.diff.contains("-let a = 1;"));
        assert!(res.diff.contains("+let a = 2;"));
        assert!(!res.diff.contains('\r'), "diff leaked CR: {:?}", res.diff);
        // But the actual edited content keeps its CRLFs.
        assert_eq!(res.new_content, "x\r\nlet a = 2;\r\ny\r\n");
    }

    #[test]
    fn replace_all_is_non_overlapping() {
        // Overlapping pattern "aa" in "aaaa" must match twice, not three times.
        let content = "aaaa";
        let res = applied(content, "aa", "b", true);
        assert_eq!(res.occurrences, 2);
        assert_eq!(res.new_content, "bb");
    }

    #[test]
    fn split_lines_round_trips_verbatim() {
        // Sanity: reconstructing from line spans yields the exact original.
        for content in [
            "a\nb\nc\n",
            "a\r\nb\r\n",
            "no trailing newline",
            "trailing\n",
            "\n\n\n",
            "\u{FEFF}bom\nmore\n",
            "",
        ] {
            let lines = split_lines(content);
            let mut rebuilt = String::new();
            let mut cursor = 0;
            for l in &lines {
                rebuilt.push_str(&content[cursor..l.end]);
                cursor = l.end;
            }
            rebuilt.push_str(&content[cursor..]);
            assert_eq!(rebuilt, content, "round-trip failed for {content:?}");
        }
    }

    #[test]
    fn whitespace_match_requires_full_block_alignment() {
        // A needle whose middle line differs must not match even if first/last do.
        let content = "    open()\n    middle()\n    close()\n";
        let old = "open()\nDIFFERENT()\nclose()";
        let err = apply_edit(content, old, "x", false).unwrap_err();
        assert_eq!(err, EditError::NotFound);
    }
}
