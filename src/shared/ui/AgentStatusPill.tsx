// The status-bar agent pill: a live summary of every agent run across all
// projects. Green dot + count while agents work (amber when one is blocked on
// an approval), gray when idle, and a messenger-style unread counter for
// finished runs the user hasn't reviewed. Hover (or click to pin) opens an
// upward popover listing each agent — project, chat, live status, elapsed —
// with one-click switching to its chat and a Stop control.

import { createEffect, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { Square } from "lucide-solid";

import {
  activeAgents,
  anyAgentAwaitingApproval,
  clearFinishedAgents,
  dismissFinishedAgent,
  formatAgoMs,
  formatElapsedMs,
  requestAgentFocus,
  unseenFinishedAgents,
  type ActiveAgent,
  type AgentOutcome,
  type FinishedAgent,
} from "../agentActivity";
import { cancelChatRun } from "../api/mothership";

const OUTCOME_LABELS: Record<AgentOutcome, string> = {
  completed: "Finished",
  failed: "Failed",
  cancelled: "Stopped",
  interrupted: "Interrupted",
};

export function AgentStatusPill() {
  const [isOpen, setIsOpen] = createSignal(false);
  const [isPinned, setIsPinned] = createSignal(false);
  const [now, setNow] = createSignal(Date.now());
  let rootRef: HTMLDivElement | undefined;
  let closeTimer: number | undefined;

  const runningCount = () => activeAgents().length;
  const unseenCount = () => unseenFinishedAgents().length;
  const tone = () =>
    runningCount() === 0
      ? "idle"
      : anyAgentAwaitingApproval()
        ? "attention"
        : "busy";
  const label = () =>
    runningCount() === 0
      ? "Agents"
      : `${runningCount()} running`;

  // Tick once a second while the popover shows elapsed times.
  createEffect(() => {
    if (!isOpen()) {
      return;
    }
    setNow(Date.now());
    const interval = window.setInterval(() => setNow(Date.now()), 1000);
    onCleanup(() => window.clearInterval(interval));
  });

  const cancelScheduledClose = () => {
    if (closeTimer !== undefined) {
      window.clearTimeout(closeTimer);
      closeTimer = undefined;
    }
  };
  const open = () => {
    cancelScheduledClose();
    setIsOpen(true);
  };
  const close = () => {
    cancelScheduledClose();
    setIsOpen(false);
    setIsPinned(false);
  };
  const scheduleClose = () => {
    if (isPinned()) {
      return;
    }
    cancelScheduledClose();
    // Grace period so the cursor can travel from the pill into the popover.
    closeTimer = window.setTimeout(() => {
      closeTimer = undefined;
      setIsOpen(false);
    }, 140);
  };

  onMount(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && isOpen()) {
        close();
      }
    };
    const handlePointerDown = (event: PointerEvent) => {
      if (!isOpen() || !rootRef) {
        return;
      }
      if (event.target instanceof Node && !rootRef.contains(event.target)) {
        close();
      }
    };
    document.addEventListener("keydown", handleKeyDown);
    document.addEventListener("pointerdown", handlePointerDown);
    onCleanup(() => {
      document.removeEventListener("keydown", handleKeyDown);
      document.removeEventListener("pointerdown", handlePointerDown);
      cancelScheduledClose();
    });
  });

  const openAgentChat = (agent: { projectId?: string; chatId: string }) => {
    requestAgentFocus(agent);
    close();
  };
  const openFinishedChat = (agent: FinishedAgent) => {
    // Opening the chat marks it reviewed; clearing here as well covers the
    // case where that chat is already on screen (no chat switch happens).
    dismissFinishedAgent(agent.runId);
    openAgentChat(agent);
  };
  const stopAgent = (agent: ActiveAgent, event: MouseEvent) => {
    event.stopPropagation();
    void cancelChatRun(agent.runId).catch(() => {
      // The run may have just finished; the event stream reconciles either way.
    });
  };

  return (
    <div
      ref={rootRef}
      class="agents-pill"
      onMouseEnter={open}
      onMouseLeave={scheduleClose}
    >
      <button
        classList={{
          "status-pill": true,
          "agents-pill__button": true,
          [`agents-pill__button--${tone()}`]: true,
        }}
        type="button"
        aria-expanded={isOpen()}
        aria-haspopup="true"
        aria-label="Agent activity"
        onClick={() => {
          if (isOpen() && isPinned()) {
            close();
          } else {
            open();
            setIsPinned(true);
          }
        }}
      >
        <span class="agents-pill__dot" aria-hidden="true" />
        <span>{label()}</span>
        <Show when={unseenCount() > 0}>
          <span class="agents-pill__unseen">({unseenCount()})</span>
        </Show>
      </button>

      <Show when={isOpen()}>
        <div
          class="agents-popover"
          role="region"
          aria-label="Agent activity"
          onMouseEnter={cancelScheduledClose}
          onMouseLeave={scheduleClose}
        >
          <Show
            when={runningCount() > 0 || unseenCount() > 0}
            fallback={
              <div class="agents-popover__empty">
                No agents running. Results you haven't opened yet will wait
                here.
              </div>
            }
          >
            <Show when={runningCount() > 0}>
              <div class="agents-popover__heading">
                Running
                <span class="agents-popover__heading-count">
                  {runningCount()}
                </span>
              </div>
              <For each={activeAgents()}>
                {(agent) => (
                  <div
                    classList={{
                      "agents-popover__row": true,
                      "agents-popover__row--attention": agent.awaitingApproval,
                    }}
                    role="button"
                    tabIndex={0}
                    title={`Open “${agent.chatTitle}”`}
                    onClick={() => openAgentChat(agent)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        openAgentChat(agent);
                      }
                    }}
                  >
                    <span
                      classList={{
                        "agents-popover__row-dot": true,
                        "agents-popover__row-dot--attention":
                          agent.awaitingApproval,
                      }}
                      aria-hidden="true"
                    />
                    <span class="agents-popover__row-body">
                      <span class="agents-popover__row-title">
                        {agent.chatTitle}
                      </span>
                      <span class="agents-popover__row-sub">
                        <Show when={agent.projectName ?? agent.projectId}>
                          {(project) => (
                            <span class="agents-popover__row-project">
                              {project()}
                            </span>
                          )}
                        </Show>
                        <span
                          classList={{
                            "agents-popover__row-status": true,
                            "agents-popover__row-status--attention":
                              agent.awaitingApproval,
                          }}
                        >
                          {agent.statusLabel}
                        </span>
                        <Show when={agent.statusDetail}>
                          <span
                            class="agents-popover__row-detail"
                            title={agent.statusDetail}
                          >
                            {agent.statusDetail}
                          </span>
                        </Show>
                      </span>
                    </span>
                    <span class="agents-popover__row-side">
                      <span class="agents-popover__row-time">
                        {formatElapsedMs(agent.startedAtMs, now())}
                      </span>
                      <button
                        class="agents-popover__stop"
                        type="button"
                        title="Stop this agent"
                        aria-label={`Stop agent in ${agent.chatTitle}`}
                        onClick={(event) => stopAgent(agent, event)}
                      >
                        <Square size={11} />
                      </button>
                    </span>
                  </div>
                )}
              </For>
            </Show>

            <Show when={unseenCount() > 0}>
              <div class="agents-popover__heading">
                Awaiting review
                <span class="agents-popover__heading-count agents-popover__heading-count--unseen">
                  {unseenCount()}
                </span>
                <button
                  class="agents-popover__clear"
                  type="button"
                  onClick={() => clearFinishedAgents()}
                >
                  Clear
                </button>
              </div>
              <For each={unseenFinishedAgents()}>
                {(agent) => (
                  <div
                    class="agents-popover__row"
                    role="button"
                    tabIndex={0}
                    title={`Open “${agent.chatTitle}”`}
                    onClick={() => openFinishedChat(agent)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        openFinishedChat(agent);
                      }
                    }}
                  >
                    <span
                      class={`agents-popover__row-dot agents-popover__row-dot--${agent.outcome}`}
                      aria-hidden="true"
                    />
                    <span class="agents-popover__row-body">
                      <span class="agents-popover__row-title">
                        {agent.chatTitle}
                      </span>
                      <span class="agents-popover__row-sub">
                        <Show when={agent.projectName ?? agent.projectId}>
                          {(project) => (
                            <span class="agents-popover__row-project">
                              {project()}
                            </span>
                          )}
                        </Show>
                        <span
                          class={`agents-popover__row-status agents-popover__row-status--${agent.outcome}`}
                        >
                          {OUTCOME_LABELS[agent.outcome]}
                        </span>
                      </span>
                    </span>
                    <span class="agents-popover__row-side">
                      <span class="agents-popover__row-time">
                        {formatAgoMs(agent.finishedAtMs, now())}
                      </span>
                    </span>
                  </div>
                )}
              </For>
            </Show>
          </Show>
        </div>
      </Show>
    </div>
  );
}
