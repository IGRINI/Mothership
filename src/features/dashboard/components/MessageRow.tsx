// One conversation row: user bubble or attributed assistant message (avatar,
// model name, parts, change sets, error card), plus the inline editor for
// re-sending an edited user message.

import { createMemo, onMount, Show } from "solid-js";
import {
  AlertTriangle,
  ChevronDown,
  Play,
  RefreshCw,
  Send,
  X,
} from "lucide-solid";

import type {
  ChangeSetSummary,
  ChatMessage,
  ConnectorSettingsSnapshot,
  RevertOutcome,
} from "../../../shared/api/mothership";
import {
  buildRenderableMessageParts,
  formatMessageTime,
  humanizeError,
  resolveAttribution,
  thinkingLabel,
} from "../message-model";
import type { MessagePartView, ToolExecutionView } from "../types";
import type { ToolImagePreviewItem } from "../ToolCards";
import { BrandMark } from "./BrandMark";
import { ChangeSetGroup } from "./ChangeSetGroup";
import { MessageMarkdown } from "./MessageMarkdown";
import { MessageParts } from "./MessageParts";

export function MessageRow(props: {
  message: ChatMessage;
  projectId?: string;
  transport?: string;
  expandedInlineTools: Record<string, boolean>;
  editingDraft: string;
  isEditing: boolean;
  isBusy?: boolean;
  parts: MessagePartView[];
  tools: ToolExecutionView[];
  toolsById: Record<string, ToolExecutionView>;
  changeSets: ChangeSetSummary[];
  onApproveTool: (toolCallId: string) => void;
  onRevertChangeSet: (changeSetId: string) => Promise<RevertOutcome>;
  onBranchMessage: (message: ChatMessage) => void;
  onCancelEdit: () => void;
  onCancelTool: (toolCallId: string) => void;
  onContinue?: () => void;
  onDenyTool: (toolCallId: string) => void;
  onError: (message: string) => void;
  onEditDraftChange: (value: string) => void;
  onPreviewToolImage: (
    image: ToolImagePreviewItem,
    images: ToolImagePreviewItem[],
  ) => void;
  onRetry?: () => void;
  settings?: ConnectorSettingsSnapshot;
  onStartEdit: (message: ChatMessage) => void;
  onSubmitEdit: (messageId: string) => void;
  onToggleInlineTool: (toolCallId: string) => void;
}) {
  const message = () => props.message;
  const isUser = () => message().role === "user";
  const isFailed = () =>
    message().role === "assistant" && message().status === "failed";
  const attribution = () =>
    resolveAttribution(props.settings, message().providerId, message().modelId);
  const body = () =>
    message().content ||
    (message().status === "cancelled"
      ? "Response cancelled."
      : message().status === "sending"
        ? thinkingLabel(props.transport)
        : "No content.");
  const assistantParts = createMemo(() =>
    buildRenderableMessageParts(
      message(),
      props.parts,
      props.tools,
      props.toolsById,
      props.transport,
    ),
  );

  // Messenger layout: user on the right in a colored bubble, agent on the left
  // with an avatar. A failed run keeps the partial assistant response visible
  // and appends the error controls underneath it.
  return (
    <article
      classList={{
        "message-row": true,
        "message-row--user": isUser(),
        "message-row--assistant": !isUser(),
      }}
    >
      <Show when={!isUser()}>
        <Avatar role="assistant" iconUrl={attribution().icon} />
      </Show>
      <div class="message-row__content">
        <Show when={!isUser()}>
          <div class="message-meta">
            <strong>{attribution().name}</strong>
            <span>{formatMessageTime(message().createdAt)}</span>
          </div>
        </Show>
        <Show
          when={props.isEditing}
          fallback={
            <>
              <Show
                when={!isUser()}
                fallback={
                  <MessageMarkdown
                    content={body()}
                    projectId={props.projectId}
                    onError={props.onError}
                    onPreviewImage={props.onPreviewToolImage}
                  />
                }
              >
                <MessageParts
                  expandedInlineTools={props.expandedInlineTools}
                  parts={assistantParts()}
                  projectId={props.projectId}
                  working={message().status === "sending"}
                  changeSets={props.changeSets}
                  onApproveTool={props.onApproveTool}
                  onCancelTool={props.onCancelTool}
                  onDenyTool={props.onDenyTool}
                  onError={props.onError}
                  onPreviewToolImage={props.onPreviewToolImage}
                  onToggleTool={props.onToggleInlineTool}
                />
              </Show>
            </>
          }
        >
          <MessageEditor
            disabled={Boolean(props.isBusy)}
            messageId={message().id}
            value={props.editingDraft}
            onCancel={props.onCancelEdit}
            onChange={props.onEditDraftChange}
            onSubmit={props.onSubmitEdit}
          />
        </Show>
        <Show when={!isUser() && props.changeSets.length > 0}>
          <div class="change-sets">
            <ChangeSetGroup
              changeSets={props.changeSets}
              projectId={props.projectId}
              onRevert={props.onRevertChangeSet}
            />
          </div>
        </Show>
        <Show when={isFailed()}>
          <ErrorCard
            error={message().error ?? "The run failed before Core recorded an error."}
            disabled={Boolean(props.isBusy)}
            onContinue={props.onContinue}
            onRetry={props.onRetry}
          />
        </Show>
      </div>
    </article>
  );
}

