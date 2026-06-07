// Client-local session restore: which project was open and which chat within
// each project, so relaunching reopens where you left off. This is per-client UI
// state (localStorage) — never backend, so a phone and a desktop can view
// different projects/chats independently.

const STORAGE_KEY = "mothership.session.v1";

interface SessionState {
  projectId?: string;
  /** Last-active chat id per project. */
  chatByProject: Record<string, string>;
}

function load(): SessionState {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return { chatByProject: {} };
    }
    const parsed = JSON.parse(raw) as Partial<SessionState>;
    return {
      projectId:
        typeof parsed.projectId === "string" ? parsed.projectId : undefined,
      chatByProject:
        parsed.chatByProject && typeof parsed.chatByProject === "object"
          ? (parsed.chatByProject as Record<string, string>)
          : {},
    };
  } catch {
    return { chatByProject: {} };
  }
}

function save(state: SessionState) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    // Best effort — losing session restore must never break the app.
  }
}

export function lastProjectId(): string | undefined {
  return load().projectId;
}

export function lastChatId(projectId: string): string | undefined {
  return load().chatByProject[projectId];
}

/** Remember the active project (called when the client switches projects). */
export function rememberProject(projectId: string): void {
  const state = load();
  state.projectId = projectId;
  save(state);
}

/** Remember the active chat for a project (and mark the project active). */
export function rememberChat(projectId: string, chatId: string | undefined): void {
  const state = load();
  state.projectId = projectId;
  if (chatId) {
    state.chatByProject[projectId] = chatId;
  } else {
    delete state.chatByProject[projectId];
  }
  save(state);
}
