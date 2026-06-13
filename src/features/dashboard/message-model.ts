// Pure chat view-model helpers: merging wire messages/chats into local state,
// ordering message parts, deriving renderable parts, accumulating streamed tool
// state, and the small formatting utilities the conversation UI shares. No
// signals, no JSX — everything here is a plain function over immutable inputs
// (callers pass current state in and set the returned value).

import type {
  ChatMessage,
  ChatMessagePart,
  ChatThreadSummary,
  ConnectorSettingsSnapshot,
  JsonValue,
  SendChatMessageResult,
  ToolArtifact,
  ToolExecutionRecord,
} from "../../shared/api/mothership";
import type {
  ConversationTimelineItem,
  MessagePartView,
  RenderableMessagePart,
  ToolExecutionView,
} from "./types";

export const CHAT_MESSAGE_PAGE_SIZE = 60;

/** Replace (or append) one chat summary without reordering the list. */
export function mergeChatInPlace(
  current: ChatThreadSummary[],
  chat: ChatThreadSummary,
): ChatThreadSummary[] {
  const existingIndex = current.findIndex((item) => item.id === chat.id);
  if (existingIndex === -1) {
    return [...current, chat];
  }

  const next = [...current];
  next[existingIndex] = chat;
  return next;
}

/** Move (or insert) a chat to the top — freshest activity first. */
export function bumpChat(
  current: ChatThreadSummary[],
  chat: ChatThreadSummary,
): ChatThreadSummary[] {
  return [chat, ...current.filter((item) => item.id !== chat.id)];
}

export function mergeMessages(
  current: readonly unknown[],
  incoming: readonly unknown[],
): ChatMessage[] {
  const messagesById = new Map<string, ChatMessage>();
  for (const message of current) {
    if (!isChatMessage(message)) {
      continue;
    }
    messagesById.set(message.id, message);
  }
  for (const message of incoming) {
    if (!isChatMessage(message)) {
      continue;
    }
    messagesById.set(message.id, message);
  }

  return Array.from(messagesById.values()).sort(compareMessages);
}

export function removedMessageIdsForRetry(
  result: SendChatMessageResult,
  current: ChatMessage[],
): string[] {
  if (result.removedMessageIds && result.removedMessageIds.length > 0) {
    return result.removedMessageIds;
  }

  return current
    .filter(
      (message) =>
        message.chatId === result.chat.id &&
        message.position > result.userMessage.position,
    )
    .map((message) => message.id);
}

export function appendMessageDelta(
  current: ChatMessage[],
  messageId: string,
  delta: string,
): ChatMessage[] {
  let changed = false;
  const next = current.map((message) => {
    if (message.id !== messageId) {
      return message;
    }
    changed = true;
    return {
      ...message,
      content: `${message.content}${delta}`,
    };
  });

  return changed ? next : current;
}

export function messagePartsByMessageId(
  parts: ChatMessagePart[],
): Record<string, MessagePartView[]> {
  const grouped: Record<string, MessagePartView[]> = {};

  for (const part of parts) {
    grouped[part.messageId] ??= [];
    grouped[part.messageId].push(messagePartViewFromApi(part));
  }

  for (const messageParts of Object.values(grouped)) {
    messageParts.sort(compareMessageParts);
  }

  return grouped;
}

function messagePartViewFromApi(part: ChatMessagePart): MessagePartView {
  return {
    id: `part:${part.id}`,
    kind: part.kind,
    messageId: part.messageId,
    text: part.text ?? undefined,
    toolCallId: part.toolCallId ?? undefined,
    createdAt: timestampToMillis(part.createdAt),
    sequence: part.id,
  };
}

export function compareMessageParts(
  left: MessagePartView,
  right: MessagePartView,
) {
  if (left.sequence !== undefined && right.sequence !== undefined) {
    return left.sequence - right.sequence;
  }

  if (left.createdAt !== right.createdAt) {
    return left.createdAt - right.createdAt;
  }

  return left.id.localeCompare(right.id);
}