function MessageEditor(props: {
  disabled: boolean;
  messageId: string;
  value: string;
  onCancel: () => void;
  onChange: (value: string) => void;
  onSubmit: (messageId: string) => void;
}) {
  const canSubmit = () => props.value.trim().length > 0 && !props.disabled;
  let textareaRef: HTMLTextAreaElement | undefined;

  onMount(() => {
    textareaRef?.focus();
    textareaRef?.setSelectionRange(textareaRef.value.length, textareaRef.value.length);
  });

  return (
    <form
      class="message-editor"
      onSubmit={(event) => {
        event.preventDefault();
        props.onSubmit(props.messageId);
      }}
    >
      <textarea
        ref={textareaRef}
        rows={3}
        value={props.value}
        disabled={props.disabled}
        onInput={(event) => props.onChange(event.currentTarget.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
            event.preventDefault();
            props.onSubmit(props.messageId);
          }
          if (event.key === "Escape") {
            event.preventDefault();
            props.onCancel();
          }
        }}
      />
      <div class="message-editor__actions">
        <button
          class="message-editor__button"
          type="submit"
          title="Send edited message"
          disabled={!canSubmit()}
        >
          <Send size={14} />
        </button>
        <button
          class="message-editor__button"
          type="button"
          title="Cancel edit"
          disabled={props.disabled}
          onClick={props.onCancel}
        >
          <X size={14} />
        </button>
      </div>
    </form>
  );
}

function ErrorCard(props: {
  error: string;
  disabled: boolean;
  onContinue?: () => void;
  onRetry?: () => void;
}) {
  return (
    <div class="chat-error-card" role="alert">
      <div class="chat-error-card__head">
        <AlertTriangle size={16} />
        <strong>Couldn't get a response</strong>
      </div>
      <p class="chat-error-card__summary">{humanizeError(props.error)}</p>
      <div class="chat-error-card__actions">
        <Show when={props.onContinue}>
          <button
            class="chat-error-card__continue"
            type="button"
            disabled={props.disabled}
            onClick={() => props.onContinue?.()}
          >
            <Play size={14} />
            Continue
          </button>
        </Show>
        <Show when={props.onRetry}>
          <button
            class="chat-error-card__retry"
            type="button"
            disabled={props.disabled}
            onClick={() => props.onRetry?.()}
          >
            <RefreshCw size={14} />
            Retry
          </button>
        </Show>
        <details class="chat-error-card__details">
          <summary>
            Details
            <ChevronDown size={13} />
          </summary>
          <pre class="chat-error-card__raw">{props.error}</pre>
        </details>
      </div>
    </div>
  );
}

function Avatar(props: { role: "assistant" | "user"; iconUrl?: string | null }) {
  if (props.role === "user") {
    return <div class="avatar avatar--message avatar--user">You</div>;
  }

  return (
    <div class="avatar avatar--message avatar--agent">
      <Show when={props.iconUrl} fallback={<BrandMark compact />}>
        <img
          class="avatar__adapter-icon"
          src={props.iconUrl!}
          alt=""
          draggable={false}
        />
      </Show>
    </div>
  );
}
