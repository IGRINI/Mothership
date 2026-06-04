import { Clock, Circle, Folder, MoreVertical, Plus } from "lucide-solid";

import type {
  ChatThreadSummary,
  ProjectSummary,
} from "../../../shared/api/mothership";
import { VirtualList } from "../../../shared/ui/VirtualList";
import { startWindowDrag } from "../../../shared/window-drag";
import { formatRelativeTime } from "../time";
import { BrandMark } from "./BrandMark";
import { SectionHeader } from "./SectionHeader";

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
  onOpenSettings?: () => void;
  onSelectProject: (projectId: string) => void;
  projects: ProjectSummary[];
}) {
  return (
    <aside class="sidebar" aria-label="Workspace navigation">
      <div class="sidebar__brand" onMouseDown={startWindowDrag}>
        <BrandMark />
        <strong>Mothership</strong>
        <button
          class="icon-button"
          type="button"
          title="Open project"
          disabled={props.isOpeningProject}
          onClick={props.onOpenProject}
        >
          <Folder size={16} />
        </button>
      </div>

      <button
        class="new-chat-button"
        type="button"
        disabled={!props.activeProjectId}
        onClick={props.onNewChat}
      >
        <Plus size={18} />
        <span>New Chat</span>
        <kbd>Ctrl+K</kbd>
      </button>

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
              onClick={() => props.onOpenChat(item.id)}
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
        <button
          class="project-open-button"
          type="button"
          disabled={props.isOpeningProject}
          onClick={props.onOpenProject}
        >
          <Folder size={16} />
          <span>{props.isOpeningProject ? "Opening..." : "Open folder"}</span>
        </button>
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
              onClick={() => props.onSelectProject(project.id)}
            />
          )}
        </VirtualList>
      </section>

      <div class="account-card">
        <div class="avatar avatar--user">MS</div>
        <div>
          <strong>Local Profile</strong>
          <span>Desktop Core</span>
        </div>
        <button
          class="icon-button icon-button--ghost"
          type="button"
          title="Settings"
          onClick={props.onOpenSettings}
        >
          <MoreVertical size={16} />
        </button>
      </div>
    </aside>
  );
}

function RecentChatRow(props: {
  item: ChatThreadSummary;
  onClick: () => void;
  selected: boolean;
}) {
  return (
    <button
      classList={{
        "recent-row": true,
        "recent-row--selected": props.selected,
      }}
      type="button"
      onClick={props.onClick}
    >
      <RecentIcon selected={props.selected} />
      <span>{props.item.title}</span>
      <time>{formatRelativeTime(props.item.updatedAt)}</time>
    </button>
  );
}

function RecentIcon(props: { selected: boolean }) {
  if (!props.selected) {
    return <Clock size={14} />;
  }

  return <Circle size={14} />;
}

function ProjectRow(props: {
  onClick: () => void;
  project: ProjectSummary;
  selected: boolean;
}) {
  return (
    <button
      classList={{
        "project-row": true,
        "project-row--selected": props.selected,
      }}
      type="button"
      onClick={props.onClick}
    >
      <span class="project-icon">
        <Folder size={18} />
      </span>
      <span class="project-row__text">
        <strong>{props.project.name}</strong>
        <small>{props.project.path}</small>
      </span>
      <span>{props.project.chatCount}</span>
    </button>
  );
}