export function buildRenderableMessageParts(
  message: ChatMessage,
  parts: MessagePartView[],
  tools: ToolExecutionView[],
  toolsById: Record<string, ToolExecutionView>,
  transport?: string,
): RenderableMessagePart[] {
  const orderedParts = [...parts].sort(compareMessageParts);
  const renderedTools = new Set<string>();
  const renderedParts: RenderableMessagePart[] = [];

  for (const part of orderedParts) {
    if (part.kind === "text") {
      if (part.text) {
        renderedParts.push({
          id: part.id,
          kind: "text",
          text: part.text,
        });
      }
      continue;
    }

    if (!part.toolCallId) {
      continue;
    }

    renderedTools.add(part.toolCallId);
    renderedParts.push({
      id: part.id,
      kind: "tool",
      tool: toolsById[part.toolCallId],
      toolCallId: part.toolCallId,
    });
  }

  if (orderedParts.length === 0) {
    const fallback = fallbackMessageBody(message, transport);
    if (fallback) {
      renderedParts.push({
        id: `text:${message.id}:fallback`,
        kind: "text",
        text: fallback,
      });
    }
  }

  for (const tool of tools) {
    if (renderedTools.has(tool.toolCallId)) {
      continue;
    }

    renderedParts.push({
      id: `tool:${tool.toolCallId}`,
      kind: "tool",
      tool,
      toolCallId: tool.toolCallId,
    });
  }

  if (renderedParts.length === 0) {
    const fallback = fallbackMessageBody(message, transport);
    if (fallback) {
      renderedParts.push({
        id: `text:${message.id}:fallback`,
        kind: "text",
        text: fallback,
      });
    }
  }

  return renderedParts;
}

function fallbackMessageBody(message: ChatMessage, transport?: string) {
  return (
    message.content ||
    (message.status === "cancelled"
      ? "Response cancelled."
      : message.status === "sending"
        ? thinkingLabel(transport)
        : message.status === "failed"
          ? ""
        : "No content.")
  );
}

export function attachMessageIdToToolExecutions(
  current: Record<string, ToolExecutionView>,
  runId: string,
  messageId: string,
): Record<string, ToolExecutionView> {
  let changed = false;
  const next: Record<string, ToolExecutionView> = {};

  for (const [toolCallId, tool] of Object.entries(current)) {
    if (tool.runId === runId && tool.messageId !== messageId) {
      changed = true;
      next[toolCallId] = { ...tool, messageId };
    } else {
      next[toolCallId] = tool;
    }
  }

  return changed ? next : current;
}

export function toolExecutionViewFromRecord(
  record: ToolExecutionRecord,
): ToolExecutionView {
  return {
    toolCallId: record.toolCallId,
    runId: record.runId,
    chatId: record.chatId,
    messageId: record.messageId,
    projectId: record.projectId,
    command: record.command,
    kind: record.kind,
    message: record.message,
    output: record.output,
    result: record.result,
    toolKind: record.toolKind ?? undefined,
    payload: payloadObject(record.payload),
    // A persisted record carries no per-event touched-paths list (those are
    // accumulated from the live event stream); the record's artifacts stand in.
    touchedPaths: undefined,
    artifacts: record.artifacts,
    createdAt: timestampToMillis(record.createdAt),
    updatedAt: timestampToMillis(record.updatedAt),
  };
}

// The wire `payload` is an arbitrary JSON value (`JsonValue`), but the tool
// cards only make sense of object payloads (they index keys like `program` /
// `path`). Narrow to a plain object, treating scalar/array/null payloads as
// "no semantic payload" so the view-model field stays `Record<string, unknown>`.
export function payloadObject(
  value: JsonValue | null | undefined,
): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

// Accumulate workspace-relative paths across events, preserving first-seen
// order and dropping duplicates. Returns undefined when nothing is known yet.
export function mergeTouchedPaths(
  previous: string[] | undefined,
  incoming: string[] | undefined,
): string[] | undefined {
  if (!incoming || incoming.length === 0) {
    return previous;
  }
  if (!previous || previous.length === 0) {
    return [...incoming];
  }

  const merged = [...previous];
  const seen = new Set(previous);
  for (const path of incoming) {
    if (!seen.has(path)) {
      seen.add(path);
      merged.push(path);
    }
  }
  return merged;
}

// Accumulate artifacts across events, deduping by artifactId and letting a
// later event replace an earlier artifact with the same id (e.g. a diff that
// grew and spilled to a logRef).
export function mergeToolArtifacts(
  previous: ToolArtifact[] | undefined,
  incoming: ToolArtifact[] | undefined,
): ToolArtifact[] | undefined {
  if (!incoming || incoming.length === 0) {
    return previous;
  }
  if (!previous || previous.length === 0) {
    return [...incoming];
  }

  const merged = [...previous];
  for (const artifact of incoming) {
    const index = merged.findIndex(
      (existing) => existing.artifactId === artifact.artifactId,
    );
    if (index === -1) {
      merged.push(artifact);
    } else {
      merged[index] = artifact;
    }
  }
  return merged;
}

export function compareToolExecutions(
  left: ToolExecutionView,
  right: ToolExecutionView,
) {
  if (left.createdAt !== right.createdAt) {
    return left.createdAt - right.createdAt;
  }

  return left.toolCallId.localeCompare(right.toolCallId);
}

