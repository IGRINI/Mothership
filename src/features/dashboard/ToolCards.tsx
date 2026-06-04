import { For, JSX, Show, createSignal } from "solid-js";
import {
  FilePlus,
  FileSearch,
  FileText,
  GitCompare,
  ListTree,
  Terminal,
} from "lucide-solid";

import type { ToolArtifact, ToolKind } from "../../shared/api/mothership";
import { CodeBlock } from "./components/CodeBlock";
import { DiffBlock } from "./components/DiffBlock";
import { OutputBlock } from "./components/OutputBlock";
import { parseNumberedOutput, splitPath, type CodeLineModel } from "./components/code";
import { highlightCommand } from "./components/highlight";
import { diffStat, newSideLines } from "./components/diff";

// Opens a workspace path in an external app. Goes through a capability-checked
// Core/Tauri command — never a raw shell open of UI-supplied data.
export type OpenPathFn = (path: string) => void;

// Lazily fetches a byte range of a tool's *persisted* output artifact (the
// snapshot taken at tool-call time), NOT the live file — which may have changed.
// Keyed by tool_call_id + the artifact's durable logRef.
export type LoadArtifactRangeFn = (args: {
  toolCallId: string;
  logRef: string;
  offset: number;
  limit: number;
}) => Promise<{ content: string; nextOffset: number | null; eof: boolean }>;

// One generous chunk is enough for the overwhelmingly common case (files under
// a few hundred KB). Larger files load the first chunk and point at "Открыть".
const FULL_LOAD_LIMIT = 256 * 1024;

// Minimal structural view of a tool execution. Kept loose on purpose so this
// module does not depend on Dashboard's internal `ToolExecutionView` shape — it
// only reads the fields it knows how to render.
export interface ToolCardData {
  kind: string;
  toolCallId?: string;
  toolKind?: ToolKind;
  payload?: Record<string, unknown> | null;
  touchedPaths?: string[];
  artifacts?: ToolArtifact[];
  message?: string | null;
  /** Model-facing text (line-numbered read window, command stdout, …). */
  output?: string;
}

// --- Defensive payload accessors -------------------------------------------
// `payload` is `Record<string, unknown>`; never assume a field exists or has a
// given type. These readers return undefined when the field is absent or the
// wrong type, so cards simply omit anything they cannot show.

function readString(
  payload: Record<string, unknown> | null | undefined,
  key: string,
): string | undefined {
  const value = payload?.[key];
  return typeof value === "string" ? value : undefined;
}

