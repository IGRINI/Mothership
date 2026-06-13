import {
  For,
  JSX,
  Match,
  Show,
  Switch,
  createEffect,
  createSignal,
  onCleanup,
} from "solid-js";
import { convertFileSrc } from "@tauri-apps/api/core";
import {
  ChevronDown,
  FilePlus,
  FileSearch,
  FileText,
  GitCompare,
  Image,
  ListTree,
  Terminal,
} from "lucide-solid";

import {
  getChangeFileDiff,
  readImageDataUrlWithTimeout,
  type ChangeFileDiff,
  type ToolArtifact,
  type ToolCommand,
  type ToolExecutionResult,
  type ToolKind,
} from "../../shared/api/mothership";
import {
  commandPresentation,
  looksLikeUnifiedDiff,
  type CommandOutputView,
} from "../../shared/toolCommandPresentation";
import { FileActions, onFileContextMenu } from "../../shared/ui/FileActions";
import { CodeBlock } from "./components/CodeBlock";
import { CollapsibleDiff } from "./components/CollapsibleDiff";
import { DiffBlock } from "./components/DiffBlock";
import { FileTree } from "./components/FileTree";
import { OutputBlock } from "./components/OutputBlock";
import { parseNumberedOutput, splitPath, type CodeLineModel } from "./components/code";
import { highlightCommand } from "./components/highlight";
import { diffStat, newSideLines } from "./components/diff";

// Opens a workspace path in an external app. Goes through a capability-checked
// Core/Tauri command — never a raw shell open of UI-supplied data.
export type OpenPathFn = (path: string) => void;

// Given a touched path, the id of the change file recording THIS tool call's
// edit to it (so the card can show the live, foldable per-edit diff from the
// change journal instead of a frozen compact diff). Undefined if not recorded.
export type FindChangeFileId = (path: string) => string | undefined;

// Loads a change file's whole-file diff (its own before/after snapshot, so it
// stays a faithful record of that specific edit) and renders it GitHub-style:
// changes ± context, unchanged gaps collapsed into unfold rows.
function ChangeFileDiffView(props: { changeFileId: string }) {
  const [diff, setDiff] = createSignal<ChangeFileDiff | null>(null);
  const [state, setState] = createSignal<"loading" | "ready" | "error">(
    "loading",
  );
  createEffect(() => {
    const id = props.changeFileId;
    setState("loading");
    getChangeFileDiff(id, 0, 0, true)
      .then((value) => {
        setDiff(value);
        setState(value.unavailable ? "error" : "ready");
      })
      .catch(() => setState("error"));
  });
  return (
    <Switch fallback={<p class="tool-body__note">Загрузка diff…</p>}>
      <Match when={state() === "error"}>
        <p class="tool-body__note">Не удалось загрузить diff.</p>
      </Match>
      <Match when={state() === "ready" && diff()}>
        {(value) => <CollapsibleDiff diff={value().lines.join("\n")} />}
      </Match>
    </Switch>
  );
}

// Lazily fetches a byte range of a tool's *persisted* output artifact (the
// snapshot taken at tool-call time), NOT the live file — which may have changed.
// Keyed by tool_call_id + the artifact's durable logRef.
export type LoadArtifactRangeFn = (args: {
  toolCallId: string;
  logRef: string;
  offset: number;
  limit: number;
}) => Promise<{ content: string; nextOffset: number | null; eof: boolean }>;

export interface ToolImagePreviewItem {
  id: string;
  src: string;
  path: string;
  label: string;
  contentType: string;
  sizeBytes: number;
  preview: string;
}

export type PreviewImageFn = (
  image: ToolImagePreviewItem,
  images: ToolImagePreviewItem[],
) => void;