export function buildConversationTimeline(
  messages: ChatMessage[],
): ConversationTimelineItem[] {
  return messages
    .filter(isChatMessage)
    .map((message) => ({
      id: `message:${message.id}`,
      kind: "message",
      messageId: message.id,
    }));
}

export function normalizeMessages(messages: ChatMessage[]) {
  return mergeMessages([], messages);
}

export function isChatMessage(value: unknown): value is ChatMessage {
  if (!value || typeof value !== "object") {
    return false;
  }

  const message = value as Partial<ChatMessage>;
  return (
    typeof message.id === "string" &&
    typeof message.chatId === "string" &&
    typeof message.position === "number" &&
    (message.role === "user" || message.role === "assistant") &&
    typeof message.content === "string" &&
    (message.status === "complete" ||
      message.status === "cancelled" ||
      message.status === "failed" ||
      message.status === "sending") &&
    typeof message.createdAt === "string"
  );
}

export function limitChatMessages(messages: ChatMessage[]) {
  return messages.slice(-CHAT_MESSAGE_PAGE_SIZE);
}

function compareMessages(left: ChatMessage, right: ChatMessage) {
  if (left.position !== right.position) {
    return left.position - right.position;
  }

  const leftTime = Number(left.createdAt);
  const rightTime = Number(right.createdAt);
  if (Number.isFinite(leftTime) && Number.isFinite(rightTime) && leftTime !== rightTime) {
    return leftTime - rightTime;
  }

  const roleOrder = (message: ChatMessage) => (message.role === "user" ? 0 : 1);
  const roleDifference = roleOrder(left) - roleOrder(right);
  if (roleDifference !== 0) {
    return roleDifference;
  }

  return left.id.localeCompare(right.id);
}

export function formatMessageTime(timestamp: string) {
  const date = unixTimestampToDate(timestamp);
  return date.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function thinkingLabel(transport?: string) {
  if (!transport) {
    return "Thinking...";
  }

  const label: Record<string, string> = {
    http_json: "HTTP JSON",
    http_sse: "HTTP streaming",
    websocket: "WebSocket",
  };

  return `Thinking via ${label[transport] ?? transport}...`;
}

function unixTimestampToDate(timestamp: string) {
  const numericTimestamp = Number(timestamp);
  return new Date(
    Number.isFinite(numericTimestamp) ? numericTimestamp * 1000 : Date.now(),
  );
}

export function timestampToMillis(timestamp: string) {
  const numericTimestamp = Number(timestamp);
  return Number.isFinite(numericTimestamp) ? numericTimestamp * 1000 : Date.now();
}

export function currentUnixTimestamp() {
  return Math.floor(Date.now() / 1000).toString();
}

export function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Resolves an assistant message's provider/model into a display name + adapter
 * icon, using the live connector list. Falls back to "Mothership" (and the
 * built-in logo) for user messages or when the producing adapter is unknown.
 */
export function resolveAttribution(
  settings: ConnectorSettingsSnapshot | undefined,
  providerId?: string | null,
  modelId?: string | null,
): { name: string; icon?: string | null } {
  if (!providerId) {
    return { name: "Mothership", icon: undefined };
  }
  const provider = settings?.providers.find((item) => item.id === providerId);
  const model = provider?.models.find((item) => item.id === modelId);
  const name = model?.label ?? modelId ?? provider?.label ?? "Mothership";
  return { name, icon: provider?.icon ?? undefined };
}

/**
 * Turns a raw run error into a one-line human summary: strips our internal
 * wrapper prefixes and, if the provider returned a JSON body, surfaces its
 * detail/message. The full original string is still shown under "Details".
 */
export function humanizeError(raw: string): string {
  let message = (raw ?? "").trim();

  for (const prefix of [
    "invalid request:",
    "adapter chat failed:",
    "adapter chat error:",
  ]) {
    if (message.toLowerCase().startsWith(prefix)) {
      message = message.slice(prefix.length).trim();
    }
  }

  const jsonStart = message.indexOf("{");
  if (jsonStart !== -1) {
    try {
      const parsed = JSON.parse(message.slice(jsonStart));
      const detail =
        parsed?.detail ??
        parsed?.message ??
        parsed?.error?.message ??
        (typeof parsed?.error === "string" ? parsed.error : undefined);
      if (typeof detail === "string" && detail.trim()) {
        return detail.trim();
      }
    } catch {
      // Not JSON — fall through to the cleaned string.
    }
  }

  return message || "The model provider could not complete this request.";
}

export function appendToolOutput(
  current: string,
  stream: "stdout" | "stderr" | null | undefined,
  chunk: string,
) {
  const prefix = stream === "stderr" ? "[stderr] " : "";
  const next = `${current}${prefix}${chunk}`;
  const max = 12_000;
  return next.length > max ? `... output trimmed ...\n${next.slice(-max)}` : next;
}
