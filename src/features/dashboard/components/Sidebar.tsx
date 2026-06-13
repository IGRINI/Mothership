import { createSignal, Match, Show, Switch } from "solid-js";
import { Dynamic } from "solid-js/web";
import { Circle, Folder, MessageSquare, Plus, type LucideProps } from "lucide-solid";
import type { Component } from "solid-js";

import type {
  ChatThreadSummary,
  ProjectSummary,
} from "../../../shared/api/mothership";
import {
  chatAwaitingApproval,
  chatHasRunningAgent,
  chatUnseenOutcome,
  projectAgentCounts,
} from "../../../shared/agentActivity";
import { openContextMenu } from "../../../shared/ui/ContextMenu";
import { VirtualList } from "../../../shared/ui/VirtualList";
import { startWindowDrag } from "../../../shared/window-drag";
import { formatRelativeTime } from "../time";
import { BrandMark } from "./BrandMark";
import {
  PROJECT_LUCIDE_ICONS,
  ProjectIconPicker,
  type ProjectAppearanceTarget,
} from "./ProjectIconPicker";
import { SectionHeader } from "./SectionHeader";

const LUCIDE_ICON_BY_ID: Record<string, Component<LucideProps>> =
  Object.fromEntries(PROJECT_LUCIDE_ICONS.map((entry) => [entry.id, entry.Icon]));

interface RenameState {
  kind: "chat" | "project";
  id: string;
}

export function Sidebar(props: {
  activeChatId?: string;
  activeProjectId?: string;
  chats: ChatThreadSummary[];
  isLoadingChats: boolean;
  isLoadingProjects: boolean;
  isOpeningProject: boolean;
  onNewChat: () => void;
  onOpenChat: (chatId: string) => void;
  onOpenProject: () => void;
  onSelectProject: (projectId: string) => void;
  onRenameChat: (chatId: string, title: string) => void;
  onDeleteChat: (chatId: string) => void;
  onRenameProject: (projectId: string, name: string) => void;
  onDeleteProject: (projectId: string) => void;
  onSetProjectAppearance: (
    projectId: string,
    icon: string | null,
    iconColor: string | null,
  ) => void;
  projects: ProjectSummary[];
}) {
  const [renaming, setRenaming] = createSignal<RenameState | null>(null);
  const [appearanceTarget, setAppearanceTarget] =
    createSignal<ProjectAppearanceTarget | null>(null);

  const isRenaming = (kind: RenameState["kind"], id: string) => {
    const current = renaming();
    return current?.kind === kind && current.id === id;
  };

  const commitRename = (
    kind: RenameState["kind"],
    id: string,
    value: string,
    original: string,
  ) => {
    setRenaming(null);
    const trimmed = value.trim();
    if (!trimmed || trimmed === original) {
      return;
    }
    if (kind === "chat") {
      props.onRenameChat(id, trimmed);
    } else {
      props.onRenameProject(id, trimmed);
    }
  };

  const openChatMenu = (event: MouseEvent, chat: ChatThreadSummary) => {
    event.preventDefault();
    const { clientX: x, clientY: y } = event;
    openContextMenu({
      x,
      y,
      items: [
        {
          label: "Rename",
          onSelect: () => setRenaming({ kind: "chat", id: chat.id }),
        },
        {
          label: "Delete chat…",
          danger: true,
          onSelect: () =>
            openContextMenu({
              x,
              y,
              items: [
                {
                  label: "Delete permanently",
                  danger: true,
                  onSelect: () => props.onDeleteChat(chat.id),
                },
                { label: "Cancel", onSelect: () => {} },
              ],
            }),
        },
      ],
    });
  };

  const openProjectMenu = (event: MouseEvent, project: ProjectSummary) => {
    event.preventDefault();
    const { clientX: x, clientY: y } = event;
    openContextMenu({
      x,
      y,
      items: [
        {
          label: "Rename",
          onSelect: () => setRenaming({ kind: "project", id: project.id }),
        },
        {
          label: "Change icon…",
          onSelect: () =>
            setAppearanceTarget({
              projectId: project.id,
              icon: project.icon,
              iconColor: project.iconColor,
              anchorX: x,
              anchorY: y,
            }),
        },
        {
          label: "Remove project…",
          danger: true,
          onSelect: () =>
            openContextMenu({
              x,
              y,
              items: [
                {
                  label: "Remove with all its chats",
                  danger: true,
                  onSelect: () => props.onDeleteProject(project.id),
                },
                { label: "Cancel", onSelect: () => {} },
              ],
            }),
        },
      ],
    });
  };

  return (
    <aside class="sidebar" aria-label="Workspace navigation">
      <div class="sidebar__brand" onMouseDown={startWindowDrag}>
        <BrandMark />
        <button
          class="new-chat-button"
          type="button"
          title="New chat (Ctrl+K)"
          disabled={!props.activeProjectId}
          onClick={props.onNewChat}
        >
          <Plus size={18} />
          <span>New Chat</span>
        </button>
      </div>

      <section class="sidebar-section sidebar-section--recent">
        <SectionHeader title="Recent" />
        <VirtualList
          ariaLabel="Recent chats"
          class="recent-list"
          empty={
            <div class="list-empty">
              {props.isLoadingChats
                ? "Loading chats..."
                : props.activeProjectId
                  ? "No chats yet"
                  : "Open a project first"}
            </div>
          }
          estimateSize={42}
          getItemKey={(chat) => chat.id}
          items={props.chats}
          overscan={8}
        >
          {(item) => (
            <RecentChatRow
              item={item}
              selected={item.id === props.activeChatId}
              renaming={isRenaming("chat", item.id)}
              onClick={() => props.onOpenChat(item.id)}
              onContextMenu={(event) => openChatMenu(event, item)}
              onCommitRename={(value) =>
                commitRename("chat", item.id, value, item.title)
              }
              onCancelRename={() => setRenaming(null)}
            />
          )}
        </VirtualList>
      </section>

      <section class="sidebar-section sidebar-section--projects">
        <SectionHeader
          title="Projects"
          action={
            <button
              class="icon-button icon-button--ghost"
              type="button"
              title="Open project"
              disabled={props.isOpeningProject}
              onClick={props.onOpenProject}
            >
              <Plus size={16} />
            </button>
          }
        />
        <VirtualList
          ariaLabel="Projects"
          class="project-list"
          empty={
            <div class="list-empty">
              {props.isLoadingProjects ? "Loading projects..." : "No projects"}
            </div>
          }
          estimateSize={64}
          getItemKey={(project) => project.id}
          items={props.projects}
          overscan={6}
        >
          {(project) => (
            <ProjectRow
              project={project}
              selected={project.id === props.activeProjectId}
              renaming={isRenaming("project", project.id)}
              onClick={() => props.onSelectProject(project.id)}
              onContextMenu={(event) => openProjectMenu(event, project)}
              onCommitRename={(value) =>
                commitRename("project", project.id, value, project.name)
              }
              onCancelRename={() => setRenaming(null)}
              onPickAppearance={(anchorX, anchorY) =>
                setAppearanceTarget({
                  projectId: project.id,
                  icon: project.icon,
                  iconColor: project.iconColor,
                  anchorX,
                  anchorY,
                })
              }
            />
          )}
        </VirtualList>
      </section>

      <ProjectIconPicker
        target={appearanceTarget()}
        onApply={(projectId, icon, iconColor) => {
          props.onSetProjectAppearance(projectId, icon, iconColor);
          // Keep the picker open so color + icon can be combined; the row
          // updates live behind it. Update the target so selection marks track.
          setAppearanceTarget((current) =>
            current && current.projectId === projectId
              ? { ...current, icon, iconColor }
              : current,
          );
        }}
        onClose={() => setAppearanceTarget(null)}
      />
    </aside>
  );
}

