import { Show, type JSX } from "solid-js";
import { Send, Square } from "lucide-solid";

import type { LlmModel } from "../../../shared/api/mothership";

export type ReasoningOptionId = string;

export function Composer(props: {
  activeRunId?: string;
  draft: string;
  hasProject: boolean;
  isSending: boolean;
  model?: LlmModel;
  reasoningOptionId?: ReasoningOptionId;
  onCancelRun: () => void;
  onDraftChange: (value: string) => void;
  onReasoningOptionChange: (optionId: ReasoningOptionId) => void;
  onSend: () => void;
  ReasoningSelector: (props: {
    disabled: boolean;
    model?: LlmModel;
    value?: ReasoningOptionId;
    onChange: (optionId: ReasoningOptionId) => void;
  }) => JSX.Element;
}) {
  const ReasoningSelector = props.ReasoningSelector;
  const canSend = () =>
    props.hasProject && props.draft.trim().length > 0 && !props.isSending;

  return (
    <form
      class="composer"
      onSubmit={(event) => {
        event.preventDefault();
        props.onSend();
      }}
    >
      <textarea
        rows={2}
        placeholder={props.hasProject ? "Ask Mothership anything..." : "Open a project..."}
        value={props.draft}
        disabled={!props.hasProject}
        onInput={(event) => props.onDraftChange(event.currentTarget.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
            event.preventDefault();
            props.onSend();
          }
        }}
      />
      <div class="composer__actions">
        <ReasoningSelector
          disabled={props.isSending || Boolean(props.activeRunId)}
          model={props.model}
          value={props.reasoningOptionId}
          onChange={props.onReasoningOptionChange}
        />
        <Show
          when={props.activeRunId}
          fallback={
            <button
              class="send-button"
              disabled={!canSend()}
              type="submit"
              title="Send message"
            >
              <Send size={16} />
            </button>
          }
        >
          <button
            class="send-button send-button--stop"
            type="button"
            title="Stop response"
            onClick={props.onCancelRun}
          >
            <Square size={13} />
          </button>
        </Show>
      </div>
    </form>
  );
}
