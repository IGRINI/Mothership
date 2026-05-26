import { createSignal, JSX, onCleanup, onMount, Show } from "solid-js";
import {
  ChevronDown,
  Circle,
  Clock,
  Copy,
  FileText,
  Folder,
  GitBranch,
  MoreVertical,
  Paperclip,
  Plus,
  Search,
  Send,
  Terminal,
  ThumbsDown,
  ThumbsUp,
} from "lucide-solid";
import { listen } from "@tauri-apps/api/event";

import {
  ChatRunEvent,
  ChatMessage,
  ChatThreadSummary,
  ConnectorSettingsSnapshot,
  createChat,
  getChat,
  getConnectorSettings,
  listChats,
  sendChatMessage,
  setSelectedModel,
} from "../../shared/api/mothership";
import { VirtualList } from "../../shared/ui/VirtualList";
import mothershipLogoUrl from "../../assets/mothership-logo.png";

const projects: ProjectItem[] = [
  {
    id: "mothership",
    name: "Mothership",
    path: "E:/Mothership",
    branch: "main",
    tone: "green",
  },
  {
    id: "research",
    name: "Research",
    path: "projects-to-research",
    branch: "local",
    tone: "yellow",
  },
  {
    id: "sidecar",
    name: "Sidecar",
    path: "src-sidecar",
    branch: "core",
    tone: "blue",
  },
];

