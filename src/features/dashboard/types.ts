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
