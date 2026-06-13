// One tool call inside a message: collapsible row with a typed icon, headline,
// diff-stat / status badge, hover file actions, and an expandable body that
// renders the typed semantic card (or the generic command/output fallback),
// plus approve/deny/cancel controls while the call is pending.

import { createSignal, Show } from "solid-js";
import { Check, ChevronDown, Square, X } from "lucide-solid";

import type { ChangeSetSummary } from "../../../shared/api/mothership";
import {
  getToolArtifactRange,
  openToolPath,
} from "../../../shared/api/mothership";
import { commandPresentation } from "../../../shared/toolCommandPresentation";
import { FileActions, onFileContextMenu } from "../../../shared/ui/FileActions";
import {
  ApprovalPreview,
  ToolCard,
  toolDiffStat,
  type ToolImagePreviewItem,
} from "../ToolCards";
import {
  countTextLines,
  formatToolCommand,
  formatToolHeadline,
  formatToolOutput,
  isTerminalToolKind,
  shouldShowInlineToolOutput,
  toolStatusLabel,
  toolTone,
} from "../tool-format";
import type { ToolExecutionView } from "../types";
import { ToolKindIcon } from "./ToolKindIcon";

const TOOL_OUTPUT_MAX_VISIBLE_LINES = 10;

export function InlineToolCall(props: {
  expanded: boolean;
  tool: ToolExecutionView;
  changeSets?: ChangeSetSummary[];
  onApprove: () => void;
  onCancel: () => void;
  onDeny: () => void;
  onPreviewImage: (
    image: ToolImagePreviewItem,
    images: ToolImagePreviewItem[],
  ) => void;
  onToggle: () => void;
}) {
  const command = () => formatToolCommand(props.tool);
  const commandIntent = () => commandPresentation(props.tool).intent;
  // The change file (in the journal) recording THIS tool call's edit to a path,
  // so the card shows the live, foldable per-edit diff (its own before/after
  // snapshot) instead of a frozen compact one. Undefined → fall back to the
  // stored artifact diff.
  const findChangeFileId = (path: string): string | undefined => {
    for (const set of props.changeSets ?? []) {
      if (set.toolCallId && set.toolCallId === props.tool.toolCallId) {
        const file = set.files?.find((entry) => entry.path === path);
        if (file) {
          return file.id;
        }
      }
    }
    return undefined;
  };
  const output = () => formatToolOutput(props.tool);
  const outputLineCount = () => countTextLines(output());
  const isOutputScrollable = () =>
    outputLineCount() > TOOL_OUTPUT_MAX_VISIBLE_LINES;
  const failureMessage = () =>
    props.tool.kind === "failed"
      ? (
          props.tool.result?.message ??
          props.tool.message ??
          props.tool.result?.stderrPreview ??
          props.tool.result?.stdoutPreview ??
          ""
        ).trim()
      : "";
  const canApprove = () => props.tool.kind === "permission_requested";
  const canCancel = () =>
    !canApprove() && !isTerminalToolKind(props.tool.kind);
  // Prefer the typed semantic card whenever the backend supplied both a tool
  // kind and a payload; otherwise fall back to the generic text rendering.
  const hasSemanticCard = () =>
    Boolean(props.tool.toolKind && props.tool.payload);
  // Header diff-stat for a completed edit/patch (+N −M and an op badge).
  const headerStat = () =>
    props.tool.kind === "completed" ? toolDiffStat(props.tool) : undefined;

  // Open a workspace path via the capability-checked Core command. Surface a
  // rejection (containment refused / opener failed) instead of swallowing it.
  const [openError, setOpenError] = createSignal<string | undefined>();
  const handleOpenPath = (path: string) => {
    setOpenError(undefined);
    openToolPath(props.tool.projectId, path).catch((error: unknown) => {
      const message =
        typeof error === "string"
          ? error
          : error instanceof Error
            ? error.message
            : "не удалось открыть файл";
      setOpenError(message);
    });
  };
  // The file this tool touched (read/write/edit payload path, else the first
  // patched path) — drives the row's hover actions + right-click menu.
  const primaryPath = (): string | undefined => {
    const payload = props.tool.payload;
    const fromPayload =
      payload && typeof payload.path === "string" ? payload.path : undefined;
    return fromPayload ?? props.tool.touchedPaths?.[0];
  };
  const primaryArtifactPath = (): string | undefined =>
    props.tool.artifacts?.find(
      (artifact) =>
        artifact.kind === "image" &&
        artifact.contentType.startsWith("image/") &&
        typeof artifact.logRef === "string" &&
        artifact.logRef.trim().length > 0,
    )?.logRef?.trim();
  const primaryContextPath = () => primaryPath() ?? primaryArtifactPath();
  // Lazily fetch a range of the call's persisted output artifact (the snapshot,
  // not the live file).
  const loadArtifactRange = async (args: {
    toolCallId: string;
    logRef: string;
    offset: number;
    limit: number;
  }) => {
    const range = await getToolArtifactRange(
      args.toolCallId,
      args.logRef,
      args.offset,
      args.limit,
    );
    return {
      content: range.content,
      nextOffset: range.nextOffset ?? null,
      eof: range.eof,
    };
  };

  return (
    <div class="inline-tool-call">
      <div
        class="inline-tool-call__row"
        onContextMenu={(event) => {
          const path = primaryContextPath();
          if (path) {
            onFileContextMenu(event, {
              projectId: props.tool.projectId ?? undefined,
              path,
              copyPath: path,
              artifact: path === primaryArtifactPath(),
              onOpen: handleOpenPath,
            });
          }
        }}
      >
        <button
          class="inline-tool-call__summary"
          type="button"
          aria-expanded={props.expanded}
          onClick={props.onToggle}
        >
          <span class="inline-tool-call__icon">
            <ToolKindIcon
              kind={props.tool.toolKind}
              commandIntent={commandIntent()}
            />
          </span>
          <span
            class="inline-tool-call__title"
            title={formatToolHeadline(props.tool)}
          >
            {formatToolHeadline(props.tool)}
          </span>
          <Show when={failureMessage()}>
            {(message) => (
              <span class="inline-tool-call__error-preview" title={message()}>
                {message()}
              </span>
            )}
          </Show>
        </button>
        {/* Hover file actions sit to the LEFT of the diff-stat — identical to the
            change-set rows. */}
        <Show when={primaryPath()}>
          {(path) => (
            <FileActions
              class="inline-tool-call__file-actions"
              projectId={props.tool.projectId ?? undefined}
              path={path()}
              onOpen={handleOpenPath}
            />
          )}
        </Show>
        {/* A completed edit/patch shows a diff-stat (+N −M); otherwise surface a
            status that needs attention (running, denied, failed). A plain
            "Completed" is implied by the row, so it shows nothing. */}
        <Show
          when={headerStat()}
          fallback={
            <Show when={props.tool.kind !== "completed"}>
              <span
                class={`tool-status tool-status--${toolTone(props.tool.kind)}`}
              >
                {toolStatusLabel(props.tool.kind)}
              </span>
            </Show>
          }
        >
          {(stat) => (
            <span class="tool-stat">
              <Show when={stat().add}>
                <span class="tool-stat__add">+{stat().add}</span>
              </Show>
              <Show when={stat().del}>
                <span class="tool-stat__del">−{stat().del}</span>
              </Show>
              <Show when={stat().op}>
                <span class={`tool-stat__op tool-stat__op--${stat().op}`}>
                  {stat().op}
                </span>
              </Show>
            </span>
          )}
        </Show>
        <button
          class="inline-tool-call__toggle"
          type="button"
          aria-label={props.expanded ? "Свернуть" : "Развернуть"}
          onClick={props.onToggle}
        >
          <ChevronDown
            classList={{
              "inline-tool-call__chevron": true,
              "inline-tool-call__chevron--open": props.expanded,
            }}
            size={14}
          />
        </button>
      </div>

      <Show when={props.expanded}>
        <div class="inline-tool-call__body">
          <Show
            when={hasSemanticCard()}
            fallback={
              <>
                <Show when={command()}>
                  <pre class="inline-tool-call__command">{command()}</pre>
                </Show>

                <Show when={!canApprove() && props.tool.message}>
                  <p class="inline-tool-call__message">{props.tool.message}</p>
                </Show>

                <Show when={shouldShowInlineToolOutput(props.tool, output())}>
                  <pre
                    classList={{
                      "inline-tool-call__output": true,
                      "inline-tool-call__output--scrollable":
                        isOutputScrollable(),
                    }}
                    style={`--tool-output-lines: ${TOOL_OUTPUT_MAX_VISIBLE_LINES}`}
                  >
                    {output()}
                  </pre>
                </Show>
              </>
            }
          >
            <ToolCard
              tool={props.tool}
              onOpenPath={handleOpenPath}
              loadArtifactRange={loadArtifactRange}
              findChangeFileId={findChangeFileId}
              onPreviewImage={props.onPreviewImage}
            />
          </Show>

          <Show when={hasSemanticCard() && failureMessage()}>
            {(message) => <p class="tool-body__error">{message()}</p>}
          </Show>

          <Show when={openError()}>
            {(message) => <p class="tool-body__error">{message()}</p>}
          </Show>

          {/* Show the pending change prominently before the human approves it,
              in both the semantic and fallback paths. */}
          <Show when={canApprove() && props.tool.message}>
            {(message) => (
              <ApprovalPreview
                message={message()}
                artifacts={props.tool.artifacts}
              />
            )}
          </Show>

          <Show when={canApprove() || canCancel()}>
            <div class="inline-tool-call__actions">
              <Show when={canApprove()}>
                <button type="button" onClick={props.onApprove}>
                  <Check size={14} />
                  Approve
                </button>
                <button type="button" onClick={props.onDeny}>
                  <X size={14} />
                  Deny
                </button>
              </Show>
              <Show when={canCancel()}>
                <button type="button" onClick={props.onCancel}>
                  <Square size={13} />
                  Cancel
                </button>
              </Show>
            </div>
          </Show>
        </div>
      </Show>
    </div>
  );
}
