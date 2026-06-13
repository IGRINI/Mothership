import type {
  ToolArtifact,
  ToolCommand,
  ToolExecutionEventKind,
  ToolExecutionResult,
  ToolKind,
} from "../../shared/api/mothership";

export interface ToolExecutionView {
  toolCallId: string;
  runId?: string | null;
  chatId?: string | null;
  messageId?: string | null;
  projectId?: string | null;
  command?: ToolCommand | null;
  kind: ToolExecutionEventKind;
  message?: string | null;
  output: string;
  result?: ToolExecutionResult | null;
  toolKind?: ToolKind;
  payload?: Record<string, unknown> | null;
  touchedPaths?: string[];
  artifacts?: ToolArtifact[];
  createdAt: number;
  updatedAt: number;
}

export type ConversationTimelineItem = MessageTimelineItem;

// Timeline items intentionally carry only ids — never the message/tool objects
// themselves. That keeps an item's identity stable across content changes (a
// streaming delta, a tool-output chunk), so the row stays mounted and updates in
// place. Rows read the live message/tool by id from a reactive lookup.
export interface MessageTimelineItem {
  id: string;
  kind: "message";
  messageId: string;
}

export interface MessagePartView {
  id: string;
  kind: "text" | "tool";
  messageId: string;
  text?: string;
  toolCallId?: string;
  createdAt: number;
  sequence?: number;
}

export interface RenderableMessagePart {
  id: string;
  kind: "text" | "tool";
  text?: string;
  tool?: ToolExecutionView;
  toolCallId?: string;
}

export type ProviderStatusTone = "ready" | "warmup" | "loading" | "error" | "idle";

export interface ProviderStatusSummary {
  label: string;
  tone: ProviderStatusTone;
  tooltip: string;
}

export interface SearchSelectOption {
  detail?: string;
  disabled?: boolean;
  label: string;
  searchText?: string;
  status?: {
    label: string;
    tone: ProviderStatusTone;
  };
  title?: string;
  value: string;
}
