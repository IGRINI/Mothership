// Unified-diff parsing utilities shared by DiffBlock and the per-file patch view.
//
// CONTRACT (chosen up front, per review): diff artifacts produced by the backend
// (`edit_file`, `apply_patch`) are *unified diffs*. The UI parses them — it never
// guesses a bespoke shape. If a tool ever emits something that is not a unified
// diff, the fix is on the backend (emit unified), not heuristics here.

export interface DiffRow {
  tone: "add" | "del" | "ctx" | "hunk" | "meta";
  /** 1-based line number in the OLD file (absent for added lines). */
  oldNo?: number;
  /** 1-based line number in the NEW file (absent for removed lines). */
  newNo?: number;
  /** The line content WITHOUT the leading +/-/space marker. */
  text: string;
}

/** Parse a unified diff into display rows, tracking old/new line numbers. */
export function parseUnifiedHunks(diff: string): DiffRow[] {
  const rows: DiffRow[] = [];
  let oldNo = 0;
  let newNo = 0;
  for (const raw of diff.replace(/\r\n/g, "\n").split("\n")) {
    if (raw.startsWith("@@")) {
      const match = /@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
      if (match) {
        oldNo = Number.parseInt(match[1], 10);
        newNo = Number.parseInt(match[2], 10);
      }
      rows.push({ tone: "hunk", text: raw });
      continue;
    }
    if (isMetaLine(raw)) {
      rows.push({ tone: "meta", text: raw });
      continue;
    }
    if (raw.startsWith("+")) {
      rows.push({ tone: "add", newNo: newNo++, text: raw.slice(1) });
      continue;
    }
    if (raw.startsWith("-")) {
      rows.push({ tone: "del", oldNo: oldNo++, text: raw.slice(1) });
      continue;
    }
    const text = raw.startsWith(" ") ? raw.slice(1) : raw;
    rows.push({ tone: "ctx", oldNo: oldNo++, newNo: newNo++, text });
  }
  return rows;
}

function isMetaLine(line: string): boolean {
  return (
    line.startsWith("+++") ||
    line.startsWith("---") ||
    line.startsWith("diff ") ||
    line.startsWith("index ") ||
    line.startsWith("new file") ||
    line.startsWith("deleted file") ||
    line.startsWith("rename ") ||
    line.startsWith("copy ") ||
    line.startsWith("similarity ") ||
    line.startsWith("old mode") ||
    line.startsWith("new mode") ||
    line.startsWith("\\ No newline")
  );
}

/**
 * Reconstruct the NEW-side file content from a unified diff: context + added
 * lines, carrying their new-file line numbers (deletions dropped). For a newly
 * written file the diff is all additions, so this yields the whole file — which
 * is what `write_file` shows.
 */
export function newSideLines(diff: string): { no: number; text: string }[] {
  const out: { no: number; text: string }[] = [];
  for (const row of parseUnifiedHunks(diff)) {
    if ((row.tone === "add" || row.tone === "ctx") && row.newNo !== undefined) {
      out.push({ no: row.newNo, text: row.text });
    }
  }
  return out;
}

/** Count added / removed lines in a unified diff (ignoring +++/--- headers). */
export function diffStat(diff: string): { add: number; del: number } {
  let add = 0;
  let del = 0;
  for (const line of diff.replace(/\r\n/g, "\n").split("\n")) {
    if (line.startsWith("+") && !line.startsWith("+++")) {
      add += 1;
    } else if (line.startsWith("-") && !line.startsWith("---")) {
      del += 1;
    }
  }
  return { add, del };
}

export type FileOp = "A" | "M" | "D";

export interface FileDiff {
  op: FileOp;
  /** Display path (new path for adds/mods, old path for deletes). */
  path: string;
  add: number;
  del: number;
  /** The unified-diff body for just this file (fed to DiffBlock). */
  body: string;
}

/**
 * Split a combined unified diff into one entry per file. Handles both
 * `git`-style diffs (`diff --git a/x b/x`) and bare `--- /+++ ` pairs.
 * Returns a single synthetic entry when the text has hunks but no file headers.
 */
export function splitUnifiedDiffByFile(diff: string): FileDiff[] {
  const normalized = diff.replace(/\r\n/g, "\n");
  const lines = normalized.split("\n");
  const blocks: string[][] = [];
  let current: string[] | null = null;

  for (const line of lines) {
    const startsGitBlock = line.startsWith("diff --git ");
    const startsBareBlock =
      !current && line.startsWith("--- ") && !line.startsWith("--- a/");
    if (startsGitBlock) {
      if (current) {
        blocks.push(current);
      }
      current = [line];
      continue;
    }
    // A new `--- a/…` after we already have content also begins a file.
    if (current && line.startsWith("--- ") && hasHunk(current)) {
      blocks.push(current);
      current = [line];
      continue;
    }
    if (!current && (line.startsWith("--- ") || startsBareBlock)) {
      current = [line];
      continue;
    }
    if (current) {
      current.push(line);
    }
  }
  if (current) {
    blocks.push(current);
  }

  const files = blocks
    .map((block) => block.join("\n"))
    .filter((body) => body.trim().length > 0)
    .map(toFileDiff);

  if (files.length === 0 && normalized.trim().length > 0) {
    const stat = diffStat(normalized);
    return [{ op: "M", path: "", add: stat.add, del: stat.del, body: normalized }];
  }
  return files;
}

function hasHunk(lines: string[]): boolean {
  return lines.some((line) => line.startsWith("@@"));
}

function toFileDiff(body: string): FileDiff {
  const lines = body.split("\n");
  let oldPath: string | undefined;
  let newPath: string | undefined;
  let op: FileOp = "M";

  for (const line of lines) {
    if (line.startsWith("diff --git ")) {
      const match = /diff --git a\/(.+?) b\/(.+)$/.exec(line);
      if (match) {
        oldPath = match[1];
        newPath = match[2];
      }
    } else if (line.startsWith("new file")) {
      op = "A";
    } else if (line.startsWith("deleted file")) {
      op = "D";
    } else if (line.startsWith("--- ")) {
      const path = stripDiffPath(line.slice(4));
      if (path && path !== "/dev/null") {
        oldPath = path;
      } else if (path === "/dev/null") {
        op = "A";
      }
    } else if (line.startsWith("+++ ")) {
      const path = stripDiffPath(line.slice(4));
      if (path && path !== "/dev/null") {
        newPath = path;
      } else if (path === "/dev/null") {
        op = "D";
      }
    }
  }

  const stat = diffStat(body);
  const path = op === "D" ? oldPath ?? newPath ?? "" : newPath ?? oldPath ?? "";
  return { op, path, add: stat.add, del: stat.del, body };
}

function stripDiffPath(raw: string): string {
  // Trim a trailing tab+timestamp, then a leading a/ or b/ prefix.
  const path = raw.split("\t")[0].trim();
  if (path.startsWith("a/") || path.startsWith("b/")) {
    return path.slice(2);
  }
  return path;
}
