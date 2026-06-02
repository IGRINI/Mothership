import { For, JSX, Show } from "solid-js";
import {
  FileDiff,
  FilePenLine,
  FilePlus,
  FileSearch,
  FileText,
  GitCompare,
  ListTree,
  Replace,
  Terminal,
} from "lucide-solid";

import type { ToolArtifact, ToolKind } from "../../shared/api/mothership";

// Minimal structural view of a tool execution. Kept loose on purpose so this
// module does not depend on Dashboard's internal `ToolExecutionView` shape — it
// only reads the typed-tool fields it knows how to render.
export interface ToolCardData {
  kind: string;
  toolKind?: ToolKind;
  payload?: Record<string, unknown> | null;
  touchedPaths?: string[];
  artifacts?: ToolArtifact[];
  message?: string | null;
}

// --- Defensive payload accessors -------------------------------------------
// `payload` is `Record<string, unknown>`; never assume a field exists or has a
// given type. These readers return undefined when the field is absent or the
// wrong type, so cards can simply omit anything they cannot show.

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
  return typeof value === "number" && Number.isFinite(value)
    ? value
    : undefined;
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
      <span
        class={`tool-card__pill tool-card__pill--${statusTone(props.status)}`}
      >
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
    normalized === "denied"
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
  return (
    <Show when={props.path}>
      <code class="tool-card__path" title={props.path}>
        {props.path}
      </code>
    </Show>
  );
}

// --- Diff rendering ---------------------------------------------------------

interface DiffLine {
  text: string;
  tone: "add" | "remove" | "hunk" | "meta" | "context";
}

function classifyDiffLine(line: string): DiffLine["tone"] {
  if (line.startsWith("@@")) {
    return "hunk";
  }
  if (line.startsWith("+++") || line.startsWith("---")) {
    return "meta";
  }
  if (line.startsWith("diff ") || line.startsWith("index ")) {
    return "meta";
  }
  if (line.startsWith("+")) {
    return "add";
  }
  if (line.startsWith("-")) {
    return "remove";
  }
  return "context";
}

function toDiffLines(diff: string): DiffLine[] {
  return diff.replace(/\r\n/g, "\n").split("\n").map((text) => ({
    text,
    tone: classifyDiffLine(text),
  }));
}

export function DiffCard(props: {
  diff: string;
  truncated?: boolean;
  logRef?: string | null;
  sizeBytes?: number;
}) {
  const lines = () => toDiffLines(props.diff);
  return (
    <div class="tool-diff">
      <pre class="tool-diff__body">
        <For each={lines()}>
          {(line) => (
            <span class={`tool-diff__line tool-diff__line--${line.tone}`}>
              {line.text.length > 0 ? line.text : " "}
            </span>
          )}
        </For>
      </pre>
      <Show when={props.truncated}>
        <div class="tool-diff__note">
          diff truncated
          <Show when={formatBytes(props.sizeBytes)}>
            {(size) => <span> — full size {size()}</span>}
          </Show>
          <Show when={props.logRef}>
            {(ref) => (
              <span>
                {" "}
                · full diff at <code>{ref()}</code>
              </span>
            )}
          </Show>
        </div>
      </Show>
    </div>
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

// --- Before-approval preview ------------------------------------------------
// When a tool is `permission_requested`, the backend packs a human-readable
// preview into `message`: a summary, then (optionally) a blank line, then a
// bounded unified diff. Split on the first blank line so the diff portion can
// be syntax-colored; if there is no diff portion, render the whole thing as a
// plain preview block.

function splitApprovalPreview(message: string): {
  summary: string;
  diff?: string;
} {
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
  const messageTruncated = () => /\[preview truncated/i.test(props.message);
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
                <DiffCard diff={diff()} truncated={messageTruncated()} />
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
            <DiffCard
              diff={artifact().preview}
              truncated={artifact().truncated}
              sizeBytes={artifact().sizeBytes}
              logRef={artifact().logRef}
            />
          </>
        )}
      </Show>
    </div>
  );
}

// --- Per-kind semantic cards ------------------------------------------------

