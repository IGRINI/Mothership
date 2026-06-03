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
//! Each stage runs only when the previous ones fail to find any match, and every
//! stage enforces the same uniqueness rule: a unique match unless `replace_all`
//! is set. None of these stages performs similarity/Levenshtein/block-anchor
//! fuzzing — a clearly different string never matches.
//!
//! 0. **Trailing-newline absorption on deletion (deletion-only).** When
//!    `new_text` is empty and `old_text` does not end with `\n` but
//!    `old_text + "\n"` occurs in the file, the deletion target becomes
//!    `old_text + "\n"` so the line terminator is removed too and no orphan
//!    blank line is left behind. This is still an *exact* match (just of the
//!    newline-extended target), so it is preferred over matching `old_text`
//!    alone — otherwise the feature could never engage. If `old_text + "\n"` is
//!    absent, the normal pipeline below runs with `old_text` unchanged. This
//!    mirrors Claude's `applyEditToFile`.
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
//! 3. **Curly ↔ straight quote-normalized.** `old_text` is compared to the file
//!    after folding typographic quotes to ASCII on *both* sides
//!    (`U+2018`/`U+2019` → `'`, `U+201C`/`U+201D` → `"`). When this is the only
//!    difference, the *original* span is replaced, and `new_text`'s quotes are
//!    rewritten to match the quote *style* actually present in the matched span
//!    (style-preserving writeback): if the file used curly quotes, straight
//!    quotes in `new_text` are curled; if the file used straight quotes, curly
//!    quotes in `new_text` are straightened.
//! 4. **Literal escape-sequence normalized.** When `old_text` contains the
//!    two-character sequences `\n` / `\t` (backslash-n / backslash-t) where the
//!    file has a real newline / tab — i.e. a model emitted the escapes
//!    literally — the unescaped needle is matched against the file and the
//!    *original* (real-character) span is replaced.
//!
//! If no stage matches, the result is [`EditError::NotFound`]. If
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
    /// `old_text` matched only after folding curly/typographic quotes to ASCII
    /// on both sides; the original span was replaced and `new_text`'s quotes
    /// were rewritten to match the quote style present in the matched span.
    QuoteNormalized,
    /// `old_text` matched only after unescaping literal `\n` / `\t` two-character
    /// sequences in `old_text` to a real newline / tab; the original
    /// (real-character) span was replaced.
    EscapeNormalized,
    /// A pure deletion (`new_text` is empty) where `old_text` did not end in a
    /// newline but `old_text + "\n"` matched; the trailing newline was deleted
    /// along with `old_text`.
    TrailingNewline,
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
/// short: try an exact match first, then progressively narrower deterministic
/// normalizations (whitespace/indentation, curly-quote style, literal escape
/// sequences), requiring a unique match unless `replace_all` is set. None of
/// these stages is a fuzzy/similarity match.
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

    // Trailing-newline absorption on a pure deletion (Claude's `applyEditToFile`
    // behavior). When deleting text that does not itself end in a newline, the
    // intent is to remove the whole line including its terminator — otherwise an
    // orphan blank line is left behind. This is still an *exact* match (of
    // `old_text + "\n"`), not a fuzzy one, so it runs before the other stages:
    // matching `old_text` alone would leave the newline and defeat the purpose.
    // If `old_text + "\n"` is absent, we fall through to the normal pipeline.
    if new_text.is_empty() && !old_text.is_empty() && !old_text.ends_with('\n') {
        let with_newline = format!("{old_text}\n");
        let trailing_spans = find_exact(content, &with_newline);
        if !trailing_spans.is_empty() {
            let decided = decide(content, &trailing_spans, replace_all)?;
            return Ok(build_result(
                content,
                decided,
                new_text,
                EditStrategy::TrailingNewline,
            ));
        }
    }

    // Stage 1: exact substring matching.
    let exact = find_exact(content, old_text);
    if !exact.is_empty() {
        if replace_all {
            return Ok(build_result(content, &exact, new_text, EditStrategy::Exact));
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

    // Stage 3: curly <-> straight quote-normalized matching. The replacement
    // text is computed per span so the file's quote *style* is preserved in the
    // writeback, which is why this stage cannot reuse `build_result`.
    let quote_spans = find_quote_normalized(content, old_text);
    if !quote_spans.is_empty() {
        let decided = decide(content, &quote_spans, replace_all)?;
        return Ok(build_result_quote_preserving(content, decided, new_text));
    }

    // Stage 4: literal escape-sequence normalization. A model emitted `\n`/`\t`
    // as two literal characters where the file has the real control character;
    // unescape the needle and match the real-character span. `new_text` is
    // written back verbatim.
    let escape_spans = find_escape_normalized(content, old_text);
    if !escape_spans.is_empty() {
        let decided = decide(content, &escape_spans, replace_all)?;
        return Ok(build_result(
            content,
            decided,
            new_text,
            EditStrategy::EscapeNormalized,
        ));
    }

    Err(EditError::NotFound)
}

/// Apply the shared uniqueness rule to a set of candidate spans: with
/// `replace_all` set, all spans are accepted; otherwise exactly one span is
/// required (zero is impossible here — callers only invoke this with a non-empty
/// set) and more than one is [`EditError::Ambiguous`].
///
/// On success the accepted spans are returned so the caller can build the
/// result; this keeps each stage's accept/ambiguity logic identical.
fn decide<'a>(
    content: &str,
    spans: &'a [Span],
    replace_all: bool,
) -> Result<&'a [Span], EditError> {
    if replace_all {
        return Ok(spans);
    }
    match spans.len() {
        1 => Ok(spans),
        count => Err(EditError::Ambiguous {
            count,
            snippets: snippets_for(content, spans),
        }),
    }
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

