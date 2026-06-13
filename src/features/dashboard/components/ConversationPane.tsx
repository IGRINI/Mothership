// The center column: chat header (title, project badge, model/provider
// selectors), the virtualized message list, the error bar, and the composer.
// Pure view — every action is delegated upward through props, and the
// timeline/lookup memos keep rows mounted across streaming updates.

import { createMemo, For, Match, Show, Switch } from "solid-js";
import { Folder, Terminal } from "lucide-solid";

import type {
  ChangeSetSummary,
  ChatMessage,
  ChatThreadSummary,
  ConnectorSettingsSnapshot,
  ProjectSummary,
  RevertOutcome,
  ToolApprovalMode,
} from "../../../shared/api/mothership";
import { VirtualList } from "../../../shared/ui/VirtualList";
import { startWindowDrag } from "../../../shared/window-drag";
import { buildConversationTimeline } from "../message-model";
import {
  selectableConnectorModelFor,
  selectableConnectorProviderFor,
} from "../model-options";
import type { MessagePartView, ToolExecutionView } from "../types";
import type { ToolImagePreviewItem } from "../ToolCards";
import { BrandMark } from "./BrandMark";
import { Composer, type ReasoningOptionId } from "./Composer";
import { MessageRow } from "./MessageRow";
import {
  ApprovalModeMenu,
  FastModeToggle,
  ModelSelector,
  ProviderSelector,
  ProviderStatusDot,
} from "./ModelControls";
import { ReasoningSelector } from "./ReasoningSelector";

export const CHAT_SCROLL_BOTTOM_THRESHOLD_PX = 48;
const CHAT_SCROLL_TOP_PADDING_PX = 12;
const CHAT_SCROLL_BOTTOM_PADDING_PX = 32;