/** Autofocusing rename field shared by chat and project rows. */
function RenameInput(props: {
  class?: string;
  initial: string;
  onCommit: (value: string) => void;
  onCancel: () => void;
}) {
  return (
    <input
      ref={(element) => {
        queueMicrotask(() => {
          element.focus();
          element.select();
        });
      }}
      class={`rename-input ${props.class ?? ""}`}
      type="text"
      value={props.initial}
      maxLength={200}
      onClick={(event) => event.stopPropagation()}
      onPointerDown={(event) => event.stopPropagation()}
      onBlur={(event) => props.onCommit(event.currentTarget.value)}
      onKeyDown={(event) => {
        event.stopPropagation();
        if (event.key === "Enter") {
          event.preventDefault();
          props.onCommit(event.currentTarget.value);
        }
        if (event.key === "Escape") {
          event.preventDefault();
          props.onCancel();
        }
      }}
    />
  );
}

function RecentChatRow(props: {
  item: ChatThreadSummary;
  onClick: () => void;
  onContextMenu: (event: MouseEvent) => void;
  onCommitRename: (value: string) => void;
  onCancelRename: () => void;
  renaming: boolean;
  selected: boolean;
}) {
  return (
    <button
      classList={{
        "recent-row": true,
        "recent-row--selected": props.selected,
        "recent-row--attention": chatAwaitingApproval(props.item.id),
      }}
      type="button"
      onClick={props.onClick}
      onContextMenu={props.onContextMenu}
    >
      <RecentIcon chatId={props.item.id} selected={props.selected} />
      <Show
        when={props.renaming}
        fallback={<span>{props.item.title}</span>}
      >
        <RenameInput
          initial={props.item.title}
          onCommit={props.onCommitRename}
          onCancel={props.onCancelRename}
        />
      </Show>
      <time>{formatRelativeTime(props.item.updatedAt)}</time>
    </button>
  );
}

/** Leading slot of a chat row: an amber pulse when the agent is blocked on a
 * tool approval, a spinner while it works, an unread-result dot once it
 * finished (until the chat is opened), else a neutral chat glyph. One slot —
 * no layout shift between states. */