// --- Stage 3: curly <-> straight quote normalization -----------------------

/// Left single typographic quote (U+2018), folded to ASCII `'`.
const LEFT_SINGLE_QUOTE: char = '\u{2018}';
/// Right single typographic quote (U+2019), folded to ASCII `'`.
const RIGHT_SINGLE_QUOTE: char = '\u{2019}';
/// Left double typographic quote (U+201C), folded to ASCII `"`.
const LEFT_DOUBLE_QUOTE: char = '\u{201C}';
/// Right double typographic quote (U+201D), folded to ASCII `"`.
const RIGHT_DOUBLE_QUOTE: char = '\u{201D}';

/// Fold the four curly/typographic quote characters to their ASCII equivalents.
/// Every other character is left untouched. Used to compare `old_text` against
/// the file when the only difference is quote *style*.
fn fold_quotes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        out.push(fold_quote_char(ch));
    }
    out
}

/// Map a single character to its quote-folded form (curly quote -> ASCII), or
/// return it unchanged.
fn fold_quote_char(ch: char) -> char {
    match ch {
        LEFT_SINGLE_QUOTE | RIGHT_SINGLE_QUOTE => '\'',
        LEFT_DOUBLE_QUOTE | RIGHT_DOUBLE_QUOTE => '"',
        other => other,
    }
}

/// Whether `text` contains any of the four curly/typographic quote characters.
fn contains_curly_quote(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(
            ch,
            LEFT_SINGLE_QUOTE | RIGHT_SINGLE_QUOTE | LEFT_DOUBLE_QUOTE | RIGHT_DOUBLE_QUOTE
        )
    })
}

/// Fold `content`'s quotes to ASCII while recording, for every byte offset in
/// the folded string, the corresponding byte offset in the *original*
/// `content`. The returned `orig_at` has length `folded.len() + 1`; for any
/// folded offset that is a char boundary, `orig_at[offset]` is the original
/// byte offset of the character starting there (and `orig_at[folded.len()]`
/// equals `content.len()`). This lets a match found in the folded string be
/// mapped back to an exact original byte span.
fn fold_quotes_with_map(content: &str) -> (String, Vec<usize>) {
    let mut folded = String::with_capacity(content.len());
    // One entry per folded byte, plus a trailing sentinel.
    let mut orig_at: Vec<usize> = Vec::with_capacity(content.len() + 1);
    let mut buf = [0u8; 4];
    for (orig_off, ch) in content.char_indices() {
        let folded_ch = fold_quote_char(ch);
        let encoded = folded_ch.encode_utf8(&mut buf);
        for _ in 0..encoded.len() {
            // Every byte of this folded char maps back to where the source char
            // began; the char boundary we care about (its first byte) therefore
            // resolves to `orig_off`, and the next char's first byte resolves to
            // that char's own start, so end offsets land correctly too.
            orig_at.push(orig_off);
        }
        folded.push_str(encoded);
    }
    orig_at.push(content.len());
    (folded, orig_at)
}