function fileNameFromPath(path: string) {
  const normalized = path.replace(/\\/g, "/");
  const name = normalized.split("/").filter(Boolean).pop();
  return name && name.trim().length > 0 ? name : path;
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

function artifactImageAssetSrc(path: string) {
  return isTauriRuntime() ? convertFileSrc(path) : path;
}

function imagePreviewItems(
  artifacts: ToolArtifact[] | undefined,
): ToolImagePreviewItem[] {
  return (artifacts ?? [])
    .filter(
      (artifact) =>
        artifact.kind === "image" &&
        artifact.contentType.startsWith("image/") &&
        typeof artifact.logRef === "string" &&
        artifact.logRef.trim().length > 0,
    )
    .map((artifact, index) => {
      const path = artifact.logRef!.trim();
      return {
        id: artifact.artifactId || `image-${index + 1}`,
        src: "",
        path,
        label: fileNameFromPath(path),
        contentType: artifact.contentType,
        sizeBytes: artifact.sizeBytes,
        preview: artifact.preview,
      };
    });
}

function toolCardErrorText(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function ToolImagePreviewButton(props: {
  image: ToolImagePreviewItem;
  images: ToolImagePreviewItem[];
  onPreviewImage?: PreviewImageFn;
}) {
  const [src, setSrc] = createSignal(props.image.src);
  const [loading, setLoading] = createSignal(false);
  const [loadError, setLoadError] = createSignal("");
  let loadRequestId = 0;
  let triedDataUrl = false;

  createEffect(() => {
    const path = props.image.path;
    const initialSrc = props.image.src;
    loadRequestId += 1;
    triedDataUrl = false;
    setSrc(initialSrc || (path ? artifactImageAssetSrc(path) : ""));
    setLoading(false);
    setLoadError("");

    if (!path) {
      setLoadError("Preview unavailable");
    }
  });

  onCleanup(() => {
    loadRequestId += 1;
  });

  const loadDataUrlFallback = () => {
    const path = props.image.path;
    if (!path || triedDataUrl) {
      setSrc("");
      setLoading(false);
      setLoadError("Preview unavailable");
      return;
    }

    triedDataUrl = true;
    const requestId = ++loadRequestId;
    setSrc("");
    setLoading(true);
    setLoadError("");
    readImageDataUrlWithTimeout(undefined, path)
      .then((dataUrl) => {
        if (requestId === loadRequestId) {
          setSrc(dataUrl);
          setLoadError("");
        }
      })
      .catch((error: unknown) => {
        if (requestId === loadRequestId) {
          setLoadError(toolCardErrorText(error));
        }
      })
      .finally(() => {
        if (requestId === loadRequestId) {
          setLoading(false);
        }
      });
  };

  const currentImage = (): ToolImagePreviewItem => ({
    ...props.image,
    src: src(),
  });

  const currentImages = () =>
    props.images.map((image) =>
      image.path === props.image.path ? currentImage() : image,
    );

  return (
    <button
      type="button"
      class="tool-card__image-preview"
      title={props.image.path}
      onClick={() => props.onPreviewImage?.(currentImage(), currentImages())}
      onContextMenu={(event) =>
        onFileContextMenu(event, {
          path: props.image.path,
          copyPath: props.image.path,
          artifact: true,
        })
      }
    >
      <Show
        when={!loadError() && src()}
        fallback={
          <span class="tool-card__image-preview-placeholder">
            {loadError()
              ? `Preview unavailable: ${loadError()}`
              : loading()
                ? "Loading preview..."
                : "Preview unavailable"}
          </span>
        }
      >
        {(value) => (
          <img
            src={value()}
            alt={props.image.label}
            loading="lazy"
            onError={loadDataUrlFallback}
          />
        )}
      </Show>
      <span class="tool-card__image-preview-caption">{props.image.label}</span>
    </button>
  );
}

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
  projectId?: string | null;
  command?: ToolCommand | null;
  result?: ToolExecutionResult | null;
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
    normalized === "complete" ||
    normalized === "completed"
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
                <DiffBlock diff={diff()} maxRows={10} />
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
            <DiffBlock diff={artifact().preview} maxRows={10} />
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

function commandCodeLines(output: string): CodeLineModel[] {
  const numbered = parseNumberedOutput(output);
  if (numbered.length > 0) {
    return numbered.map((line) => ({ ...line, read: true }));
  }
  return output.replace(/\r\n/g, "\n").split("\n").map((line, index) => ({
    no: index + 1,
    text: line,
    read: true,
  }));
}

function RunCommandIntentIcon(props: { view: CommandOutputView }) {
  return (
    <Switch fallback={<Terminal size={13} />}>
      <Match when={props.view === "file_tree"}>
        <ListTree size={13} />
      </Match>
      <Match when={props.view === "text"}>
        <FileText size={13} />
      </Match>
      <Match when={props.view === "search"}>
        <FileSearch size={13} />
      </Match>
      <Match when={props.view === "diff" || props.view === "status"}>
        <GitCompare size={13} />
      </Match>
    </Switch>
  );
}

function RunCommandOutput(props: {
  view: CommandOutputView;
  stdout: string;
  paths: string[];
  target?: string;
  projectId?: string;
}) {
  return (
    <Switch
      fallback={
        <Show when={props.stdout}>
          {(text) => <OutputBlock text={text()} />}
        </Show>
      }
    >
      <Match when={props.view === "file_tree" && props.paths.length > 0}>
        <FileTree paths={props.paths} projectId={props.projectId} />
      </Match>
      <Match when={props.view === "text" && props.stdout.trim().length > 0}>
        <CodeBlock
          lines={commandCodeLines(props.stdout)}
          title={props.target}
          path={props.target}
        />
      </Match>
      <Match
        when={
          props.view === "diff" &&
          props.stdout.trim().length > 0 &&
          looksLikeUnifiedDiff(props.stdout)
        }
      >
        <DiffBlock diff={props.stdout} maxRows={10} />
      </Match>
      <Match when={props.view === "search" && props.stdout.trim().length > 0}>
        <OutputBlock text={props.stdout} />
      </Match>
      <Match when={props.view === "status" && props.stdout.trim().length > 0}>
        <OutputBlock text={props.stdout} />
      </Match>
    </Switch>
  );
}

function RunCommandCard(props: { tool: ToolCardData }) {
  const presentation = () =>
    commandPresentation({
      command: props.tool.command,
      payload: props.tool.payload,
      result: props.tool.result,
      output: props.tool.output,
    });

  return (
    <div class="tool-body">
      <div class="tool-body__head">
        <RunCommandIntentIcon view={presentation().outputView} />
        <CommandLine text={presentation().commandLine || "command"} />
        <Show when={presentation().exitCode !== undefined}>
          <span
            class={`tool-card__pill tool-card__pill--${
              presentation().exitCode === 0 ? "ok" : "error"
            }`}
          >
            exit {presentation().exitCode}
          </span>
        </Show>
      </div>
      <RunCommandOutput
        view={presentation().outputView}
        stdout={presentation().stdout}
        paths={presentation().paths}
        target={presentation().target}
        projectId={props.tool.projectId ?? undefined}
      />
      <Show when={presentation().stderr}>
        {(text) => <OutputBlock text={text()} tone="err" />}
      </Show>
      <Show
        when={
          presentation().target ||
          presentation().truncated ||
          presentation().logRef
        }
      >
        <div class="tool-card__chips">
          <Show when={presentation().target}>
            {(value) => <Chip label="target" value={<code>{value()}</code>} />}
          </Show>
          <Show when={presentation().truncated}>
            <Chip label="output" value="truncated" />
          </Show>
          <Show when={presentation().logRef}>
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
  projectId?: string;
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

  // Mark the originally-read window so its lines carry the read highlight even
  // before the surrounding file is lazily loaded as context.
  const windowLines = () =>
    parseNumberedOutput(props.output ?? "").map((line) => ({
      ...line,
      read: true,
    }));
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
      <div
        class="tool-body__head"
        onContextMenu={(event) => {
          const target = path();
          if (target) {
            onFileContextMenu(event, {
              projectId: props.projectId,
              path: target,
              onOpen: props.onOpen,
            });
          }
        }}
      >
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
          loadMoreLabel="Загрузить ещё"
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
  projectId?: string;
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
      <div
        class="tool-body__head"
        onContextMenu={(event) => {
          const target = path();
          if (target) {
            onFileContextMenu(event, {
              projectId: props.projectId,
              path: target,
              onOpen: props.onOpen,
            });
          }
        }}
      >
        <FilePlus size={13} />
        <PathLabel path={path()} />
        <StatusPill status={status()} />
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
  projectId?: string;
  onOpen?: OpenPathFn;
  findChangeFileId?: FindChangeFileId;
}) {
  const path = () => readString(props.payload, "path");
  const status = () => readString(props.payload, "status");
  const occurrences = () => readNumber(props.payload, "occurrences");
  const diff = () => findDiffArtifact(props.artifacts);
  // Prefer the live, foldable diff from the change journal (whole-file context,
  // real positions, GitHub-style unfold). Fall back to the frozen compact diff
  // artifact when the change file isn't known (e.g. not yet recorded).
  const changeFileId = () => {
    const target = path();
    return target ? props.findChangeFileId?.(target) : undefined;
  };

  return (
    <div class="tool-body">
      <div
        class="tool-body__head"
        onContextMenu={(event) => {
          const target = path();
          if (target) {
            onFileContextMenu(event, {
              projectId: props.projectId,
              path: target,
              onOpen: props.onOpen,
            });
          }
        }}
      >
        <FileText size={13} />
        <PathLabel path={path()} />
        <StatusPill status={status()} />
        <Show when={occurrences() !== undefined}>
          <span class="tool-body__hint">{occurrences()}×</span>
        </Show>
      </div>
      <Show
        when={changeFileId()}
        fallback={
          <Show when={diff()}>
            {(artifact) => <DiffBlock diff={artifact().preview} maxRows={10} />}
          </Show>
        }
      >
        {(id) => <ChangeFileDiffView changeFileId={id()} />}
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

// Split the combined patch diff (`# op path` header per file, then that file's
// unified diff) into per-path diff text. A real diff line never starts with a
// bare "# " (context/+/− lines are prefixed), so the headers are unambiguous.
function parsePatchDiffs(combined: string): Record<string, string> {
  const out: Record<string, string> = {};
  let path: string | null = null;
  let buffer: string[] = [];
  const flush = () => {
    if (path !== null) out[path] = buffer.join("\n");
  };
  for (const line of combined.split("\n")) {
    const match = /^# \S+ (.+)$/.exec(line);
    if (match) {
      flush();
      path = match[1];
      buffer = [];
    } else if (path !== null) {
      buffer.push(line);
    }
  }
  flush();
  return out;
}

function PatchFileRow(props: {
  file: PatchFileMeta;
  projectId?: string;
  diff?: string;
  changeFileId?: string;
}) {
  const [open, setOpen] = createSignal(false);
  const parts = () => splitPath(props.file.path);
  const badge = () => patchOpBadge(props.file.op);
  const hasDiff = () =>
    Boolean(props.changeFileId) ||
    Boolean(props.diff && props.diff.trim().length > 0);
  return (
    <div
      class="tool-sub"
      classList={{ "tool-sub--static": !hasDiff() }}
      onContextMenu={(event) =>
        onFileContextMenu(event, {
          projectId: props.projectId,
          path: props.file.path,
        })
      }
    >
      <div
        class="tool-sub__summary"
        classList={{
          "tool-sub__summary--static": !hasDiff(),
          "tool-sub__summary--clickable": hasDiff(),
        }}
        onClick={() => {
          if (hasDiff()) setOpen(!open());
        }}
      >
        <Show when={hasDiff()}>
          <ChevronDown
            classList={{
              "tool-sub__chevron": true,
              "tool-sub__chevron--open": open(),
            }}
            size={13}
          />
        </Show>
        <span class={`tool-sub__op tool-sub__op--${badge()}`}>{badge()}</span>
        <code class="tool-sub__path">
          <Show when={parts().dir}>
            <span class="tool-sub__dir">{parts().dir}</span>
          </Show>
          <span class="tool-sub__name">{parts().name || "(файл)"}</span>
        </code>
        <span class="tool-sub__spacer" />
        <FileActions
          class="tool-sub__actions"
          projectId={props.projectId}
          path={props.file.path}
        />
        <span class="tool-sub__stat">
          <Show when={props.file.added > 0}>
            <span class="tool-stat__add">+{props.file.added}</span>
          </Show>{" "}
          <Show when={props.file.removed > 0}>
            <span class="tool-stat__del">−{props.file.removed}</span>
          </Show>
        </span>
      </div>
      <Show when={open() && hasDiff()}>
        <div class="tool-sub__diff">
          <Show
            when={props.changeFileId}
            fallback={<DiffBlock diff={props.diff} maxRows={10} />}
          >
            {(id) => <ChangeFileDiffView changeFileId={id()} />}
          </Show>
        </div>
      </Show>
    </div>
  );
}

function ApplyPatchCard(props: {
  payload: Record<string, unknown> | null | undefined;
  touchedPaths?: string[];
  projectId?: string;
  artifacts?: ToolArtifact[];
  findChangeFileId?: FindChangeFileId;
}) {
  const status = () => readString(props.payload, "status");
  const files = () => readPatchFiles(props.payload?.files);
  // Per-file unified diffs, parsed from the combined diff artifact (real line
  // numbers + context) so each row can expand to its actual changes.
  const diffByPath = () => {
    const artifact = findDiffArtifact(props.artifacts);
    return artifact ? parsePatchDiffs(artifact.preview) : {};
  };
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
                  <li
                    class="tool-card__file"
                    onContextMenu={(event) =>
                      onFileContextMenu(event, {
                        projectId: props.projectId,
                        path: file,
                      })
                    }
                  >
                    <code>{file}</code>
                    <FileActions
                      class="tool-card__file-actions"
                      projectId={props.projectId}
                      path={file}
                    />
                  </li>
                )}
              </For>
            </ul>
          </Show>
        }
      >
        <div class="tool-subs">
          <For each={files()}>
            {(file) => (
              <PatchFileRow
                file={file}
                projectId={props.projectId}
                diff={diffByPath()[file.path]}
                changeFileId={props.findChangeFileId?.(file.path)}
              />
            )}
          </For>
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
          <Chip label="результат" value="обрезан" />
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

function ProviderServiceCard(props: {
  tool: ToolCardData;
  payload: Record<string, unknown> | null | undefined;
  onPreviewImage?: PreviewImageFn;
}) {
  const [promptExpanded, setPromptExpanded] = createSignal(false);
  const providerId = () => readString(props.payload, "providerId");
  const modelId = () => readString(props.payload, "modelId");
  const prompt = () => readString(props.payload, "prompt") ?? promptPreview();
  const promptPreview = () => readString(props.payload, "promptPreview");
  const imageCount = () => readNumber(props.payload, "imageCount");
  const status = () => props.tool.result?.status;
  const message = () => props.tool.result?.message ?? props.tool.message ?? "";
  const images = () => imagePreviewItems(props.tool.artifacts);
  const firstImage = () => images()[0];

  return (
    <div
      class="tool-body"
      onContextMenu={(event) => {
        const image = firstImage();
        if (!image) {
          return;
        }
        onFileContextMenu(event, {
          path: image.path,
          copyPath: image.path,
          artifact: true,
        });
      }}
    >
      <div class="tool-body__head">
        <Image size={13} />
        <span class="tool-card__title">Image generation</span>
        <StatusPill status={status()} />
      </div>
      <div class="tool-card__chips">
        <Show when={providerId()}>
          {(value) => <Chip label="provider" value={<code>{value()}</code>} />}
        </Show>
        <Show when={modelId()}>
          {(value) => <Chip label="model" value={<code>{value()}</code>} />}
        </Show>
        <Show when={imageCount() !== undefined}>
          <Chip label="images" value={String(imageCount())} />
        </Show>
      </div>
      <Show when={prompt()}>
        {(value) => (
          <div class="tool-card__prompt-block">
            <button
              type="button"
              class="tool-card__prompt-toggle"
              onClick={() => setPromptExpanded((expanded) => !expanded)}
            >
              <ChevronDown
                size={13}
                classList={{ "is-open": promptExpanded() }}
              />
              <span>Prompt</span>
            </button>
            <Show
              when={promptExpanded()}
              fallback={
                <p class="tool-body__note">
                  Prompt: <code>{promptPreview() ?? value()}</code>
                </p>
              }
            >
              <pre class="tool-card__prompt-full">{value()}</pre>
            </Show>
          </div>
        )}
      </Show>
      <Show when={images().length > 0}>
        <div class="tool-card__image-grid">
          <For each={images()}>
            {(image) => (
              <ToolImagePreviewButton
                image={image}
                images={images()}
                onPreviewImage={props.onPreviewImage}
              />
            )}
          </For>
        </div>
      </Show>
      <Show when={status() !== "failed" && message().trim()}>
        <OutputBlock text={message()} />
      </Show>
    </div>
  );
}

// --- Dispatcher -------------------------------------------------------------

export function ToolCard(props: {
  tool: ToolCardData;
  onOpenPath?: OpenPathFn;
  loadArtifactRange?: LoadArtifactRangeFn;
  findChangeFileId?: FindChangeFileId;
  onPreviewImage?: PreviewImageFn;
}) {
  const payload = () => props.tool.payload;

  // A reactive accessor (not an IIFE): re-evaluates if `toolKind` resolves late
  // during streaming, and keeps the fallback for unknown/incomplete payloads.
  const body = () => {
    switch (props.tool.toolKind) {
      case "run_command":
        return <RunCommandCard tool={props.tool} />;
      case "read_file":
        return (
          <ReadFileCard
            payload={payload()}
            output={
              props.tool.output || props.tool.result?.stdoutPreview || ""
            }
            toolCallId={props.tool.toolCallId}
            projectId={props.tool.projectId ?? undefined}
            onOpen={props.onOpenPath}
            loadArtifactRange={props.loadArtifactRange}
          />
        );
      case "write_file":
        return (
          <WriteFileCard
            payload={payload()}
            artifacts={props.tool.artifacts}
            projectId={props.tool.projectId ?? undefined}
            onOpen={props.onOpenPath}
          />
        );
      case "edit_file":
        return (
          <EditFileCard
            payload={payload()}
            artifacts={props.tool.artifacts}
            projectId={props.tool.projectId ?? undefined}
            onOpen={props.onOpenPath}
            findChangeFileId={props.findChangeFileId}
          />
        );
      case "apply_patch":
        return (
          <ApplyPatchCard
            payload={payload()}
            touchedPaths={props.tool.touchedPaths}
            projectId={props.tool.projectId ?? undefined}
            artifacts={props.tool.artifacts}
            findChangeFileId={props.findChangeFileId}
          />
        );
      case "search_text":
        return <SearchTextCard payload={payload()} output={props.tool.output} />;
      case "image_generate":
        return (
          <ProviderServiceCard
            tool={props.tool}
            payload={payload()}
            onPreviewImage={props.onPreviewImage}
          />
        );
      default:
        return (
          <GenericPayloadCard toolKind={props.tool.toolKind} payload={payload()} />
        );
    }
  };

  return <>{body()}</>;
}
