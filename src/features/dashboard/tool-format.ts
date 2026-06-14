import type { ToolExecutionEventKind } from "../../shared/api/mothership";
import { commandPresentation } from "../../shared/toolCommandPresentation";
import type { ToolExecutionView } from "./types";

export function formatToolCommand(tool: ToolExecutionView): string {
  return commandPresentation(tool).commandLine;
}

export function formatToolHeadline(tool: ToolExecutionView) {
  if (tool.toolKind === "run_command" || tool.command) {
    return commandPresentation(tool).headline;
  }

  const path = toolPath(tool);
  switch (tool.toolKind) {
    case "read_file":
      return path ? `Read ${path}` : "Read file";
    case "write_file":
      return path ? `Wrote ${path}` : "Wrote file";
    case "edit_file":
      return path ? `Edited ${path}` : "Edited file";
    case "apply_patch":
      return "Applied patch";
    case "search_text":
      return "Searched";
    case "image_generate":
      return "Generated image";
    default:
      return "Used tool";
  }
}

export function formatToolOutput(tool: ToolExecutionView) {
  if (tool.output) {
    return tool.output;
  }

  if (!tool.result) {
    return "";
  }

  const stdout = (tool.result.stdoutPreview || tool.result.stdoutTail || "").trim();
  const stderr = (tool.result.stderrPreview || tool.result.stderrTail || "").trim();

  // The result message is rendered on its own line, so do not repeat it inside
  // the output block.
  return [stdout, stderr ? `[stderr]\n${stderr}` : ""]
    .filter(Boolean)
    .join("\n\n");
}

export function countTextLines(text: string) {
  const visibleText = text.replace(/(?:\r\n|\r|\n)+$/, "");
  if (visibleText.length === 0) {
    return 1;
  }

  return visibleText.split(/\r\n|\r|\n/).length;
}

export function shouldShowInlineToolOutput(tool: ToolExecutionView, output: string) {
  if (output.trim().length > 0) {
    return true;
  }

  return tool.kind === "started" || tool.kind === "output";
}

export function isTerminalToolKind(kind: ToolExecutionEventKind) {
  return (
    kind === "completed" ||
    kind === "failed" ||
    kind === "cancelled" ||
    kind === "timed_out" ||
    kind === "permission_denied" ||
    kind === "loop_blocked"
  );
}

export function toolStatusLabel(kind: ToolExecutionEventKind) {
  const labels: Record<ToolExecutionEventKind, string> = {
    queued: "Queued",
    permission_requested: "Needs approval",
    permission_denied: "Denied",
    waiting_for_resource: "Waiting",
    started: "Running",
    output: "Running",
    backgrounded: "Background",
    completed: "Completed",
    failed: "Failed",
    cancelled: "Cancelled",
    timed_out: "Timed out",
    loop_blocked: "Blocked",
  };
  return labels[kind];
}

export function toolTone(kind: ToolExecutionEventKind) {
  if (kind === "completed") {
    return "done";
  }
  if (
    kind === "failed" ||
    kind === "permission_denied" ||
    kind === "timed_out" ||
    kind === "cancelled"
  ) {
    return "error";
  }
  if (
    kind === "permission_requested" ||
    kind === "waiting_for_resource" ||
    kind === "loop_blocked"
  ) {
    return "pending";
  }
  return "running";
}

function toolPath(tool: ToolExecutionView): string | undefined {
  const path = tool.payload?.["path"];
  return typeof path === "string" && path ? path : undefined;
}