/// Find spans in `content` that match `old_text` only after curly/straight quote
/// folding. Returns an empty vector when an exact match would already have
/// succeeded (the caller runs stage 1 first) or when neither side contains any
/// quote at all, so this stage never fires spuriously on quote-free text.
///
/// The returned spans are *original* byte ranges (the curly-quoted text as it
/// appears in the file), so the writeback replaces exactly those bytes.
fn find_quote_normalized(content: &str, old_text: &str) -> Vec<Span> {
    if old_text.is_empty() {
        return Vec::new();
    }

    let folded_needle = fold_quotes(old_text);
    // If folding changed nothing on either side there is no quote drift to
    // reconcile, so this stage has nothing to add over the exact stage.
    let needle_has_curly = folded_needle != old_text;
    if !needle_has_curly && !contains_curly_quote(content) {
        return Vec::new();
    }

    let (folded_content, orig_at) = fold_quotes_with_map(content);
    let folded_spans = find_exact(&folded_content, &folded_needle);

    folded_spans
        .into_iter()
        .map(|s| Span {
            start: orig_at[s.start],
            end: orig_at[s.end],
        })
        .collect()
}

/// Rewrite the quote *style* of `new_text` to match the matched original span.
///
/// For each quote family (double, then single) independently: if the matched
/// span uses curly quotes of that family, straight quotes of that family in
/// `new_text` are curled (open/close heuristic, with contraction handling for
/// single quotes); if the span uses straight quotes of that family, curly quotes
/// in `new_text` are straightened; if the span has no quote of that family, that
/// family is left untouched. This is symmetric, so it preserves the file's
/// typography whether the model sent straight quotes into a curly-quoted file or
/// vice versa.
fn preserve_quote_style(actual_old: &str, new_text: &str) -> String {
    let span_has_curly_double =
        actual_old.contains(LEFT_DOUBLE_QUOTE) || actual_old.contains(RIGHT_DOUBLE_QUOTE);
    let span_has_straight_double = actual_old.contains('"');
    let span_has_curly_single =
        actual_old.contains(LEFT_SINGLE_QUOTE) || actual_old.contains(RIGHT_SINGLE_QUOTE);
    let span_has_straight_single = actual_old.contains('\'');

    let chars: Vec<char> = new_text.chars().collect();
    let mut out = String::with_capacity(new_text.len());
    for (i, &ch) in chars.iter().enumerate() {
        match ch {
            '"' if span_has_curly_double => {
                out.push(if is_opening_context(&chars, i) {
                    LEFT_DOUBLE_QUOTE
                } else {
                    RIGHT_DOUBLE_QUOTE
                });
            }
            LEFT_DOUBLE_QUOTE | RIGHT_DOUBLE_QUOTE
                if span_has_straight_double && !span_has_curly_double =>
            {
                out.push('"');
            }
            '\'' if span_has_curly_single => {
                out.push(curly_single_for(&chars, i));
            }
            LEFT_SINGLE_QUOTE | RIGHT_SINGLE_QUOTE
                if span_has_straight_single && !span_has_curly_single =>
            {
                out.push('\'');
            }
            other => out.push(other),
        }
    }
    out
}

/// Choose the curly single quote for a straight `'` at index `i`: a `'` between
/// two letters is treated as a contraction apostrophe (right single quote);
/// otherwise the open/close heuristic decides.
fn curly_single_for(chars: &[char], i: usize) -> char {
    let prev_is_letter = i
        .checked_sub(1)
        .and_then(|p| chars.get(p))
        .is_some_and(|c| c.is_alphabetic());
    let next_is_letter = chars.get(i + 1).is_some_and(|c| c.is_alphabetic());
    if prev_is_letter && next_is_letter {
        RIGHT_SINGLE_QUOTE
    } else if is_opening_context(chars, i) {
        LEFT_SINGLE_QUOTE
    } else {
        RIGHT_SINGLE_QUOTE
    }
}

