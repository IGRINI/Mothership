//! Dependency-free line diff for the change journal.
//!
//! Two outputs are needed: the `+N -M` counts stored on each change file (cheap,
//! computed at capture time for every changed file) and a unified-diff rendering
//! for the lazy per-file view (computed on demand). Counts use an O(min) two-row
//! LCS-length pass; the unified rendering uses a full LCS table and is bounded so
//! a pathologically large file can never blow up memory — beyond the cap it falls
//! back to a whole-file replace rendering (and the caller marks the file large).

/// Per-side line cap. Files whose before/after exceeds this are marked "large"
/// by the service (counts skipped, diff not rendered) rather than diffed — this
/// bounds both the LCS table memory (O(n·m)) and time deterministically. Source
/// files comfortably fit; it only excludes multi-thousand-line generated blobs.
pub const MAX_DIFF_LINES: usize = 3000;

/// Whether `text` has more *logical* lines than [`MAX_DIFF_LINES`] — the same
/// count [`split_lines`] produces, so the service marks "large" exactly when the
/// LCS path would otherwise overflow the cap. A file with no trailing newline has
/// one more logical line than it has `\n` bytes, so a 3001-line file with 3000
/// newlines is still over the cap. Early-exits without scanning the whole file.
pub fn exceeds_line_cap(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let mut newlines = 0usize;
    for &byte in text.as_bytes() {
        if byte == b'\n' {
            newlines += 1;
            if newlines > MAX_DIFF_LINES {
                return true;
            }
        }
    }
    let logical = if text.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    };
    logical > MAX_DIFF_LINES
}

/// Additions/deletions for one file, derived from line-level LCS.
pub fn diff_counts(before: &str, after: &str) -> (u32, u32) {
    let a = split_lines(before);
    let b = split_lines(after);
    if a.is_empty() && b.is_empty() {
        return (0, 0);
    }
    if a.len() > MAX_DIFF_LINES || b.len() > MAX_DIFF_LINES {
        // Naive fallback: treat it as a full replace.
        return (b.len() as u32, a.len() as u32);
    }
    let lcs = lcs_len(&a, &b);
    let deletions = (a.len() - lcs) as u32;
    let additions = (b.len() - lcs) as u32;
    (additions, deletions)
}

/// A rendered unified diff plus its counts.
pub struct UnifiedDiff {
    pub lines: Vec<String>,
    pub additions: u32,
    pub deletions: u32,
}

/// Render a unified diff (with `@@` hunk headers and 3 lines of context) between
/// `before` and `after`.
pub fn unified_diff(before: &str, after: &str) -> UnifiedDiff {
    unified_diff_with_context(before, after, 3)
}

/// Render a unified diff with `context` lines of context around each change.
/// A `context` of [`usize::MAX`] yields a single hunk spanning the whole file —
/// every unchanged line kept as context — so the caller can show the full file
/// with its edits marked in place.
pub fn unified_diff_with_context(before: &str, after: &str, context: usize) -> UnifiedDiff {
    let a = split_lines(before);
    let b = split_lines(after);
    if a.len() > MAX_DIFF_LINES || b.len() > MAX_DIFF_LINES {
        return naive_unified(&a, &b);
    }

    let ops = lcs_ops(&a, &b);
    build_unified(&ops, context)
}

/// Split a fragment into lines, stripping a trailing `\r` so CRLF text diffs
/// cleanly. A trailing newline does not yield a spurious final blank line; a
/// genuinely empty fragment yields no lines.
pub(crate) fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.len() > 1 {
        if let Some(last) = lines.last() {
            if last.is_empty() {
                lines.pop();
            }
        }
    }
    lines
}

