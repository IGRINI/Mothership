// Renders an assistant message's ordered parts (text + tool calls), optionally
// collapsing everything but the final answer behind a "Worked for…" spoiler.
// Includes the typewriter reveal for streamed text and the per-part identity
// plumbing that keeps rows mounted across streaming updates.

import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  Show,
  untrack,
} from "solid-js";
import { ChevronDown, Terminal } from "lucide-solid";

import type { ChangeSetSummary } from "../../../shared/api/mothership";
import { chatSettings } from "../../../shared/chatSettings";
import { WorkSpoiler } from "../../../shared/ui/WorkSpoiler";
import type { RenderableMessagePart, ToolExecutionView } from "../types";
import type { ToolImagePreviewItem } from "../ToolCards";
import { InlineToolCall } from "./InlineToolCall";
import { MessageMarkdown } from "./MessageMarkdown";

export interface MessagePartsProps {
  expandedInlineTools: Record<string, boolean>;
  parts: RenderableMessagePart[];
  projectId?: string;
  /** True while the run is still streaming (drives the "Working…" spoiler). */
  working: boolean;
  changeSets?: ChangeSetSummary[];
  onApproveTool: (toolCallId: string) => void;
  onCancelTool: (toolCallId: string) => void;
  onDenyTool: (toolCallId: string) => void;
  onError: (message: string) => void;
  onPreviewToolImage: (
    image: ToolImagePreviewItem,
    images: ToolImagePreviewItem[],
  ) => void;
  onToggleTool: (toolCallId: string) => void;
}

/** Index of the final answer: the last non-empty text part. Everything else is
 * "work" that the collapse-work pref hides behind one spoiler. */
function finalTextIndex(parts: RenderableMessagePart[]): number {
  // The final answer is a TRAILING text part — one with no tool call after it.
  // Scanning from the end, a tool before any non-empty text means the run is
  // still mid-work (no final yet), so nothing is pulled out of the spoiler; an
  // intermediate "let me check…" preamble that precedes tools is NOT mistaken
  // for the final answer while the run streams.
  for (let index = parts.length - 1; index >= 0; index -= 1) {
    const part = parts[index];
    if (part.kind === "text" && part.text?.trim()) {
      return index;
    }
    if (part.kind === "tool") {
      return -1;
    }
  }
  return -1;
}

/** Wall-clock span of the work, from the earliest tool start to the latest tool
 * end, or undefined when no tool carries timing yet. */
function workDurationMs(workParts: RenderableMessagePart[]): number | undefined {
  const tools = workParts
    .map((part) => part.tool)
    .filter((tool): tool is ToolExecutionView => Boolean(tool));
  if (tools.length === 0) {
    return undefined;
  }
  const start = Math.min(...tools.map((tool) => tool.createdAt));
  const end = Math.max(...tools.map((tool) => tool.updatedAt));
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) {
    return undefined;
  }
  return end - start;
}

function formatWorkDuration(ms: number): string {
  const totalSeconds = Math.max(0, Math.round(ms / 1000));
  if (totalSeconds < 60) {
    return `${totalSeconds}s`;
  }
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  if (minutes < 60) {
    return seconds ? `${minutes}m ${seconds}s` : `${minutes}m`;
  }
  const hours = Math.floor(minutes / 60);
  const remMinutes = minutes % 60;
  return remMinutes ? `${hours}h ${remMinutes}m` : `${hours}h`;
}