/// Whether the character at `index` should be treated as an *opening* quote: it
/// is opening at the start of the text or when preceded by whitespace or an
/// opening bracket / dash. Mirrors Claude's `isOpeningContext` heuristic.
fn is_opening_context(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    matches!(
        chars[index - 1],
        ' ' | '\t' | '\n' | '\r' | '(' | '[' | '{' | '\u{2014}' | '\u{2013}'
    )
}

// --- Stage 4: literal escape-sequence normalization ------------------------

/// Find spans in `content` that match `old_text` after unescaping the literal
/// two-character sequences `\n` / `\t` (backslash-n / backslash-t) to a real
/// newline / tab. Returns an empty vector unless `old_text` actually contains
/// one of those sequences (so this stage never duplicates the exact stage). The
/// returned spans are real-character byte ranges in `content`.
fn find_escape_normalized(content: &str, old_text: &str) -> Vec<Span> {
    let Some(unescaped) = unescape_n_t(old_text) else {
        return Vec::new();
    };
    if unescaped.is_empty() {
        return Vec::new();
    }
    find_exact(content, &unescaped)
}

/// Unescape only the literal `\n` and `\t` two-character sequences in `text`,
/// returning `None` when `text` contains neither (so the caller can cheaply skip
/// the stage). A trailing lone backslash and any other backslash escape are left
/// verbatim, keeping the transform narrow and deterministic.
fn unescape_n_t(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut found = false;
    // `\n`/`\t` are ASCII, so scanning bytes is safe on UTF-8 and never splits a
    // multibyte char (no multibyte byte equals the ASCII backslash).
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => {
                    out.push('\n');
                    found = true;
                    i += 2;
                    continue;
                }
                b't' => {
                    out.push('\t');
                    found = true;
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        // Copy this byte's whole char verbatim. `i` is on a char boundary here:
        // either the previous iteration consumed a full `\n`/`\t` pair (ASCII)
        // or it advanced by a full char below.
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    if found {
        Some(out)
    } else {
        None
    }
}

/// Length in bytes of the UTF-8 character whose leading byte is `b`. `b` is
/// assumed to be a valid UTF-8 leading byte (it always is when iterating a
/// `&str` at char boundaries).
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
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

/// Build the result for the quote-normalized stage. Each `span` is replaced with
/// a *style-preserved* rendering of `new_text` derived from that span's own
/// original quote style, so differently-quoted matches under `replace_all` each
/// keep their own typography. The diff shows the first span's change.
fn build_result_quote_preserving(content: &str, spans: &[Span], new_text: &str) -> EditApplied {
    let mut new_content = String::with_capacity(content.len());
    let mut cursor = 0usize;
    let mut first_replacement: Option<String> = None;
    for span in spans {
        let actual_old = &content[span.start..span.end];
        let replacement = preserve_quote_style(actual_old, new_text);
        new_content.push_str(&content[cursor..span.start]);
        new_content.push_str(&replacement);
        cursor = span.end;
        if first_replacement.is_none() {
            first_replacement = Some(replacement);
        }
    }
    new_content.push_str(&content[cursor..]);

    let first = spans.first().copied().unwrap_or(Span { start: 0, end: 0 });
    let first_replacement = first_replacement.unwrap_or_default();
    let diff = render_diff(&content[first.start..first.end], &first_replacement);

    EditApplied {
        new_content,
        occurrences: spans.len(),
        strategy: EditStrategy::QuoteNormalized,
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
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
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
        let err =
            apply_edit(content, "let total = compute_average(values);", "x", false).unwrap_err();
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

    // --- Stage 3: curly <-> straight quote normalization -------------------

    // Convenience handles for the typographic quotes used across these tests.
    const LSQUO: char = '\u{2018}';
    const RSQUO: char = '\u{2019}';
    const LDQUO: char = '\u{201C}';
    const RDQUO: char = '\u{201D}';

    #[test]
    fn quote_normalized_file_curly_model_straight_double() {
        // File has curly double quotes; model sent straight quotes. It must match
        // via stage 3 and the writeback must KEEP the file's curly style.
        let content = format!("let msg = {LDQUO}hello{RDQUO};\n");
        let res = applied(
            &content,
            "let msg = \"hello\";",
            "let msg = \"world\";",
            false,
        );
        assert_eq!(res.strategy, EditStrategy::QuoteNormalized);
        assert_eq!(res.occurrences, 1);
        let expected = format!("let msg = {LDQUO}world{RDQUO};\n");
        assert_eq!(res.new_content, expected);
    }

    #[test]
    fn quote_normalized_file_straight_model_curly_double() {
        // Vice versa: file has straight quotes, model sent curly quotes. Match via
        // stage 3 and the writeback must straighten new_text to keep file style.
        let content = "let msg = \"hello\";\n";
        let old = format!("let msg = {LDQUO}hello{RDQUO};");
        let new = format!("let msg = {LDQUO}world{RDQUO};");
        let res = applied(content, &old, &new, false);
        assert_eq!(res.strategy, EditStrategy::QuoteNormalized);
        assert_eq!(res.new_content, "let msg = \"world\";\n");
    }

    #[test]
    fn quote_normalized_single_quotes_curly_file() {
        // Curly single quotes in the file, straight in the model output.
        let content = format!("name = {LSQUO}Ada{RSQUO}\n");
        let res = applied(&content, "name = 'Ada'", "name = 'Grace'", false);
        assert_eq!(res.strategy, EditStrategy::QuoteNormalized);
        // Opening quote stays a LEFT single curly, closing stays RIGHT.
        let expected = format!("name = {LSQUO}Grace{RSQUO}\n");
        assert_eq!(res.new_content, expected);
    }

    #[test]
    fn quote_normalized_preserves_contraction_apostrophe() {
        // The matched span uses a curly apostrophe inside a contraction. When we
        // curl new_text, an apostrophe between two letters must become a RIGHT
        // single quote (not a LEFT opening quote).
        let content = format!("s = {LSQUO}don{RSQUO}t{RSQUO}\n");
        // Model sent straight quotes for the same text.
        let old = "s = 'don't'";
        let new = "s = 'won't'";
        let res = applied(&content, old, new, false);
        assert_eq!(res.strategy, EditStrategy::QuoteNormalized);
        // Opening -> LEFT, the contraction apostrophe -> RIGHT, closing -> RIGHT.
        let expected = format!("s = {LSQUO}won{RSQUO}t{RSQUO}\n");
        assert_eq!(res.new_content, expected);
    }

    #[test]
    fn quote_normalized_multibyte_span_maps_to_original_bytes() {
        // Curly quotes are 3 bytes each; ensure the original byte span is mapped
        // correctly even with multibyte content around it.
        let content = format!("世界 = {LDQUO}café{RDQUO} 🌍\n");
        let res = applied(&content, "世界 = \"café\" 🌍", "世界 = \"thé\" 🌍", false);
        assert_eq!(res.strategy, EditStrategy::QuoteNormalized);
        let expected = format!("世界 = {LDQUO}thé{RDQUO} 🌍\n");
        assert_eq!(res.new_content, expected);
    }

    #[test]
    fn quote_normalized_respects_uniqueness() {
        // Two curly-quoted matches; straight-quoted needle is ambiguous without
        // replace_all (uniqueness must still be enforced in stage 3).
        let content = format!("a = {LDQUO}x{RDQUO}\nb = {LDQUO}x{RDQUO}\n");
        let err = apply_edit(&content, "= \"x\"", "= \"y\"", false).unwrap_err();
        match err {
            EditError::Ambiguous { count, .. } => assert_eq!(count, 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn quote_normalized_replace_all_preserves_each_span_style() {
        // replace_all over two curly-quoted spans. Stage 3 handles both (neither
        // is an exact substring of the straight needle), and the per-span
        // writeback keeps each one's curly style. A mix of curly + straight could
        // not reach stage 3 for the straight span, since the exact stage would
        // claim it first — so both spans here are curly by design.
        let content = format!("a = {LDQUO}x{RDQUO}\nb = {LDQUO}x{RDQUO}\n");
        let res = applied(&content, "= \"x\"", "= \"y\"", true);
        assert_eq!(res.strategy, EditStrategy::QuoteNormalized);
        assert_eq!(res.occurrences, 2);
        let expected = format!("a = {LDQUO}y{RDQUO}\nb = {LDQUO}y{RDQUO}\n");
        assert_eq!(res.new_content, expected);
    }

    #[test]
    fn quote_normalized_does_not_fire_when_no_quotes_involved() {
        // Quote-free, clearly different text must NOT be rescued by stage 3.
        let content = "let total = compute_sum(values);\n";
        let err =
            apply_edit(content, "let total = compute_average(values);", "x", false).unwrap_err();
        assert_eq!(err, EditError::NotFound);
    }

    #[test]
    fn quote_normalized_does_not_match_different_text_with_quotes() {
        // Same quote shape but a genuinely different identifier inside the quotes
        // must still NOT match — folding quotes does not relax the rest.
        let content = format!("msg = {LDQUO}hello{RDQUO}\n");
        let err = apply_edit(&content, "msg = \"goodbye\"", "x", false).unwrap_err();
        assert_eq!(err, EditError::NotFound);
    }

    #[test]
    fn quote_normalized_leaves_new_text_quotes_when_span_has_no_quote_family() {
        // The matched span has curly DOUBLE quotes but no single quotes; a single
        // quote (apostrophe) in new_text must be left untouched.
        let content = format!("v = {LDQUO}a{RDQUO}\n");
        let res = applied(&content, "v = \"a\"", "v = \"it's\"", false);
        assert_eq!(res.strategy, EditStrategy::QuoteNormalized);
        // Double quotes curled to match the file; the apostrophe stays straight
        // because the span had no single-quote family to mirror.
        let expected = format!("v = {LDQUO}it's{RDQUO}\n");
        assert_eq!(res.new_content, expected);
    }

    // --- Stage 4: literal escape-sequence normalization --------------------

    #[test]
    fn escape_normalized_literal_newline_in_old_text() {
        // Model emitted "\n" as two literal characters where the file has a real
        // newline. Stage 4 unescapes and matches the real-character span.
        let content = "line one\nline two\n";
        let old = "line one\\nline two"; // backslash-n, not a real newline
        let res = applied(content, old, "single line", false);
        assert_eq!(res.strategy, EditStrategy::EscapeNormalized);
        assert_eq!(res.occurrences, 1);
        assert_eq!(res.new_content, "single line\n");
    }

    #[test]
    fn escape_normalized_literal_tab_in_old_text() {
        // Literal "\t" where the file has a real tab character.
        let content = "key\tvalue\n";
        let old = "key\\tvalue"; // backslash-t
        let res = applied(content, old, "key = value", false);
        assert_eq!(res.strategy, EditStrategy::EscapeNormalized);
        assert_eq!(res.new_content, "key = value\n");
    }

    #[test]
    fn escape_normalized_mixed_newline_and_tab() {
        let content = "a\n\tb\n";
        let old = "a\\n\\tb"; // \n then \t, both literal
        let res = applied(content, old, "done", false);
        assert_eq!(res.strategy, EditStrategy::EscapeNormalized);
        assert_eq!(res.new_content, "done\n");
    }

    #[test]
    fn escape_normalized_new_text_written_verbatim_with_real_escapes() {
        // The replacement text may itself contain real newlines; it is written
        // back verbatim (no unescaping of new_text).
        let content = "x\ny\n";
        let old = "x\\ny"; // matches the real "x\ny"
        let new = "p\nq"; // real newline in the replacement
        let res = applied(content, old, new, false);
        assert_eq!(res.strategy, EditStrategy::EscapeNormalized);
        assert_eq!(res.new_content, "p\nq\n");
    }

    #[test]
    fn escape_normalized_respects_uniqueness() {
        let content = "a\nb\nc\na\nb\nc\n";
        let old = "a\\nb"; // unescapes to "a\nb", which occurs twice
        let err = apply_edit(content, old, "z", false).unwrap_err();
        match err {
            EditError::Ambiguous { count, .. } => assert_eq!(count, 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn escape_normalized_does_not_fire_without_literal_escapes() {
        // old_text has no backslash escapes, so stage 4 never runs and a genuine
        // mismatch stays NotFound (no fuzzy rescue).
        let content = "alpha\nbeta\n";
        let err = apply_edit(content, "alpha beta gamma", "x", false).unwrap_err();
        assert_eq!(err, EditError::NotFound);
    }

    #[test]
    fn escape_normalized_does_not_match_when_real_chars_absent() {
        // old_text uses "\n" but the file has the literal two characters, NOT a
        // real newline. Stage 1 already matches the literal form, so stage 4 must
        // not produce a different (wrong) interpretation. Here the exact stage
        // wins because the file literally contains backslash-n.
        let content = "a\\nb\n"; // file literally contains backslash, n
        let res = applied(content, "a\\nb", "c", false);
        assert_eq!(res.strategy, EditStrategy::Exact);
        assert_eq!(res.new_content, "c\n");
    }

    #[test]
    fn escape_normalized_multibyte_safe() {
        // Real newline between multibyte content; literal "\n" in old_text.
        let content = "café\nثعلب\n";
        let old = "café\\nثعلب";
        let res = applied(content, old, "ok", false);
        assert_eq!(res.strategy, EditStrategy::EscapeNormalized);
        assert_eq!(res.new_content, "ok\n");
    }

    // --- Trailing-newline absorption on deletion (deletion-preference) -----

    #[test]
    fn trailing_newline_absorbed_on_pure_deletion() {
        // Deleting a whole line: new_text empty, old_text has no trailing newline,
        // but old_text + "\n" matches. The newline is removed too, leaving no
        // orphan blank line.
        let content = "keep\nremove me\nkeep too\n";
        let res = applied(content, "remove me", "", false);
        assert_eq!(res.strategy, EditStrategy::TrailingNewline);
        assert_eq!(res.occurrences, 1);
        assert_eq!(res.new_content, "keep\nkeep too\n");
    }

    #[test]
    fn trailing_newline_not_used_when_new_text_nonempty() {
        // Replacement (not deletion): the trailing-newline stage must not engage;
        // an exact substring replacement keeps the newline.
        let content = "keep\nremove me\nkeep too\n";
        let res = applied(content, "remove me", "kept", false);
        assert_eq!(res.strategy, EditStrategy::Exact);
        assert_eq!(res.new_content, "keep\nkept\nkeep too\n");
    }

    #[test]
    fn trailing_newline_deletion_exact_wins_when_old_text_has_newline() {
        // old_text already ends in "\n", so the dedicated trailing stage is not
        // needed; exact deletion handles it and removes exactly that span.
        let content = "keep\nremove me\nkeep too\n";
        let res = applied(content, "remove me\n", "", false);
        assert_eq!(res.strategy, EditStrategy::Exact);
        assert_eq!(res.new_content, "keep\nkeep too\n");
    }

    #[test]
    fn trailing_newline_respects_uniqueness() {
        // The line appears twice; deletion without replace_all is ambiguous.
        let content = "dup\nmid\ndup\n";
        let err = apply_edit(content, "dup", "", false).unwrap_err();
        match err {
            EditError::Ambiguous { count, .. } => assert_eq!(count, 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn trailing_newline_replace_all_deletes_all_with_newlines() {
        let content = "dup\nmid\ndup\n";
        let res = applied(content, "dup", "", true);
        assert_eq!(res.strategy, EditStrategy::TrailingNewline);
        assert_eq!(res.occurrences, 2);
        assert_eq!(res.new_content, "mid\n");
    }

    #[test]
    fn trailing_newline_absorption_skipped_when_no_newline_after_match() {
        // old_text occurs at end of file with no trailing newline, and there is
        // no "old_text + \n" anywhere. The trailing-newline preference is skipped
        // (its target is absent) and exact deletion of the substring takes over.
        let content = "alpha tail";
        let res = applied(content, "tail", "", false);
        assert_eq!(res.strategy, EditStrategy::Exact);
        assert_eq!(res.new_content, "alpha ");
    }

    #[test]
    fn trailing_newline_does_not_match_different_text() {
        // A genuinely absent line must not be deleted by the trailing-newline
        // stage (no fuzzy rescue on deletions either).
        let content = "one\ntwo\n";
        let err = apply_edit(content, "three", "", false).unwrap_err();
        assert_eq!(err, EditError::NotFound);
    }

    // --- Cross-stage ordering / negative fuzz guards -----------------------

    #[test]
    fn stage_ordering_exact_beats_quote_and_escape() {
        // When an exact match exists, none of the new stages should be consulted.
        let content = "value = \"plain\";\n";
        let res = applied(content, "\"plain\"", "\"PLAIN\"", false);
        assert_eq!(res.strategy, EditStrategy::Exact);
        assert_eq!(res.new_content, "value = \"PLAIN\";\n");
    }

    #[test]
    fn no_new_stage_turns_a_clear_mismatch_into_a_match() {
        // A string that differs in more than quotes/escapes/trailing-newline must
        // remain NotFound across ALL stages (the central anti-fuzz guarantee).
        let content = format!("greeting = {LDQUO}hello there{RDQUO}\n");
        // Different words AND quote style AND a literal escape: still no match.
        let err = apply_edit(&content, "greeting = \"farewell\\nfriend\"", "x", false).unwrap_err();
        assert_eq!(err, EditError::NotFound);
    }

    #[test]
    fn new_stages_preserve_crlf_surroundings() {
        // Quote-normalized match inside CRLF content: surrounding CRLFs intact.
        let content = format!("a\r\nmsg = {LDQUO}hi{RDQUO}\r\nb\r\n");
        let res = applied(&content, "msg = \"hi\"", "msg = \"bye\"", false);
        assert_eq!(res.strategy, EditStrategy::QuoteNormalized);
        let expected = format!("a\r\nmsg = {LDQUO}bye{RDQUO}\r\nb\r\n");
        assert_eq!(res.new_content, expected);
        assert!(res.new_content.contains("\r\n"));
    }

    #[test]
    fn quote_normalized_bom_preserved() {
        let content = format!("\u{FEFF}t = {LDQUO}a{RDQUO}\n");
        let res = applied(&content, "t = \"a\"", "t = \"b\"", false);
        assert!(res.new_content.starts_with('\u{FEFF}'));
        let expected = format!("\u{FEFF}t = {LDQUO}b{RDQUO}\n");
        assert_eq!(res.new_content, expected);
    }

    #[test]
    fn no_change_guard_precedes_new_stages() {
        // old == new is a no-op regardless of quotes/escapes being present.
        let content = format!("x = {LDQUO}a{RDQUO}\n");
        let old = format!("x = {LDQUO}a{RDQUO}");
        let err = apply_edit(&content, &old, &old, false).unwrap_err();
        assert_eq!(err, EditError::NoChange);
    }

    #[test]
    fn unescape_n_t_helper_only_touches_n_and_t() {
        // Direct unit check: only \n and \t are unescaped; \r, \", \\ are left
        // verbatim, and a string with no \n/\t yields None.
        assert_eq!(unescape_n_t("a\\nb").as_deref(), Some("a\nb"));
        assert_eq!(unescape_n_t("a\\tb").as_deref(), Some("a\tb"));
        assert_eq!(unescape_n_t("plain text"), None);
        assert_eq!(unescape_n_t("a\\rb"), None); // \r not in scope
        assert_eq!(unescape_n_t("a\\\"b"), None); // escaped quote not in scope
    }

    #[test]
    fn fold_quotes_with_map_round_trips_offsets() {
        // The offset map must point every folded char boundary back to the
        // original char start, including across multibyte content.
        let content = format!("a{LDQUO}世{RDQUO}b");
        let (folded, orig_at) = fold_quotes_with_map(&content);
        assert_eq!(folded, "a\"世\"b");
        // For each char boundary in the folded string, the mapped original slice
        // must itself be valid (no panic) and the whole map ends at content.len().
        assert_eq!(*orig_at.last().unwrap(), content.len());
        let mut folded_pos = 0;
        for fch in folded.chars() {
            let orig = orig_at[folded_pos];
            // Slicing the original at the mapped offset must land on a boundary.
            assert!(content.is_char_boundary(orig));
            folded_pos += fch.len_utf8();
        }
    }
}