/// Length of the longest common subsequence of two line slices, in O(min) space.
fn lcs_len(a: &[&str], b: &[&str]) -> usize {
    let n = b.len();
    let mut prev = vec![0usize; n + 1];
    let mut curr = vec![0usize; n + 1];
    for &ai in a {
        for j in 0..n {
            curr[j + 1] = if ai == b[j] {
                prev[j] + 1
            } else {
                curr[j].max(prev[j + 1])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// One aligned diff operation.
enum DiffOp<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

/// Full LCS table + backtrack into an alignment of [`DiffOp`]s.
fn lcs_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<DiffOp<'a>> {
    let m = a.len();
    let n = b.len();
    // dp[i][j] = LCS length of a[i..] and b[j..].
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut ops = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < m && j < n {
        if a[i] == b[j] {
            ops.push(DiffOp::Equal(a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(DiffOp::Delete(a[i]));
            i += 1;
        } else {
            ops.push(DiffOp::Insert(b[j]));
            j += 1;
        }
    }
    while i < m {
        ops.push(DiffOp::Delete(a[i]));
        i += 1;
    }
    while j < n {
        ops.push(DiffOp::Insert(b[j]));
        j += 1;
    }
    ops
}

/// Group an alignment into unified-diff hunks with `context` lines of context
/// around each change (`usize::MAX` => one hunk covering the whole file).
fn build_unified(ops: &[DiffOp<'_>], context: usize) -> UnifiedDiff {
    // (tag, old_line, new_line, text). A `0` line number means "n/a" for that
    // side (a deletion has no new line; an insertion has no old line).
    let mut entries: Vec<(char, usize, usize, &str)> = Vec::with_capacity(ops.len());
    let (mut old_no, mut new_no) = (1usize, 1usize);
    for op in ops {
        match op {
            DiffOp::Equal(text) => {
                entries.push((' ', old_no, new_no, text));
                old_no += 1;
                new_no += 1;
            }
            DiffOp::Delete(text) => {
                entries.push(('-', old_no, 0, text));
                old_no += 1;
            }
            DiffOp::Insert(text) => {
                entries.push(('+', 0, new_no, text));
                new_no += 1;
            }
        }
    }

    let additions = entries.iter().filter(|entry| entry.0 == '+').count() as u32;
    let deletions = entries.iter().filter(|entry| entry.0 == '-').count() as u32;

    let change_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.0 != ' ')
        .map(|(index, _)| index)
        .collect();
    if change_indices.is_empty() {
        return UnifiedDiff {
            lines: Vec::new(),
            additions,
            deletions,
        };
    }

    // Expand each change by CONTEXT and merge overlapping/adjacent windows.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for &index in &change_indices {
        let start = index.saturating_sub(context);
        let end = index.saturating_add(context).min(entries.len() - 1);
        match hunks.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => hunks.push((start, end)),
        }
    }

    let mut lines = Vec::new();
    for (start, end) in hunks {
        let slice = &entries[start..=end];
        let old_start = slice
            .iter()
            .find_map(|entry| (entry.1 > 0).then_some(entry.1))
            .unwrap_or(0);
        let new_start = slice
            .iter()
            .find_map(|entry| (entry.2 > 0).then_some(entry.2))
            .unwrap_or(0);
        let old_len = slice
            .iter()
            .filter(|entry| entry.0 == ' ' || entry.0 == '-')
            .count();
        let new_len = slice
            .iter()
            .filter(|entry| entry.0 == ' ' || entry.0 == '+')
            .count();
        lines.push(format!(
            "@@ -{old_start},{old_len} +{new_start},{new_len} @@"
        ));
        for entry in slice {
            lines.push(format!("{}{}", entry.0, entry.3));
        }
    }

    UnifiedDiff {
        lines,
        additions,
        deletions,
    }
}

/// Whole-file replace rendering used when a file is too large for the LCS table.
fn naive_unified(a: &[&str], b: &[&str]) -> UnifiedDiff {
    let mut lines = Vec::with_capacity(a.len() + b.len() + 1);
    lines.push(format!("@@ -1,{} +1,{} @@", a.len(), b.len()));
    for line in a {
        lines.push(format!("-{line}"));
    }
    for line in b {
        lines.push(format!("+{line}"));
    }
    UnifiedDiff {
        lines,
        additions: b.len() as u32,
        deletions: a.len() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_modified_lines() {
        let before = "a\nb\nc\n";
        let after = "a\nB\nc\nd\n";
        // b -> B is one delete + one insert; d is one insert.
        let (add, del) = diff_counts(before, after);
        assert_eq!((add, del), (2, 1));
    }

    #[test]
    fn counts_added_file() {
        let (add, del) = diff_counts("", "x\ny\n");
        assert_eq!((add, del), (2, 0));
    }

    #[test]
    fn counts_deleted_file() {
        let (add, del) = diff_counts("x\ny\nz\n", "");
        assert_eq!((add, del), (0, 3));
    }

    #[test]
    fn unified_has_hunk_header_and_changed_lines() {
        let diff = unified_diff("a\nb\nc\n", "a\nB\nc\n");
        assert!(diff.lines.iter().any(|line| line.starts_with("@@")));
        assert!(diff.lines.iter().any(|line| line == "-b"));
        assert!(diff.lines.iter().any(|line| line == "+B"));
        assert!(diff.lines.iter().any(|line| line == " a"));
        assert_eq!((diff.additions, diff.deletions), (1, 1));
    }

    #[test]
    fn unchanged_yields_no_diff_lines() {
        let diff = unified_diff("a\nb\n", "a\nb\n");
        assert!(diff.lines.is_empty());
        assert_eq!((diff.additions, diff.deletions), (0, 0));
    }

    #[test]
    fn full_context_keeps_lines_far_from_the_change() {
        // A single edit in the middle of a 9-line file. The default 3-line
        // context drops the outermost lines; full context keeps every one.
        let before = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let after = "1\n2\n3\n4\nFIVE\n6\n7\n8\n9\n";

        let windowed = unified_diff(before, after);
        assert!(!windowed.lines.iter().any(|line| line == " 1"));
        assert!(!windowed.lines.iter().any(|line| line == " 9"));

        let full = unified_diff_with_context(before, after, usize::MAX);
        assert!(full.lines.iter().any(|line| line == " 1"));
        assert!(full.lines.iter().any(|line| line == " 9"));
        assert!(full.lines.iter().any(|line| line == "+FIVE"));
        assert!(full.lines.iter().any(|line| line == "-5"));
        // One hunk covering the whole file.
        assert_eq!(
            full.lines.iter().filter(|line| line.starts_with("@@")).count(),
            1
        );
    }

    #[test]
    fn line_cap_counts_logical_lines_including_missing_trailing_newline() {
        assert!(!exceeds_line_cap(""));
        // Exactly at the cap (with trailing newline) is allowed.
        let at_cap = "x\n".repeat(MAX_DIFF_LINES);
        assert!(!exceeds_line_cap(&at_cap));
        // One over the cap, WITHOUT a trailing newline: MAX newlines but
        // MAX+1 logical lines — must be flagged.
        let over = format!("{}{}", "x\n".repeat(MAX_DIFF_LINES), "tail");
        assert!(exceeds_line_cap(&over));
    }
}