export function ConversationPane(props: {
  activeChat: ChatThreadSummary | null;
  activeProject: ProjectSummary | null;
  activeRunId?: string;
  connectorSettings?: ConnectorSettingsSnapshot;
  draft: string;
  editingDraft: string;
  editingMessageId?: string;
  error: string;
  expandedInlineTools: Record<string, boolean>;
  isLoading: boolean;
  isSending: boolean;
  messagePartsByMessageId: Record<string, MessagePartView[]>;
  messages: ChatMessage[];
  reasoningOptionId?: ReasoningOptionId;
  fastModeEnabled: boolean;
  runTransports: Record<string, string>;
  toolApprovalMode: ToolApprovalMode;
  toolExecutionsByMessageId: Record<string, ToolExecutionView[]>;
  changeSetsByMessageId: Record<string, ChangeSetSummary[]>;
  onApproveTool: (toolCallId: string) => void;
  onRevertChangeSet: (changeSetId: string) => Promise<RevertOutcome>;
  onBranchMessage: (message: ChatMessage) => void;
  onCancelEdit: () => void;
  onCancelRun: () => void;
  onCancelTool: (toolCallId: string) => void;
  onContinue: () => void;
  onDenyTool: (toolCallId: string) => void;
  onDraftChange: (value: string) => void;
  onError: (message: string) => void;
  onEditDraftChange: (value: string) => void;
  onMessageScrollElement: (element: HTMLDivElement | undefined) => void;
  onPreviewToolImage: (
    image: ToolImagePreviewItem,
    images: ToolImagePreviewItem[],
  ) => void;
  onReasoningOptionChange: (optionId: ReasoningOptionId) => void;
  onFastModeChange: (enabled: boolean) => void;
  onRetry: () => void;
  onSelectModel: (providerId: string, modelId: string) => void;
  onToolApprovalModeChange: (mode: ToolApprovalMode) => void;
  onSendMessage: () => void;
  onStartEdit: (message: ChatMessage) => void;
  onSubmitEdit: (messageId: string) => void;
  onToggleInlineTool: (toolCallId: string) => void;
}) {
  const timelineItems = createMemo(() =>
    buildConversationTimeline(props.messages),
  );

  // Live, by-id lookups. Because timeline items hold only ids, a row reads its
  // current message/tool from these maps reactively: a streaming delta or a
  // tool-output chunk updates only the looked-up value, so the row stays mounted
  // and SolidMarkdown's "reconcile" strategy patches just the changed nodes
  // instead of tearing the row down and re-parsing the whole markdown AST.
  const messagesById = createMemo(() => {
    const map: Record<string, ChatMessage> = {};
    for (const message of props.messages) {
      map[message.id] = message;
    }
    return map;
  });
  const toolsById = createMemo(() => {
    const map: Record<string, ToolExecutionView> = {};
    for (const tools of Object.values(props.toolExecutionsByMessageId)) {
      for (const tool of tools) {
        map[tool.toolCallId] = tool;
      }
    }
    return map;
  });
  // The open chat owns its model (provider + model id); fall back to the global
  // default-for-new-chats. The header selectors + status dot derive from this, so
  // they reflect the open chat rather than a global setting.
  const chatModel = createMemo(() => {
    const chat = props.activeChat;
    const settings = props.connectorSettings;
    return {
      providerId: chat?.providerId ?? settings?.selectedModel.providerId,
      modelId: chat?.modelId ?? settings?.selectedModel.modelId,
    };
  });
  const activeProvider = createMemo(() =>
    selectableConnectorProviderFor(props.connectorSettings, chatModel().providerId),
  );
  // Reasoning options must follow the OPEN CHAT's model, not the global one —
  // otherwise the composer offers reasoning levels for the wrong model.
  const activeModel = createMemo(() =>
    selectableConnectorModelFor(
      props.connectorSettings,
      chatModel().providerId,
      chatModel().modelId,
    ),
  );

  return (
    <section class="conversation-pane" aria-label="Active chat">
      <header class="conversation-header" onMouseDown={startWindowDrag}>
        <div class="conversation-header__title">
          <h1>{props.activeChat?.title ?? "New chat"}</h1>
        </div>

        <Show when={props.activeProject}>
          {(project) => (
            <div class="project-badge" title={project().path}>
              <Folder size={14} />
              <span>{project().name}</span>
            </div>
          )}
        </Show>

        <div class="agent-status-chip">
          <Terminal size={16} />
          <ModelSelector
            selected={chatModel()}
            settings={props.connectorSettings}
            onSelectModel={props.onSelectModel}
          />
          <ProviderStatusDot provider={activeProvider()} />
          <ProviderSelector
            selected={chatModel()}
            settings={props.connectorSettings}
            onSelectModel={props.onSelectModel}
          />
        </div>

      </header>

      <VirtualList
        ariaLabel="Chat messages"
        class="message-list"
        empty={
          <ConversationState
            error={props.error}
            hasProject={Boolean(props.activeProject)}
            isLoading={props.isLoading}
            messageCount={props.activeChat?.messageCount}
          />
        }
        estimateSize={140}
        getItemKey={(item) => item.id}
        items={timelineItems()}
        overscan={8}
        paddingEnd={CHAT_SCROLL_BOTTOM_PADDING_PX}
        paddingStart={CHAT_SCROLL_TOP_PADDING_PX}
        scrollRef={props.onMessageScrollElement}
        stickToEnd
        stickToEndThreshold={CHAT_SCROLL_BOTTOM_THRESHOLD_PX}
      >
        {(item) => {
          // Per-row memo: it re-runs on every messages/tools change but, thanks to
          // createMemo's `===` dedup, only *notifies* (and so only re-renders the
          // markdown) when this row's own message/tool object actually changes.
          // Without it, every row would re-parse on each streaming delta because
          // they all read the shared by-id map.
          //
          // The <Show> is deliberately NON-keyed: a keyed callback would tear the
          // row down and remount MessageRow every time the message object is
          // replaced — i.e. on every streaming delta. With a plain child the
          // component instance persists and only its reactive props update.
          const message = createMemo(() => messagesById()[item.messageId]);
          return (
            <Show when={message()}>
              <MessageRow
                message={message()!}
                projectId={props.activeProject?.id}
                transport={props.runTransports[item.messageId]}
                expandedInlineTools={props.expandedInlineTools}
                editingDraft={props.editingDraft}
                isEditing={props.editingMessageId === item.messageId}
                isBusy={props.isSending}
                parts={props.messagePartsByMessageId[item.messageId] ?? []}
                onBranchMessage={props.onBranchMessage}
                onCancelEdit={props.onCancelEdit}
                onCancelTool={props.onCancelTool}
                onContinue={props.onContinue}
                onDenyTool={props.onDenyTool}
                onError={props.onError}
                onEditDraftChange={props.onEditDraftChange}
                onPreviewToolImage={props.onPreviewToolImage}
                onRetry={props.onRetry}
                settings={props.connectorSettings}
                onStartEdit={props.onStartEdit}
                onSubmitEdit={props.onSubmitEdit}
                onApproveTool={props.onApproveTool}
                onToggleInlineTool={props.onToggleInlineTool}
                tools={props.toolExecutionsByMessageId[item.messageId] ?? []}
                toolsById={toolsById()}
                changeSets={props.changeSetsByMessageId[item.messageId] ?? []}
                onRevertChangeSet={props.onRevertChangeSet}
              />
            </Show>
          );
        }}
      </VirtualList>

      <Show when={props.error}>
        <div class="chat-error" role="alert">
          {props.error}
        </div>
      </Show>

      <Composer
        activeRunId={props.activeRunId}
        draft={props.draft}
        hasProject={Boolean(props.activeProject)}
        isSending={props.isSending}
        model={activeModel()}
        reasoningOptionId={props.reasoningOptionId}
        fastModeEnabled={props.fastModeEnabled}
        toolApprovalMode={props.toolApprovalMode}
        onCancelRun={props.onCancelRun}
        onDraftChange={props.onDraftChange}
        onReasoningOptionChange={props.onReasoningOptionChange}
        onFastModeChange={props.onFastModeChange}
        onToolApprovalModeChange={props.onToolApprovalModeChange}
        onSend={props.onSendMessage}
        ReasoningSelector={ReasoningSelector}
        FastModeToggle={FastModeToggle}
        ApprovalModeMenu={ApprovalModeMenu}
      />
    </section>
  );
}