function RecentIcon(props: { chatId: string; selected: boolean }) {
  return (
    <Show
      when={!chatAwaitingApproval(props.chatId)}
      fallback={
        <span
          class="chat-indicator"
          title="Agent needs a tool approval"
          aria-label="Approval needed"
        >
          <span class="chat-attention-dot" />
        </span>
      }
    >
      <Show
        when={!chatHasRunningAgent(props.chatId)}
        fallback={
          <span
            class="chat-indicator"
            title="Agent is working in this chat"
            aria-label="Agent running"
          >
            <span class="agent-spinner" />
          </span>
        }
      >
        <Show
          when={chatUnseenOutcome(props.chatId)}
          fallback={
            <Show when={props.selected} fallback={<MessageSquare size={14} />}>
              <Circle size={14} />
            </Show>
          }
        >
          {(outcome) => (
            <span
              class="chat-indicator"
              title="Agent result awaiting review"
              aria-label="Result awaiting review"
            >
              <span class={`chat-unseen-dot chat-unseen-dot--${outcome()}`} />
            </span>
          )}
        </Show>
      </Show>
    </Show>
  );
}

function ProjectRow(props: {
  onClick: () => void;
  onContextMenu: (event: MouseEvent) => void;
  onCommitRename: (value: string) => void;
  onCancelRename: () => void;
  onPickAppearance: (anchorX: number, anchorY: number) => void;
  project: ProjectSummary;
  renaming: boolean;
  selected: boolean;
}) {
  const counts = () => projectAgentCounts(props.project.id);

  const openPicker = (event: MouseEvent) => {
    // The tile is its own control: don't let the click select the project.
    event.stopPropagation();
    const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
    props.onPickAppearance(rect.left, rect.bottom + 6);
  };

  return (
    <button
      classList={{
        "project-row": true,
        "project-row--selected": props.selected,
      }}
      type="button"
      onClick={props.onClick}
      onContextMenu={props.onContextMenu}
    >
      <span
        class="project-icon"
        role="button"
        tabIndex={0}
        title="Change icon and color"
        aria-label="Change project icon"
        style={projectIconStyle(props.project.iconColor)}
        onClick={openPicker}
        onPointerDown={(event) => event.stopPropagation()}
        onKeyDown={(event) => {
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            event.stopPropagation();
            const rect = (
              event.currentTarget as HTMLElement
            ).getBoundingClientRect();
            props.onPickAppearance(rect.left, rect.bottom + 6);
          }
        }}
      >
        <ProjectGlyph icon={props.project.icon} />
      </span>
      <span class="project-row__text">
        <Show
          when={props.renaming}
          fallback={<strong>{props.project.name}</strong>}
        >
          <RenameInput
            initial={props.project.name}
            onCommit={props.onCommitRename}
            onCancel={props.onCancelRename}
          />
        </Show>
        <small>{props.project.path}</small>
      </span>
      <span class="project-row__meta">
        <Show when={counts().attention > 0}>
          <span
            class="project-agents project-agents--attention"
            title={`${counts().attention} agent(s) need a tool approval`}
          >
            <span class="chat-attention-dot" />
            {counts().attention}
          </span>
        </Show>
        <Show when={counts().running > 0}>
          <span
            class="project-agents project-agents--running"
            title={`${counts().running} agent(s) running`}
          >
            <span class="agent-spinner" />
            {counts().running}
          </span>
        </Show>
        <Show when={counts().unseen > 0}>
          <span
            class="project-agents project-agents--unseen"
            title={`${counts().unseen} result(s) awaiting review`}
          >
            {counts().unseen}
          </span>
        </Show>
        <span class="project-row__chat-count">{props.project.chatCount}</span>
      </span>
    </button>
  );
}

/** The project's tile glyph: a picked emoji, a picked lucide icon, or the
 * default folder. Unknown stored ids degrade to the folder. */
function ProjectGlyph(props: { icon?: string | null }) {
  const emoji = () => {
    const raw = props.icon?.trim();
    return raw?.startsWith("emoji:")
      ? raw.slice("emoji:".length) || undefined
      : undefined;
  };
  const lucide = () => {
    const raw = props.icon?.trim();
    return raw?.startsWith("lucide:")
      ? LUCIDE_ICON_BY_ID[raw.slice("lucide:".length)]
      : undefined;
  };

  return (
    <Switch fallback={<Folder size={18} />}>
      <Match when={emoji()}>
        {(value) => <span class="project-icon__emoji">{value()}</span>}
      </Match>
      <Match when={lucide()}>
        {(Icon) => <Dynamic component={Icon()} size={18} />}
      </Match>
    </Switch>
  );
}

function projectIconStyle(color: string | null | undefined) {
  const hex = color?.trim();
  if (!hex || !/^#[0-9a-fA-F]{6}$/.test(hex)) {
    return undefined;
  }
  // Tile tint from the user's accent: ~12% fill, ~38% border, full-color glyph.
  return {
    background: `${hex}1f`,
    "border-color": `${hex}61`,
    color: hex,
  };
}