export function Dashboard(props: { onOpenSettings?: () => void }) {
  const [chats, setChats] = createSignal<ChatThreadSummary[]>([]);
  const [messages, setMessages] = createSignal<ChatMessage[]>([]);
  const [connectorSettings, setConnectorSettings] =
    createSignal<ConnectorSettingsSnapshot>();
  const [activeChatId, setActiveChatId] = createSignal<string>();
  const [draft, setDraft] = createSignal("");
  const [error, setError] = createSignal("");
  const [runTransports, setRunTransports] = createSignal<Record<string, string>>(
    {},
  );
  const [isLoadingChats, setIsLoadingChats] = createSignal(true);
  const [isLoadingMessages, setIsLoadingMessages] = createSignal(false);
  const [isSending, setIsSending] = createSignal(false);
  let unlistenChatRun: (() => void) | undefined;

  const activeChat = () =>
    chats().find((chat) => chat.id === activeChatId()) ?? null;
  const isChatRunning = () =>
    messages().some(
      (message) => message.role === "assistant" && message.status === "sending",
    );

  onMount(() => {
    void loadChats();
    void loadConnectorSettings();

    if (isTauriRuntime()) {
      let disposed = false;
      void listen<ChatRunEvent>("chat-run-event", (event) => {
        applyChatRunEvent(event.payload);
      }).then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          unlistenChatRun = unlisten;
        }
      });

      onCleanup(() => {
        disposed = true;
      });
    }
  });

  onCleanup(() => {
    unlistenChatRun?.();
  });

  async function loadChats() {
    setIsLoadingChats(true);
    setError("");

    try {
      const loadedChats = await listChats(100);
      setChats(loadedChats);

      if (loadedChats[0]) {
        await openChat(loadedChats[0].id);
      }
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    } finally {
      setIsLoadingChats(false);
    }
  }

  async function openChat(chatId: string) {
    setActiveChatId(chatId);
    setIsLoadingMessages(true);
    setError("");

    try {
      const conversation = await getChat(chatId, 200);
      setChats((current) => mergeChatInPlace(current, conversation.chat));
      setMessages(normalizeMessages(conversation.messages));
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    } finally {
      setIsLoadingMessages(false);
    }
  }

  async function handleNewChat() {
    setError("");

    try {
      const conversation = await createChat();
      setChats((current) => bumpChat(current, conversation.chat));
      setActiveChatId(conversation.chat.id);
      setMessages(normalizeMessages(conversation.messages));
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function handleSendMessage() {
    const content = draft().trim();
    if (!content || isSending() || isChatRunning()) {
      return;
    }

    const currentChatId = activeChatId();
    setDraft("");
    setIsSending(true);
    setError("");

    try {
      const result = await sendChatMessage(currentChatId, content);

      setChats((current) => bumpChat(current, result.chat));
      setActiveChatId(result.chat.id);
      setMessages((current) => {
        const base = currentChatId === result.chat.id ? current : [];
        return mergeMessages(base, [result.userMessage, result.assistantMessage]);
      });
    } catch (caughtError) {
      setDraft(content);
      setError(errorMessage(caughtError));
    } finally {
      setIsSending(false);
    }
  }

  async function loadConnectorSettings() {
    try {
      setConnectorSettings(await getConnectorSettings());
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function handleSelectModel(providerId: string, modelId: string) {
    try {
      setConnectorSettings(await setSelectedModel(providerId, modelId));
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  function applyChatRunEvent(event: ChatRunEvent) {
    if (event.chat) {
      setChats((current) => bumpChat(current, event.chat!));
    }

    if (event.transport) {
      setRunTransports((current) => ({
        ...current,
        [event.messageId]: event.transport!,
      }));
    }

    if (event.kind === "completed" || event.kind === "failed") {
      setRunTransports((current) => {
        const next = { ...current };
        delete next[event.messageId];
        return next;
      });
    }

    if (event.chatId !== activeChatId()) {
      return;
    }

    if (event.message) {
      setMessages((current) => mergeMessages(current, [event.message!]));
    }

    if (event.kind === "failed" && event.error) {
      setError(event.error);
    }
  }

  return (
    <main class="workspace-shell">
      <Sidebar
        activeChatId={activeChatId()}
        chats={chats()}
        isLoading={isLoadingChats()}
        onNewChat={handleNewChat}
        onOpenChat={(chatId) => void openChat(chatId)}
        onOpenSettings={props.onOpenSettings}
      />
      <ConversationPane
        activeChat={activeChat()}
        connectorSettings={connectorSettings()}
        draft={draft()}
        error={error()}
        isLoading={isLoadingMessages()}
        isSending={isSending() || isChatRunning()}
        messages={messages()}
        runTransports={runTransports()}
        onDraftChange={setDraft}
        onSelectModel={handleSelectModel}
        onSendMessage={handleSendMessage}
      />
      <InspectorPane
        activeChat={activeChat()}
        messageCount={messages().length}
      />
    </main>
  );
}

function Sidebar(props: {
  activeChatId?: string;
  chats: ChatThreadSummary[];
  isLoading: boolean;
  onNewChat: () => void;
  onOpenChat: (chatId: string) => void;
  onOpenSettings?: () => void;
}) {
  return (
    <aside class="sidebar" aria-label="Workspace navigation">
      <div class="sidebar__brand" data-tauri-drag-region>
        <BrandMark />
        <strong>Mothership</strong>
        <button class="icon-button" type="button" title="New workspace">
          <Plus size={16} />
        </button>
      </div>

      <button class="new-chat-button" type="button" onClick={props.onNewChat}>
        <Plus size={18} />
        <span>New Chat</span>
        <kbd>Ctrl+K</kbd>
      </button>

      <section class="sidebar-section sidebar-section--recent">
        <SectionHeader title="Recent" action={<Search size={16} />} />
        <VirtualList
          ariaLabel="Recent chats"
          class="recent-list"
          empty={
            <div class="list-empty">
              {props.isLoading ? "Loading chats..." : "No chats yet"}
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
        <button class="text-button" type="button" onClick={props.onNewChat}>
          Start chat
          <ChevronDown size={14} />
        </button>
      </section>

      <section class="sidebar-section sidebar-section--projects">
        <SectionHeader title="Projects" action={<Plus size={16} />} />
        <VirtualList
          ariaLabel="Projects"
          class="project-list"
          estimateSize={64}
          getItemKey={(project) => project.id}
          items={projects}
          overscan={6}
        >
          {(project) => <ProjectRow project={project} />}
        </VirtualList>
        <button class="text-button text-button--wide" type="button">
          View all projects
          <ChevronDown size={14} />
        </button>
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

function ConversationPane(props: {
  activeChat: ChatThreadSummary | null;
  connectorSettings?: ConnectorSettingsSnapshot;
  draft: string;
  error: string;
  isLoading: boolean;
  isSending: boolean;
  messages: ChatMessage[];
  runTransports: Record<string, string>;
  onDraftChange: (value: string) => void;
  onSelectModel: (providerId: string, modelId: string) => void;
  onSendMessage: () => void;
}) {
  return (
    <section class="conversation-pane" aria-label="Active chat">
      <header class="conversation-header">
        <div class="conversation-header__title">
          <h1>{props.activeChat?.title ?? "New chat"}</h1>
          <button
            class="icon-button icon-button--ghost"
            type="button"
            title="Rename chat"
          >
            <FileText size={15} />
          </button>
          <span class="project-badge">Mothership</span>
        </div>

        <div class="agent-status-chip" title="Local agent connection">
          <Terminal size={16} />
          <ModelSelector
            settings={props.connectorSettings}
            onSelectModel={props.onSelectModel}
          />
          <Circle size={8} />
          <strong>Storage</strong>
          <ChevronDown size={14} />
        </div>
      </header>

      <VirtualList
        ariaLabel="Chat messages"
        class="message-list"
        empty={
          <ConversationState error={props.error} isLoading={props.isLoading} />
        }
        estimateSize={(message) =>
          Math.max(120, Math.min(420, 92 + message.content.length * 0.4))
        }
        getItemKey={(message) => message.id}
        items={props.messages}
        overscan={4}
        stickToEnd
      >
        {(message) => (
          <MessageRow
            message={message}
            transport={props.runTransports[message.id]}
          />
        )}
      </VirtualList>

      <Show when={props.error}>
        <div class="chat-error" role="alert">
          {props.error}
        </div>
      </Show>

      <Composer
        draft={props.draft}
        isSending={props.isSending}
        onDraftChange={props.onDraftChange}
        onSend={props.onSendMessage}
      />
    </section>
  );
}

function ConversationState(props: { error: string; isLoading: boolean }) {
  if (props.error) {
    return (
      <div class="conversation-state conversation-state--error">
        {props.error}
      </div>
    );
  }

  if (props.isLoading) {
    return <div class="conversation-state">Loading messages...</div>;
  }

  return (
    <div class="conversation-state">
      <BrandMark compact />
      <strong>Start a provider chat</strong>
      <span>Select a connected model before sending a message.</span>
    </div>
  );
}

function ModelSelector(props: {
  onSelectModel: (providerId: string, modelId: string) => void;
  settings?: ConnectorSettingsSnapshot;
}) {
  const models = () =>
    props.settings?.providers.flatMap((provider) => provider.models) ?? [];
  const selectedValue = () => {
    const selected = props.settings?.selectedModel;
    return selected ? `${selected.providerId}/${selected.modelId}` : "";
  };

  return (
    <select
      class="model-select"
      aria-label="Active model"
      value={selectedValue()}
      onChange={(event) => {
        const [providerId, modelId] = event.currentTarget.value.split("/");
        if (providerId && modelId) {
          props.onSelectModel(providerId, modelId);
        }
      }}
    >
      <Show
        when={models().length > 0}
        fallback={<option value="">No models connected</option>}
      >
        {models().map((model) => (
          <option value={`${model.providerId}/${model.id}`}>
            {model.providerLabel} / {model.label}
          </option>
        ))}
      </Show>
    </select>
  );
}

function InspectorPane(props: {
  activeChat: ChatThreadSummary | null;
  messageCount: number;
}) {
  return (
    <aside class="inspector-pane" aria-label="Run inspector">
      <div class="inspector-tabs" role="tablist" aria-label="Inspector tabs">
        <button class="inspector-tab inspector-tab--active" type="button">
          Inspector
        </button>
        <button class="inspector-tab" type="button">
          Context
        </button>
      </div>

      <div class="inspector-scroll">
        <InspectorSection
          title="Current Run"
          action={
            <span class="live-pill">
              <Circle size={8} />
              Idle
            </span>
          }
        >
          <div class="run-summary">
            <div class="run-summary__title">
              <Terminal size={16} />
              <strong>No active run</strong>
            </div>
            <span>Chat persistence is enabled</span>
            <div class="progress-track">
              <div class="progress-track__fill" style={{ width: "0%" }} />
            </div>
            <div class="run-summary__footer">
              <span>{props.activeChat?.title ?? "No chat selected"}</span>
              <span>{props.messageCount} messages</span>
            </div>
          </div>
        </InspectorSection>

        <InspectorSection
          title="Tool Calls"
          action={
            <button class="count-button" type="button">
              0 <ChevronDown size={13} />
            </button>
          }
        >
          <div class="panel-empty">Tool calls will appear here.</div>
        </InspectorSection>

        <InspectorSection
          title="Artifacts"
          action={
            <button class="count-button" type="button">
              0 <ChevronDown size={13} />
            </button>
          }
        >
          <div class="panel-empty">No artifacts in this chat.</div>
        </InspectorSection>

        <InspectorSection title="Notes">
          <input class="note-input" placeholder="Add a note..." />
        </InspectorSection>
      </div>
    </aside>
  );
}

function MessageRow(props: { message: ChatMessage; transport?: string }) {
  const message = () => props.message;

  return (
    <article
      classList={{
        "message-row": true,
        "message-row--assistant": message().role === "assistant",
      }}
    >
      <Avatar role={message().role} />
      <div class="message-row__content">
        <div class="message-meta">
          <strong>{message().role === "user" ? "You" : "Mothership"}</strong>
          <span>{formatMessageTime(message().createdAt)}</span>
        </div>

        <p>
          <Show
            when={message().content}
            fallback={
              message().status === "sending"
                ? thinkingLabel(props.transport)
                : "No content."
            }
          >
            {message().content}
          </Show>
        </p>

        <Show when={message().role === "assistant"}>
          <div class="message-actions">
            <button
              class="icon-button icon-button--ghost"
              type="button"
              title="Like"
            >
              <ThumbsUp size={15} />
            </button>
            <button
              class="icon-button icon-button--ghost"
              type="button"
              title="Dislike"
            >
              <ThumbsDown size={15} />
            </button>
            <button
              class="icon-button icon-button--ghost"
              type="button"
              title="Copy"
              onClick={() =>
                void navigator.clipboard?.writeText(message().content)
              }
            >
              <Copy size={15} />
            </button>
          </div>
        </Show>
      </div>
    </article>
  );
}

function Composer(props: {
  draft: string;
  isSending: boolean;
  onDraftChange: (value: string) => void;
  onSend: () => void;
}) {
  const canSend = () => props.draft.trim().length > 0 && !props.isSending;

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
        placeholder="Ask Mothership anything..."
        value={props.draft}
        onInput={(event) => props.onDraftChange(event.currentTarget.value)}
        onKeyDown={(event) => {
          if ((event.ctrlKey || event.metaKey) && event.key === "Enter") {
            event.preventDefault();
            props.onSend();
          }
        }}
      />
      <div class="composer__actions">
        <button class="icon-button" type="button" title="Attach file">
          <Paperclip size={17} />
        </button>
        <span class="shortcut-pill">Ctrl+Enter</span>
        <button
          class="send-button"
          disabled={!canSend()}
          type="submit"
          title="Send message"
        >
          <Send size={18} />
        </button>
      </div>
    </form>
  );
}

function SectionHeader(props: { action?: JSX.Element; title: string }) {
  return (
    <div class="section-header">
      <span>{props.title}</span>
      {props.action && (
        <button class="icon-button icon-button--ghost" type="button">
          {props.action}
        </button>
      )}
    </div>
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
      <RecentIcon status={props.selected ? "active" : "clock"} />
      <span>{props.item.title}</span>
      <time>{formatRelativeTime(props.item.updatedAt)}</time>
    </button>
  );
}

function RecentIcon(props: { status: RecentStatus }) {
  if (props.status === "branch") {
    return <GitBranch size={14} />;
  }

  if (props.status === "clock") {
    return <Clock size={14} />;
  }

  return <Circle size={14} />;
}

function ProjectRow(props: { project: ProjectItem }) {
  return (
    <button class="project-row" type="button">
      <span class="project-icon">
        <Folder size={18} />
      </span>
      <span class="project-row__text">
        <strong>{props.project.name}</strong>
        <small>{props.project.path}</small>
      </span>
      <span class={`branch-dot branch-dot--${props.project.tone}`} />
      <span>{props.project.branch}</span>
      <ChevronDown size={14} />
    </button>
  );
}

function Avatar(props: { role: "assistant" | "user" }) {
  if (props.role === "user") {
    return <div class="avatar avatar--message avatar--user">You</div>;
  }

  return (
    <div class="avatar avatar--message avatar--agent">
      <BrandMark compact />
    </div>
  );
}

function BrandMark(props: { compact?: boolean }) {
  return (
    <span
      classList={{
        "brand-mark": true,
        "brand-mark--compact": Boolean(props.compact),
      }}
      aria-hidden="true"
    >
      <img src={mothershipLogoUrl} alt="" draggable={false} />
    </span>
  );
}

function mergeChatInPlace(
  current: ChatThreadSummary[],
  chat: ChatThreadSummary,
): ChatThreadSummary[] {
  const existingIndex = current.findIndex((item) => item.id === chat.id);
  if (existingIndex === -1) {
    return [...current, chat];
  }

  const next = [...current];
  next[existingIndex] = chat;
  return next;
}

function bumpChat(
  current: ChatThreadSummary[],
  chat: ChatThreadSummary,
): ChatThreadSummary[] {
  return [chat, ...current.filter((item) => item.id !== chat.id)];
}

function mergeMessages(
  current: ChatMessage[],
  incoming: ChatMessage[],
): ChatMessage[] {
  const messagesById = new Map<string, ChatMessage>();
  for (const message of current) {
    messagesById.set(message.id, message);
  }
  for (const message of incoming) {
    messagesById.set(message.id, message);
  }

  return Array.from(messagesById.values()).sort(compareMessages);
}

function normalizeMessages(messages: ChatMessage[]) {
  return mergeMessages([], messages);
}

function compareMessages(left: ChatMessage, right: ChatMessage) {
  if (left.position !== right.position) {
    return left.position - right.position;
  }

  const leftTime = Number(left.createdAt);
  const rightTime = Number(right.createdAt);
  if (Number.isFinite(leftTime) && Number.isFinite(rightTime) && leftTime !== rightTime) {
    return leftTime - rightTime;
  }

  const roleOrder = (message: ChatMessage) => (message.role === "user" ? 0 : 1);
  const roleDifference = roleOrder(left) - roleOrder(right);
  if (roleDifference !== 0) {
    return roleDifference;
  }

  return left.id.localeCompare(right.id);
}

function formatMessageTime(timestamp: string) {
  const date = unixTimestampToDate(timestamp);
  return date.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

function thinkingLabel(transport?: string) {
  if (!transport) {
    return "Thinking...";
  }

  const label: Record<string, string> = {
    http_json: "HTTP JSON",
    http_sse: "HTTP streaming",
    websocket: "WebSocket",
  };

  return `Thinking via ${label[transport] ?? transport}...`;
}

function formatRelativeTime(timestamp: string) {
  const seconds = Math.max(
    0,
    Math.floor((Date.now() - unixTimestampToDate(timestamp).getTime()) / 1000),
  );

  if (seconds < 60) {
    return "now";
  }

  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m ago`;
  }

  if (seconds < 86_400) {
    return `${Math.floor(seconds / 3600)}h ago`;
  }

  return unixTimestampToDate(timestamp).toLocaleDateString([], {
    month: "short",
    day: "numeric",
  });
}

function unixTimestampToDate(timestamp: string) {
  const numericTimestamp = Number(timestamp);
  return new Date(
    Number.isFinite(numericTimestamp) ? numericTimestamp * 1000 : Date.now(),
  );
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

type RecentStatus = "active" | "branch" | "clock";

interface ProjectItem {
  branch: string;
  id: string;
  name: string;
  path: string;
  tone: "blue" | "green" | "yellow";
}