export function MessageParts(props: MessagePartsProps) {
  // Live chat pref: hide everything except the final answer behind one spoiler.
  const collapse = () => chatSettings().collapseWork;

  const split = createMemo(() => {
    const finalIndex = finalTextIndex(props.parts);
    const finalPart = finalIndex >= 0 ? props.parts[finalIndex] : undefined;
    const workParts = props.parts.filter((_, index) => index !== finalIndex);
    return { finalPart, workParts };
  });

  const workLabel = () => {
    if (props.working) {
      return "Working…";
    }
    const ms = workDurationMs(split().workParts);
    return ms != null ? `Worked for ${formatWorkDuration(ms)}` : "Worked";
  };
  const workToolCount = () =>
    split().workParts.filter((part) => part.kind === "tool").length;

  // Parts are rebuilt as FRESH objects on every streaming delta, so an
  // object-keyed <For> would remount the markdown per token. Key the rows by
  // the part's stable id instead (the id memos only notify on structural
  // changes), and let each row read its current part reactively — the part
  // component then persists and SolidMarkdown's reconcile strategy patches
  // just the changed text nodes.
  const partIds = createMemo(
    () => props.parts.map((part) => part.id),
    undefined,
    { equals: sameStringArray },
  );
  const workPartIds = createMemo(
    () => split().workParts.map((part) => part.id),
    undefined,
    { equals: sameStringArray },
  );
  const partById = (id: string) =>
    props.parts.find((part) => part.id === id);

  const finalText = createTypewriterText(
    () => split().finalPart?.text ?? "",
    () => Boolean(props.working),
  );

  const renderPart = (partId: string, textClass: string) => {
    const part = createMemo(() => partById(partId));
    const text = createTypewriterText(
      () => part()?.text ?? "",
      () => Boolean(props.working),
    );
    return (
      <Show when={part()}>
        <Show
          when={part()!.kind === "tool"}
          fallback={
            <div class={textClass}>
              <MessageMarkdown
                content={text()}
                projectId={props.projectId}
                onError={props.onError}
                onPreviewImage={props.onPreviewToolImage}
              />
            </div>
          }
        >
          <ToolPart part={part()!} {...props} />
        </Show>
      </Show>
    );
  };

  return (
    <div class="message-parts">
      <Show
        when={collapse()}
        fallback={
          <For each={partIds()}>
            {(partId) => renderPart(partId, "message-part message-part--text")}
          </For>
        }
      >
        <Show when={split().workParts.length > 0}>
          <WorkSpoiler
            label={workLabel()}
            count={workToolCount() > 0 ? workToolCount() : undefined}
            busy={props.working}
          >
            <For each={workPartIds()}>
              {(partId) =>
                renderPart(
                  partId,
                  "message-part message-part--text work-spoiler__text",
                )
              }
            </For>
          </WorkSpoiler>
        </Show>
        <Show when={split().finalPart}>
          <div class="message-part message-part--text">
            <MessageMarkdown
              content={finalText()}
              projectId={props.projectId}
              onError={props.onError}
              onPreviewImage={props.onPreviewToolImage}
            />
          </div>
        </Show>
      </Show>
    </div>
  );
}

function sameStringArray(a: readonly string[], b: readonly string[]) {
  if (a.length !== b.length) {
    return false;
  }
  for (let index = 0; index < a.length; index += 1) {
    if (a[index] !== b[index]) {
      return false;
    }
  }
  return true;
}

/**
 * Typewriter reveal for streamed text: the wire delivers deltas in coarse
 * batches, but the visible text advances a few characters per animation frame,
 * so generation still reads as live. Catch-up is proportional to the backlog
 * (large backlogs drain in a handful of frames), `active` = false renders the
 * full text instantly (chat reopen, completed messages), and a non-append
 * change (retry/edit rollback) resets without animation.
 */
function createTypewriterText(
  source: () => string,
  active: () => boolean,
): () => string {
  const [visible, setVisible] = createSignal(source());
  let frame = 0;
  let target = untrack(source);

  const step = () => {
    frame = 0;
    const current = untrack(visible);
    if (current.length >= target.length) {
      return;
    }
    const pending = target.length - current.length;
    const chunk = Math.max(1, Math.ceil(pending / 8));
    setVisible(target.slice(0, current.length + chunk));
    if (current.length + chunk < target.length) {
      frame = requestAnimationFrame(step);
    }
  };

  createEffect(() => {
    const next = source();
    const isActive = active();
    target = next;
    if (!isActive || !next.startsWith(untrack(visible))) {
      // Instant: completed message, hydration, or a rewritten (non-appended)
      // text — animating those would look like the app replaying history.
      cancelAnimationFrame(frame);
      frame = 0;
      setVisible(next);
      return;
    }
    if (frame === 0 && next.length > untrack(visible).length) {
      frame = requestAnimationFrame(step);
    }
  });

  onCleanup(() => cancelAnimationFrame(frame));
  return visible;
}

/** One tool part: the resolved inline tool card, or a queued placeholder. */
function ToolPart(props: MessagePartsProps & { part: RenderableMessagePart }) {
  return (
    <div class="message-part message-part--tool">
      {/* NON-keyed <Show>: the tool view object is replaced on every output
          chunk, and a keyed callback would remount the card per chunk. With a
          plain child the card persists and its reactive props update. */}
      <Show
        when={props.part.tool}
        fallback={
          <div class="inline-tool-call inline-tool-call--pending">
            <div class="inline-tool-call__summary">
              <ChevronDown class="inline-tool-call__chevron" size={14} />
              <Terminal size={15} />
              <span class="inline-tool-call__title">Tool call</span>
              <span class="tool-status tool-status--pending">Queued</span>
            </div>
          </div>
        }
      >
        <InlineToolCall
          expanded={Boolean(
            props.expandedInlineTools[props.part.tool!.toolCallId],
          )}
          tool={props.part.tool!}
          changeSets={props.changeSets}
          onApprove={() => props.onApproveTool(props.part.tool!.toolCallId)}
          onCancel={() => props.onCancelTool(props.part.tool!.toolCallId)}
          onDeny={() => props.onDenyTool(props.part.tool!.toolCallId)}
          onPreviewImage={props.onPreviewToolImage}
          onToggle={() => props.onToggleTool(props.part.tool!.toolCallId)}
        />
      </Show>
    </div>
  );
}