function RunCommandCard(props: { payload: Record<string, unknown> | null | undefined }) {
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
    <div class="tool-card">
      <div class="tool-card__head">
        <Terminal size={14} />
        <code class="tool-card__cmd">{commandLine()}</code>
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
        {(text) => <pre class="tool-card__stream">{text()}</pre>}
      </Show>
      <Show when={stderr()}>
        {(text) => (
          <pre class="tool-card__stream tool-card__stream--err">
            [stderr]
            {"\n"}
            {text()}
          </pre>
        )}
      </Show>
      <div class="tool-card__chips">
        <Show when={truncated()}>
          <Chip label="output" value="truncated" />
        </Show>
        <Show when={logRef()}>
          {(ref) => <Chip label="log" value={<code>{ref()}</code>} />}
        </Show>
      </div>
    </div>
  );
}

function ReadFileCard(props: { payload: Record<string, unknown> | null | undefined }) {
  const path = () => readString(props.payload, "path");
  const status = () => readString(props.payload, "status");
  const startLine = () =>
    readNumber(props.payload, "startLine") ?? readNumber(props.payload, "line");
  const endLine = () => readNumber(props.payload, "endLine");
  const window = () => readString(props.payload, "window");
  const lossy = () => readBoolean(props.payload, "lossy");
  const partial = () => readBoolean(props.payload, "partial");
  const lineRange = () => {
    const start = startLine();
    if (start === undefined) {
      return undefined;
    }
    const end = endLine();
    return end !== undefined && end !== start ? `${start}–${end}` : `${start}`;
  };

  return (
    <div class="tool-card">
      <div class="tool-card__head">
        <FileText size={14} />
        <PathLabel path={path()} />
        <StatusPill status={status()} />
      </div>
      <div class="tool-card__chips">
        <Show when={lineRange()}>
          {(range) => <Chip label="lines" value={range()} />}
        </Show>
        <Show when={window()}>
          {(value) => <Chip label="window" value={value()} />}
        </Show>
        <Show when={lossy()}>
          <Chip label="encoding" value="lossy" />
        </Show>
        <Show when={partial()}>
          <Chip label="read" value="partial" />
        </Show>
      </div>
    </div>
  );
}

function WriteFileCard(props: { payload: Record<string, unknown> | null | undefined }) {
  const path = () => readString(props.payload, "path");
  const status = () => readString(props.payload, "status");
  const bytes = () => readNumber(props.payload, "bytes");
  const sha = () => shortSha(readString(props.payload, "sha256"));

  return (
    <div class="tool-card">
      <div class="tool-card__head">
        <FilePlus size={14} />
        <PathLabel path={path()} />
        <StatusPill status={status()} />
      </div>
      <div class="tool-card__chips">
        <Show when={formatBytes(bytes())}>
          {(value) => <Chip label="size" value={value()} />}
        </Show>
        <Show when={sha()}>
          {(value) => <Chip label="sha" value={<code>{value()}</code>} />}
        </Show>
      </div>
    </div>
  );
}

