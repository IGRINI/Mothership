// Helpers for turning tool output into structured, line-numbered code models.

export interface CodeLineModel {
  /** 1-based line number shown in the gutter. */
  no: number;
  text: string;
  /** Part of the region the model actually read (highlighted background). */
  read?: boolean;
  /** Surrounding context loaded around the read region (dimmed). */
  context?: boolean;
}

// `read_file` renders each line as `{number:6}→{content}` (U+2192 RIGHTWARDS
// ARROW, not a tab — see render_numbered in file_tools.rs). Parse that back into
// structured lines, dropping the trailing footer notes the tool appends
// (truncation markers, "full content: …", "(total lines …)").
const NUMBERED_LINE = /^\s*(\d+)→(.*)$/;

export function parseNumberedOutput(output: string): CodeLineModel[] {
  const lines: CodeLineModel[] = [];
  for (const raw of output.replace(/\r\n/g, "\n").split("\n")) {
    const match = NUMBERED_LINE.exec(raw);
    if (match) {
      lines.push({ no: Number.parseInt(match[1], 10), text: match[2] });
    }
  }
  return lines;
}

/** Split a path into a muted directory part and a bright file-name part. */
export function splitPath(path: string): { dir: string; name: string } {
  const normalized = path.replace(/\\/g, "/");
  const at = normalized.lastIndexOf("/");
  if (at < 0) {
    return { dir: "", name: path };
  }
  return { dir: path.slice(0, at + 1), name: path.slice(at + 1) };
}