// IMPORTANT: branch with <Switch>, not early `return`s. A component body runs
// once; `if (!props.hasProject) return ...` would freeze on whatever was true at
// mount (e.g. "No project selected" while the snapshot is still loading) and
// never update when the project resolves — even though the header badge and
// sidebar highlight (both reactive <Show>s) correctly show the project.
function ConversationState(props: {
  error: string;
  hasProject: boolean;
  isLoading: boolean;
  messageCount?: number;
}) {
  // Skeleton rows mirror the chat about to render: same role parity as the
  // real messages (chats start with a user message), capped to a screenful.
  // The CSS bottom-anchors them and delays their appearance by 200ms, so a
  // fast local load never shows a skeleton at all.
  const skeletonRoles = createMemo(() => {
    const total = Math.max(1, props.messageCount ?? 4);
    const visible = Math.min(total, 6);
    return Array.from({ length: visible }, (_, row) => {
      const messageIndex = total - visible + row;
      return messageIndex % 2 === 0
        ? ("user" as const)
        : ("assistant" as const);
    });
  });

  return (
    <Switch
      fallback={
        <div class="conversation-state">
          <BrandMark compact />
          <strong>New chat</strong>
          <span>Describe a task for the agent and press Enter to start.</span>
        </div>
      }
    >
      <Match when={props.error}>
        <div class="conversation-state conversation-state--error">
          {props.error}
        </div>
      </Match>
      <Match when={props.isLoading}>
        <div class="conversation-state conversation-state--loading">
          <For each={skeletonRoles()}>
            {(role, index) => (
              <Show
                when={role === "assistant"}
                fallback={
                  <div
                    class={`message-skeleton message-skeleton--user message-skeleton--w${(index() % 3) + 1}`}
                  >
                    <div class="message-skeleton__bubble" />
                  </div>
                }
              >
                <div class="message-skeleton message-skeleton--assistant">
                  <div class="message-skeleton__avatar" />
                  <div class="message-skeleton__line message-skeleton__line--name" />
                  <div class="message-skeleton__line" />
                  <div class="message-skeleton__line message-skeleton__line--full" />
                  <Show when={index() === skeletonRoles().length - 1}>
                    <div class="message-skeleton__line message-skeleton__line--full message-skeleton__line--short" />
                  </Show>
                </div>
              </Show>
            )}
          </For>
        </div>
      </Match>
      <Match when={!props.hasProject}>
        <div class="conversation-state">
          <Folder size={22} />
          <strong>Open a project</strong>
          <span>No project selected.</span>
        </div>
      </Match>
    </Switch>
  );
}
