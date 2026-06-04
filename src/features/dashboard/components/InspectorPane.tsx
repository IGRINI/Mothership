import { For, Show, type JSX } from "solid-js";
import { Check, Circle, Square, Terminal, X } from "lucide-solid";

import type { ChatThreadSummary } from "../../../shared/api/mothership";
import { startWindowDrag } from "../../../shared/window-drag";
import {
  formatToolCommand,
  formatToolHeadline,
  isTerminalToolKind,
  toolStatusLabel,
  toolTone,
} from "../tool-format";
import type { ToolExecutionView } from "../types";
import { ToolKindIcon } from "./ToolKindIcon";

export function InspectorPane(props: {
  activeChat: ChatThreadSummary | null;
  messageCount: number;
  toolExecutions: ToolExecutionView[];
  onApproveTool: (toolCallId: string) => void;
  onCancelTool: (toolCallId: string) => void;
  onDenyTool: (toolCallId: string) => void;
}) {
  const activeTools = () =>
    props.toolExecutions.filter((tool) => !isTerminalToolKind(tool.kind)).length;

  return (
    <aside class="inspector-pane" aria-label="Run inspector">
      <header class="inspector-header" onMouseDown={startWindowDrag}>
        <h2>Inspector</h2>
      </header>

      <div class="inspector-scroll">
        <InspectorSection
          title="Current Run"
          action={
            <span class="live-pill">
              <Circle size={8} />
              {activeTools() > 0 ? "Tools" : "Idle"}
            </span>
          }
        >
          <div class="run-summary">
            <div class="run-summary__title">
              <Terminal size={16} />
              <strong>{activeTools() > 0 ? "Tool activity" : "No active run"}</strong>
            </div>
            <span>
              {activeTools() > 0
                ? `${activeTools()} tool job${activeTools() === 1 ? "" : "s"} active`
                : "Chat persistence is enabled"}
            </span>
            <div class="run-summary__footer">
              <span>{props.activeChat?.title ?? "No chat selected"}</span>
              <span>{props.messageCount} messages</span>
            </div>
          </div>
        </InspectorSection>

        <InspectorSection
          title="Tool Calls"
          action={<span class="count-button">{props.toolExecutions.length}</span>}
        >
          <Show
            when={props.toolExecutions.length > 0}
            fallback={<div class="panel-empty">Tool calls will appear here.</div>}
          >
            <div class="tool-call-list">
              <For each={props.toolExecutions}>
                {(tool) => (
                  <ToolCallRow
                    tool={tool}
                    onApprove={() => props.onApproveTool(tool.toolCallId)}
                    onCancel={() => props.onCancelTool(tool.toolCallId)}
                    onDeny={() => props.onDenyTool(tool.toolCallId)}
                  />
                )}
              </For>
            </div>
          </Show>
        </InspectorSection>
      </div>
    </aside>
  );
}

function InspectorSection(props: {
  action?: JSX.Element;
  children: JSX.Element;
  title: string;
}) {
  return (
    <section class="inspector-section">
      <div class="inspector-section__header">
        <h2>{props.title}</h2>
        {props.action}
      </div>
      {props.children}
    </section>
  );
}

function ToolCallRow(props: {
  tool: ToolExecutionView;
  onApprove: () => void;
  onCancel: () => void;
  onDeny: () => void;
}) {
  const command = () => formatToolCommand(props.tool);
  const headline = () => formatToolHeadline(props.tool);
  const canApprove = () => props.tool.kind === "permission_requested";
  const canCancel = () =>
    !canApprove() && !isTerminalToolKind(props.tool.kind);

  return (
    <div class="tool-call-row">
      <ToolKindIcon kind={props.tool.toolKind} />
      <div class="tool-call-row__body">
        <strong title={command() || headline()}>{headline()}</strong>
        <span class={`tool-status tool-status--${toolTone(props.tool.kind)}`}>
          {toolStatusLabel(props.tool.kind)}
        </span>
        <Show when={props.tool.message}>
          <small>{props.tool.message}</small>
        </Show>
        <Show when={props.tool.output}>
          <pre class="tool-call-row__output">{props.tool.output}</pre>
        </Show>
      </div>
      <div class="tool-call-row__actions">
        <Show when={canApprove()}>
          <button type="button" title="Approve tool" onClick={props.onApprove}>
            <Check size={14} />
          </button>
          <button type="button" title="Deny tool" onClick={props.onDeny}>
            <X size={14} />
          </button>
        </Show>
        <Show when={canCancel()}>
          <button type="button" title="Cancel tool" onClick={props.onCancel}>
            <Square size={12} />
          </button>
        </Show>
      </div>
    </div>
  );
}