function EditFileCard(props: { payload: Record<string, unknown> | null | undefined }) {
  const path = () => readString(props.payload, "path");
  const status = () => readString(props.payload, "status");
  const occurrences = () => readNumber(props.payload, "occurrences");
  const strategy = () => readString(props.payload, "strategy");
  const sha = () => shortSha(readString(props.payload, "sha256"));

  return (
    <div class="tool-card">
      <div class="tool-card__head">
        <FilePenLine size={14} />
        <PathLabel path={path()} />
        <StatusPill status={status()} />
      </div>
      <div class="tool-card__chips">
        <Show when={occurrences() !== undefined}>
          <Chip label="edits" value={String(occurrences())} />
        </Show>
        <Show when={strategy()}>
          {(value) => <Chip label="strategy" value={value()} />}
        </Show>
        <Show when={sha()}>
          {(value) => <Chip label="sha" value={<code>{value()}</code>} />}
        </Show>
      </div>
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

function ApplyPatchCard(props: {
  payload: Record<string, unknown> | null | undefined;
  touchedPaths?: string[];
}) {
  const status = () => readString(props.payload, "status");
  const files = () => {
    const fromPayload = normalizePatchFiles(props.payload?.files);
    if (fromPayload.length > 0) {
      return fromPayload;
    }
    return props.touchedPaths ?? [];
  };

  return (
    <div class="tool-card">
      <div class="tool-card__head">
        <GitCompare size={14} />
        <span class="tool-card__title">Apply patch</span>
        <StatusPill status={status()} />
      </div>
      <div class="tool-card__chips">
        <Chip label="files" value={String(files().length)} />
      </div>
      <Show when={files().length > 0}>
        <ul class="tool-card__files">
          <For each={files()}>
            {(file) => (
              <li>
                <code>{file}</code>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </div>
  );
}

function ListFilesCard(props: { payload: Record<string, unknown> | null | undefined }) {
  const count = () => readNumber(props.payload, "count");
  const truncated = () => readBoolean(props.payload, "truncated");
  const dir = () => readString(props.payload, "dir");
  const glob = () => readString(props.payload, "glob");
  const includeIgnored = () => readBoolean(props.payload, "includeIgnored");

  return (
    <div class="tool-card">
      <div class="tool-card__head">
        <ListTree size={14} />
        <span class="tool-card__title">List files</span>
        <Show when={dir()}>
          {(value) => <PathLabel path={value()} />}
        </Show>
      </div>
      <div class="tool-card__chips">
        <Show when={count() !== undefined}>
          <Chip label="count" value={String(count())} />
        </Show>
        <Show when={glob()}>
          {(value) => <Chip label="glob" value={<code>{value()}</code>} />}
        </Show>
        <Show when={includeIgnored()}>
          <Chip label="ignored" value="included" />
        </Show>
        <Show when={truncated()}>
          <Chip label="result" value="truncated" />
        </Show>
      </div>
    </div>
  );
}

function SearchTextCard(props: { payload: Record<string, unknown> | null | undefined }) {
  const pattern = () => readString(props.payload, "pattern");
  const count = () => readNumber(props.payload, "count");
  const filesWithMatches = () => readNumber(props.payload, "filesWithMatches");
  const truncated = () => readBoolean(props.payload, "truncated");
  const partial = () => readBoolean(props.payload, "partial");
  const cappedFileCount = () => readNumber(props.payload, "cappedFileCount");

  return (
    <div class="tool-card">
      <div class="tool-card__head">
        <FileSearch size={14} />
        <span class="tool-card__title">Search</span>
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
          <Chip label="matches" value={String(count())} />
        </Show>
        <Show when={filesWithMatches() !== undefined}>
          <Chip label="files" value={String(filesWithMatches())} />
        </Show>
        <Show when={cappedFileCount() !== undefined}>
          <Chip label="capped at" value={String(cappedFileCount())} />
        </Show>
        <Show when={partial()}>
          <Chip label="scan" value="partial" />
        </Show>
        <Show when={truncated()}>
          <Chip label="result" value="truncated" />
        </Show>
      </div>
    </div>
  );
}

function GenericPayloadCard(props: {
  toolKind?: ToolKind;
  payload: Record<string, unknown> | null | undefined;
}) {
  // Fallback for an unrecognized toolKind that still carries a payload: show
  // the kind plus a best-effort path/status so the card is never empty.
  const path = () => readString(props.payload, "path");
  const status = () => readString(props.payload, "status");
  return (
    <div class="tool-card">
      <div class="tool-card__head">
        <Replace size={14} />
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

export function ToolCard(props: { tool: ToolCardData }) {
  const payload = () => props.tool.payload;
  const diffArtifact = () => findDiffArtifact(props.tool.artifacts);

  const body = () => {
    switch (props.tool.toolKind) {
      case "run_command":
        return <RunCommandCard payload={payload()} />;
      case "read_file":
        return <ReadFileCard payload={payload()} />;
      case "write_file":
        return <WriteFileCard payload={payload()} />;
      case "edit_file":
        return <EditFileCard payload={payload()} />;
      case "apply_patch":
        return (
          <ApplyPatchCard
            payload={payload()}
            touchedPaths={props.tool.touchedPaths}
          />
        );
      case "list_files":
        return <ListFilesCard payload={payload()} />;
      case "search_text":
        return <SearchTextCard payload={payload()} />;
      default:
        return (
          <GenericPayloadCard
            toolKind={props.tool.toolKind}
            payload={payload()}
          />
        );
    }
  };

  return (
    <div class="tool-card-group">
      {body()}
      <Show when={diffArtifact()}>
        {(artifact) => (
          <div class="tool-card__diff-wrap">
            <div class="tool-card__diff-head">
              <FileDiff size={13} />
              <span>Diff</span>
            </div>
            <DiffCard
              diff={artifact().preview}
              truncated={artifact().truncated}
              logRef={artifact().logRef}
              sizeBytes={artifact().sizeBytes}
            />
          </div>
        )}
      </Show>
    </div>
  );
}