function readNumber(
  payload: Record<string, unknown> | null | undefined,
  key: string,
): number | undefined {
  const value = payload?.[key];
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function readBoolean(
  payload: Record<string, unknown> | null | undefined,
  key: string,
): boolean | undefined {
  const value = payload?.[key];
  return typeof value === "boolean" ? value : undefined;
}

function readStringArray(
  payload: Record<string, unknown> | null | undefined,
  key: string,
): string[] | undefined {
  const value = payload?.[key];
  if (!Array.isArray(value)) {
    return undefined;
  }
  const strings = value.filter(
    (item): item is string => typeof item === "string",
  );
  return strings.length > 0 ? strings : undefined;
}

function shortSha(value: string | undefined): string | undefined {
  if (!value) {
    return undefined;
  }
  return value.length > 12 ? value.slice(0, 12) : value;
}

function formatBytes(bytes: number | undefined): string | undefined {
  if (bytes === undefined) {
    return undefined;
  }
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

// --- Small presentational primitives ---------------------------------------

function Chip(props: { label: string; value: JSX.Element }) {
  return (
    <span class="tool-card__chip">
      <span class="tool-card__chip-key">{props.label}</span>
      <span class="tool-card__chip-value">{props.value}</span>
    </span>
  );
}

function StatusPill(props: { status?: string }) {
  return (
    <Show when={props.status}>
      <span class={`tool-card__pill tool-card__pill--${statusTone(props.status)}`}>
        {props.status}
      </span>
    </Show>
  );
}

function statusTone(status: string | undefined): string {
  const normalized = (status ?? "").toLowerCase();
  if (
    normalized === "created" ||
    normalized === "modified" ||
    normalized === "ok" ||
    normalized === "applied" ||
    normalized === "success" ||
    normalized === "complete"
  ) {
    return "ok";
  }
  if (
    normalized === "failed" ||
    normalized === "error" ||
    normalized === "denied" ||
    normalized === "too_large"
  ) {
    return "error";
  }
  if (
    normalized === "partial" ||
    normalized === "lossy" ||
    normalized === "truncated" ||
    normalized === "skipped"
  ) {
    return "warn";
  }
  return "neutral";
}

function PathLabel(props: { path?: string }) {
  const parts = () => splitPath(props.path ?? "");
  return (
    <Show when={props.path}>
      <code class="tool-card__path" title={props.path}>
        <Show when={parts().dir}>
          <span class="tool-card__path-dir">{parts().dir}</span>
        </Show>
        <span class="tool-card__path-name">{parts().name}</span>
      </code>
    </Show>
  );
}

// Locate a completed mutating tool's unified-diff artifact, if present. Only the
// completed `diff` artifact — the approval `diff-preview` is rendered separately
// by ApprovalPreview, so it is intentionally excluded here.
function findDiffArtifact(
  artifacts: ToolArtifact[] | undefined,
): ToolArtifact | undefined {
  return artifacts?.find((artifact) => artifact.kind === "diff");
}

/** Header diff-stat (+N −M and an op badge) for a mutating tool, when it carries
 *  a diff artifact. Used by the row header in Dashboard's InlineToolCall. */
export function toolDiffStat(
  tool: ToolCardData,
): { add: number; del: number; op?: "M" | "A" | "D" } | undefined {
  const artifact = findDiffArtifact(tool.artifacts);
  if (!artifact) {
    return undefined;
  }
  const stat = diffStat(artifact.preview);
  let op: "M" | "A" | "D" | undefined;
  if (tool.toolKind === "edit_file") {
    op = "M";
  } else if (tool.toolKind === "write_file") {
    // A brand-new file has no deletions (A); overwriting an existing one does (M).
    op = stat.del > 0 ? "M" : "A";
  }
  return { ...stat, op };
}

// --- Before-approval preview ------------------------------------------------
// When a tool is `permission_requested`, the backend packs a human-readable
// preview into `message`: a summary, then (optionally) a blank line, then a
// bounded unified diff. Split on the first blank line so the diff portion can
// be rendered structurally; otherwise show the whole thing as a plain preview.

function splitApprovalPreview(message: string): { summary: string; diff?: string } {
  const normalized = message.replace(/\r\n/g, "\n");
  const separator = normalized.indexOf("\n\n");
  if (separator === -1) {
    return { summary: normalized.trim() };
  }
  const summary = normalized.slice(0, separator).trim();
  const rest = normalized.slice(separator + 2);
  const looksLikeDiff = rest
    .split("\n")
    .some(
      (line) =>
        line.startsWith("@@") ||
        line.startsWith("+++") ||
        line.startsWith("---") ||
        line.startsWith("diff "),
    );
  if (!looksLikeDiff) {
    return { summary: normalized.trim() };
  }
  return { summary, diff: rest.replace(/\s+$/, "") };
}

export function ApprovalPreview(props: {
  message: string;
  artifacts?: ToolArtifact[];
}) {
  // Prefer the typed `diff-preview` artifact (a real protocol object carried in
  // the event + persisted to tool_artifacts). Fall back to splitting the legacy
  // message string only when no artifact is present.
  const previewArtifact = () =>
    props.artifacts?.find((artifact) => artifact.kind === "diff-preview");
  const parts = () => splitApprovalPreview(props.message);
  return (
    <div class="tool-card__approval">
      <div class="tool-card__approval-head">
        <GitCompare size={14} />
        <span>Pending change — review before approving</span>
      </div>
      <Show
        when={previewArtifact()}
        fallback={
          <Show
            when={parts().diff}
            fallback={<pre class="tool-card__preview">{props.message}</pre>}
          >
            {(diff) => (
              <>
                <Show when={parts().summary}>
                  <p class="tool-card__approval-summary">{parts().summary}</p>
                </Show>
                <DiffBlock diff={diff()} />
              </>
            )}
          </Show>
        }
      >
        {(artifact) => (
          <>
            <Show when={parts().summary}>
              <p class="tool-card__approval-summary">{parts().summary}</p>
            </Show>
            <DiffBlock diff={artifact().preview} />
          </>
        )}
      </Show>
    </div>
  );
}

// --- Per-kind semantic bodies ----------------------------------------------

function CommandLine(props: { text: string }) {
  const tokens = () => highlightCommand(props.text);
  return (
    <div class="tool-cmd">
      <For each={tokens()}>
        {(token) => <span class={token.cls || undefined}>{token.text}</span>}
      </For>
    </div>
  );
}

function RunCommandCard(props: {
  payload: Record<string, unknown> | null | undefined;
}) {
  const program = () => readString(props.payload, "program");
  const args = () => readStringArray(props.payload, "args") ?? [];
  const exitCode = () => readNumber(props.payload, "exitCode");
  const stdout = () => readString(props.payload, "stdoutPreview")?.trim();
  const stderr = () => readString(props.payload, "stderrPreview")?.trim();
  const truncated = () => readBoolean(props.payload, "truncated");
  const logRef = () => readString(props.payload, "logRef");
  const commandLine = () =>
    [program() ?? "", ...args()].join(" ").trim() || "command";

  return (
    <div class="tool-body">
      <div class="tool-body__head">
        <CommandLine text={commandLine()} />
        <Show when={exitCode() !== undefined}>
          <span
            class={`tool-card__pill tool-card__pill--${
              exitCode() === 0 ? "ok" : "error"
            }`}
          >
            exit {exitCode()}
          </span>
        </Show>
      </div>
      <Show when={stdout()}>
        {(text) => <OutputBlock text={text()} />}
      </Show>
      <Show when={stderr()}>
        {(text) => <OutputBlock text={text()} tone="err" />}
      </Show>
      <Show when={truncated() || logRef()}>
        <div class="tool-card__chips">
          <Show when={truncated()}>
            <Chip label="output" value="truncated" />
          </Show>
          <Show when={logRef()}>
            {(ref) => <Chip label="log" value={<code>{ref()}</code>} />}
          </Show>
        </div>
      </Show>
    </div>
  );
}

function ReadFileCard(props: {
  payload: Record<string, unknown> | null | undefined;
  output?: string;
  toolCallId?: string;
  onOpen?: OpenPathFn;
  loadArtifactRange?: LoadArtifactRangeFn;
}) {
  const path = () => readString(props.payload, "path");
  const status = () => readString(props.payload, "status");
  const startLine = () =>
    readNumber(props.payload, "startLine") ?? readNumber(props.payload, "line");
  const endLine = () => readNumber(props.payload, "endLine");
  const totalLines = () => readNumber(props.payload, "totalLines");
  const sha = () => shortSha(readString(props.payload, "sha256"));
  const lossy = () => readBoolean(props.payload, "lossy");
  const logRef = () => readString(props.payload, "logRef");
  const lineRange = () => {
    const start = startLine();
    if (start === undefined) {
      return undefined;
    }
    const end = endLine();
    return end !== undefined && end !== start ? `${start}–${end}` : `${start}`;
  };

  const windowLines = () => parseNumberedOutput(props.output ?? "");
  // Paged lazy load of the persisted snapshot: accumulate chunks and keep the
  // button alive (as "Загрузить ещё") until the backend reports EOF.
  const [loadedLines, setLoadedLines] = createSignal<CodeLineModel[] | null>(null);
  const [nextOffset, setNextOffset] = createSignal<number | null>(null);
  const [started, setStarted] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const lines = () => loadedLines() ?? windowLines();

  const moreAvailable = () => !started() || nextOffset() !== null;
  const canLoadMore = () =>
    Boolean(logRef()) &&
    Boolean(props.loadArtifactRange) &&
    Boolean(props.toolCallId) &&
    moreAvailable();

  async function loadMore() {
    const ref = logRef();
    const id = props.toolCallId;
    const fetcher = props.loadArtifactRange;
    if (!ref || !id || !fetcher) {
      return;
    }
    if (started() && nextOffset() === null) {
      return; // already at EOF
    }
    const offset = started() ? nextOffset() ?? 0 : 0;
    setLoading(true);
    try {
      const result = await fetcher({
        toolCallId: id,
        logRef: ref,
        offset,
        limit: FULL_LOAD_LIMIT,
      });
      // Mark the originally-read window so it stays highlighted; the rest is
      // surrounding context.
      const readNumbers = new Set(windowLines().map((line) => line.no));
      const parsed = parseNumberedOutput(result.content).map((line) =>
        readNumbers.has(line.no)
          ? { ...line, read: true }
          : { ...line, context: true },
      );
      setLoadedLines((prev) => (started() && prev ? [...prev, ...parsed] : parsed));
      setNextOffset(result.eof ? null : result.nextOffset ?? null);
      setStarted(true);
    } finally {
      setLoading(false);
    }
  }

  return (
    <div class="tool-body">
      <div class="tool-body__head">
        <FileText size={13} />
        <PathLabel path={path()} />
        <StatusPill status={status()} />
      </div>
      <div class="tool-card__chips">
        <Show when={lineRange()}>
          {(range) => <Chip label="строки" value={range()} />}
        </Show>
        <Show when={totalLines() !== undefined}>
          <Chip label="всего" value={String(totalLines())} />
        </Show>
        <Show when={sha()}>
          {(value) => <Chip label="sha" value={<code>{value()}</code>} />}
        </Show>
        <Show when={lossy()}>
          <Chip label="кодировка" value="lossy" />
        </Show>
      </div>
      <Show when={lines().length > 0}>
        <CodeBlock
          lines={lines()}
          path={path()}
          title={path() ? splitPath(path() as string).name : undefined}
          onOpen={props.onOpen}
          onLoadMore={canLoadMore() ? loadMore : undefined}
          loadMoreLabel={started() ? "Загрузить ещё" : "Загрузить весь файл"}
          loading={loading()}
          atEnd={started() && nextOffset() === null}
        />
      </Show>
    </div>
  );
}

function WriteFileCard(props: {
  payload: Record<string, unknown> | null | undefined;
  artifacts?: ToolArtifact[];
  onOpen?: OpenPathFn;
}) {
  const path = () => readString(props.payload, "path");
  const status = () => readString(props.payload, "status");
  const bytes = () => readNumber(props.payload, "bytes");
  const sha = () => shortSha(readString(props.payload, "sha256"));
  // Present only on a `too_large` refusal.
  const contentBytes = () => readNumber(props.payload, "contentBytes");
  const maxWriteBytes = () => readNumber(props.payload, "maxWriteBytes");
  // The written file is the NEW side of the diff artifact (a brand-new file's
  // diff is all additions). Only reconstruct from a REAL unified diff — the
  // backend emits a text summary (no `@@`) for large writes, which must not be
  // shown as content. Bounded by the preview; the full file is via "Открыть".
  const realDiff = () => {
    const artifact = findDiffArtifact(props.artifacts);
    return artifact && artifact.preview.includes("@@") ? artifact : undefined;
  };
  const lines = (): CodeLineModel[] => {
    const artifact = realDiff();
    return artifact ? newSideLines(artifact.preview) : [];
  };
  const previewPartial = () => Boolean(realDiff()?.truncated);
  // A diff artifact exists but is a summary (large write) — no body to show.
  const summaryOnly = () =>
    Boolean(findDiffArtifact(props.artifacts)) && !realDiff();

  return (
    <div class="tool-body">
      <div class="tool-body__head">
        <FilePlus size={13} />
        <PathLabel path={path()} />
        <StatusPill status={status()} />
        <Show when={path() && props.onOpen}>
          <button
            class="code-block__open tool-body__open"
            type="button"
            title="Открыть во внешнем приложении"
            onClick={() => props.onOpen?.(path() as string)}
          >
            Открыть
          </button>
        </Show>
      </div>
      <div class="tool-card__chips">
        <Show when={formatBytes(bytes())}>
          {(value) => <Chip label="размер" value={value()} />}
        </Show>
        <Show when={sha()}>
          {(value) => <Chip label="sha" value={<code>{value()}</code>} />}
        </Show>
        <Show when={formatBytes(contentBytes())}>
          {(value) => <Chip label="content" value={value()} />}
        </Show>
        <Show when={formatBytes(maxWriteBytes())}>
          {(value) => <Chip label="limit" value={value()} />}
        </Show>
      </div>
      <Show when={lines().length > 0}>
        <CodeBlock
          lines={lines()}
          path={path()}
          title={path() ? splitPath(path() as string).name : undefined}
          onOpen={props.onOpen}
        />
      </Show>
      <Show when={previewPartial()}>
        <p class="tool-body__note">
          Показан фрагмент записанного файла — целиком через «Открыть».
        </p>
      </Show>
      <Show when={summaryOnly()}>
        <p class="tool-body__note">
          Большой файл — предпросмотр содержимого недоступен, откройте через «Открыть».
        </p>
      </Show>
    </div>
  );
}

function EditFileCard(props: {
  payload: Record<string, unknown> | null | undefined;
  artifacts?: ToolArtifact[];
  onOpen?: OpenPathFn;
}) {
  const path = () => readString(props.payload, "path");
  const status = () => readString(props.payload, "status");
  const occurrences = () => readNumber(props.payload, "occurrences");
  const diff = () => findDiffArtifact(props.artifacts);

  return (
    <div class="tool-body">
      <div class="tool-body__head">
        <FileText size={13} />
        <PathLabel path={path()} />
        <StatusPill status={status()} />
        <Show when={occurrences() !== undefined}>
          <span class="tool-body__hint">{occurrences()}×</span>
        </Show>
        <Show when={path() && props.onOpen}>
          <button
            class="code-block__open tool-body__open"
            type="button"
            title="Открыть во внешнем приложении"
            onClick={() => props.onOpen?.(path() as string)}
          >
            Открыть
          </button>
        </Show>
      </div>
      <Show when={diff()}>
        {(artifact) => <DiffBlock diff={artifact().preview} />}
      </Show>
    </div>
  );
}

interface PatchFileMeta {
  path: string;
  op: string;
  added: number;
  removed: number;
}

// apply_patch records real per-file metadata in its payload. The diff artifact
// is only a "# op path" summary (no hunks), so we deliberately do NOT render
// fake per-file diffs — we show op + path + add/remove counts from the payload.
// Real per-file hunks are a backend follow-up (emit a unified-diff artifact).
function readPatchFiles(value: unknown): PatchFileMeta[] {
  if (!Array.isArray(value)) {
    return [];
  }
  const out: PatchFileMeta[] = [];
  for (const entry of value) {
    if (entry && typeof entry === "object") {
      const record = entry as Record<string, unknown>;
      const path = record.path ?? record.file ?? record.name;
      if (typeof path === "string") {
        const op = record.op ?? record.status ?? record.kind;
        out.push({
          path,
          op: typeof op === "string" ? op : "modify",
          added: typeof record.added === "number" ? record.added : 0,
          removed: typeof record.removed === "number" ? record.removed : 0,
        });
      }
    }
  }
  return out;
}

function patchOpBadge(op: string): "A" | "M" | "D" {
  const normalized = op.toLowerCase();
  if (normalized === "add" || normalized === "create" || normalized === "added") {
    return "A";
  }
  if (
    normalized === "remove" ||
    normalized === "delete" ||
    normalized === "removed"
  ) {
    return "D";
  }
  return "M";
}

function PatchFileRow(props: { file: PatchFileMeta }) {
  const parts = () => splitPath(props.file.path);
  const badge = () => patchOpBadge(props.file.op);
  return (
    <div class="tool-sub tool-sub--static">
      <div class="tool-sub__summary tool-sub__summary--static">
        <span class={`tool-sub__op tool-sub__op--${badge()}`}>{badge()}</span>
        <code class="tool-sub__path">
          <Show when={parts().dir}>
            <span class="tool-sub__dir">{parts().dir}</span>
          </Show>
          <span class="tool-sub__name">{parts().name || "(файл)"}</span>
        </code>
        <span class="tool-sub__spacer" />
        <span class="tool-sub__stat">
          <Show when={props.file.added > 0}>
            <span class="tool-stat__add">+{props.file.added}</span>
          </Show>{" "}
          <Show when={props.file.removed > 0}>
            <span class="tool-stat__del">−{props.file.removed}</span>
          </Show>
        </span>
      </div>
    </div>
  );
}

function ApplyPatchCard(props: {
  payload: Record<string, unknown> | null | undefined;
  touchedPaths?: string[];
}) {
  const status = () => readString(props.payload, "status");
  const files = () => readPatchFiles(props.payload?.files);
  // Fallback list when there is no structured per-file metadata (legacy records).
  const fallbackPaths = () => {
    const fromPayload = normalizePatchFiles(props.payload?.files);
    return fromPayload.length > 0 ? fromPayload : props.touchedPaths ?? [];
  };

  return (
    <div class="tool-body">
      <div class="tool-body__head">
        <GitCompare size={13} />
        <span class="tool-card__title">Патч</span>
        <StatusPill status={status()} />
        <Show when={files().length > 0}>
          <span class="tool-body__hint">{files().length} файл(ов)</span>
        </Show>
      </div>
      <Show
        when={files().length > 0}
        fallback={
          <Show when={fallbackPaths().length > 0}>
            <ul class="tool-card__files">
              <For each={fallbackPaths()}>
                {(file) => (
                  <li>
                    <code>{file}</code>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        }
      >
        <div class="tool-subs">
          <For each={files()}>{(file) => <PatchFileRow file={file} />}</For>
        </div>
      </Show>
    </div>
  );
}

// `files` may be a list of bare path strings or a list of objects carrying a
// path/op. Normalize to display strings without assuming a single shape.
function normalizePatchFiles(value: unknown): string[] {
  if (!Array.isArray(value)) {
    return [];
  }
  const out: string[] = [];
  for (const entry of value) {
    if (typeof entry === "string") {
      out.push(entry);
    } else if (entry && typeof entry === "object") {
      const record = entry as Record<string, unknown>;
      const path = record.path ?? record.file ?? record.name;
      const op = record.op ?? record.status ?? record.kind;
      if (typeof path === "string") {
        out.push(typeof op === "string" ? `${op}: ${path}` : path);
      }
    }
  }
  return out;
}

function ListFilesCard(props: {
  payload: Record<string, unknown> | null | undefined;
  output?: string;
}) {
  const count = () => readNumber(props.payload, "count");
  const truncated = () => readBoolean(props.payload, "truncated");
  const dir = () => readString(props.payload, "dir");
  const glob = () => readString(props.payload, "glob");
  const listing = () => (props.output ?? "").trim();

  return (
    <div class="tool-body">
      <div class="tool-body__head">
        <ListTree size={13} />
        <span class="tool-card__title">Список</span>
        <Show when={dir()}>{(value) => <PathLabel path={value()} />}</Show>
      </div>
      <div class="tool-card__chips">
        <Show when={count() !== undefined}>
          <Chip label="файлов" value={String(count())} />
        </Show>
        <Show when={glob()}>
          {(value) => <Chip label="glob" value={<code>{value()}</code>} />}
        </Show>
        <Show when={truncated()}>
          <Chip label="результат" value="truncated" />
        </Show>
      </div>
      <Show when={listing()}>
        {(text) => <OutputBlock text={text()} />}
      </Show>
    </div>
  );
}

function SearchTextCard(props: {
  payload: Record<string, unknown> | null | undefined;
  output?: string;
}) {
  const pattern = () => readString(props.payload, "pattern");
  const count = () => readNumber(props.payload, "count");
  const filesWithMatches = () => readNumber(props.payload, "filesWithMatches");
  const truncated = () => readBoolean(props.payload, "truncated");
  const hits = () => (props.output ?? "").trim();

  return (
    <div class="tool-body">
      <div class="tool-body__head">
        <FileSearch size={13} />
        <span class="tool-card__title">Поиск</span>
        <Show when={pattern()}>
          {(value) => (
            <code class="tool-card__pattern" title={value()}>
              /{value()}/
            </code>
          )}
        </Show>
      </div>
      <div class="tool-card__chips">
        <Show when={count() !== undefined}>
          <Chip label="совпадений" value={String(count())} />
        </Show>
        <Show when={filesWithMatches() !== undefined}>
          <Chip label="файлов" value={String(filesWithMatches())} />
        </Show>
        <Show when={truncated()}>
          <Chip label="результат" value="truncated" />
        </Show>
      </div>
      <Show when={hits()}>{(text) => <OutputBlock text={text()} />}</Show>
    </div>
  );
}

function GenericPayloadCard(props: {
  toolKind?: ToolKind;
  payload: Record<string, unknown> | null | undefined;
}) {
  // Fallback for an unrecognized toolKind that still carries a payload: show the
  // kind plus a best-effort path/status so the body is never empty.
  const path = () => readString(props.payload, "path");
  const status = () => readString(props.payload, "status");
  return (
    <div class="tool-body">
      <div class="tool-body__head">
        <Terminal size={13} />
        <span class="tool-card__title">{props.toolKind ?? "tool"}</span>
        <StatusPill status={status()} />
      </div>
      <Show when={path()}>
        <div class="tool-card__chips">
          <Chip label="path" value={<code>{path()}</code>} />
        </div>
      </Show>
    </div>
  );
}

// --- Dispatcher -------------------------------------------------------------

export function ToolCard(props: {
  tool: ToolCardData;
  onOpenPath?: OpenPathFn;
  loadArtifactRange?: LoadArtifactRangeFn;
}) {
  const payload = () => props.tool.payload;

  // A reactive accessor (not an IIFE): re-evaluates if `toolKind` resolves late
  // during streaming, and keeps the fallback for unknown/incomplete payloads.
  const body = () => {
    switch (props.tool.toolKind) {
      case "run_command":
        return <RunCommandCard payload={payload()} />;
      case "read_file":
        return (
          <ReadFileCard
            payload={payload()}
            output={props.tool.output}
            toolCallId={props.tool.toolCallId}
            onOpen={props.onOpenPath}
            loadArtifactRange={props.loadArtifactRange}
          />
        );
      case "write_file":
        return (
          <WriteFileCard
            payload={payload()}
            artifacts={props.tool.artifacts}
            onOpen={props.onOpenPath}
          />
        );
      case "edit_file":
        return (
          <EditFileCard
            payload={payload()}
            artifacts={props.tool.artifacts}
            onOpen={props.onOpenPath}
          />
        );
      case "apply_patch":
        return (
          <ApplyPatchCard
            payload={payload()}
            touchedPaths={props.tool.touchedPaths}
          />
        );
      case "list_files":
        return <ListFilesCard payload={payload()} output={props.tool.output} />;
      case "search_text":
        return <SearchTextCard payload={payload()} output={props.tool.output} />;
      default:
        return (
          <GenericPayloadCard toolKind={props.tool.toolKind} payload={payload()} />
        );
    }
  };

  return <>{body()}</>;
}
