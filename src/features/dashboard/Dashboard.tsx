import { createSignal, JSX, onCleanup, onMount, Show } from "solid-js";
import {
  AlertTriangle,
  Check,
  ChevronDown,
  Circle,
  Clock,
  FileText,
  Folder,
  GitBranch,
  MoreVertical,
  Paperclip,
  Pencil,
  Plus,
  RefreshCw,
  Search,
  Send,
  Square,
  Terminal,
  X,
} from "lucide-solid";
import { listen } from "@tauri-apps/api/event";

import {
  ChatRunEvent,
  ChatMessage,
  ChatThreadSummary,
  ConnectorSettingsEvent,
  ConnectorSettingsSnapshot,
  ToolCommand,
  ToolExecutionEvent,
  ToolExecutionEventKind,
  ToolExecutionRecord,
  ToolExecutionResult,
  approveToolExecution,
  branchChatFromMessage,
  cancelChatRun,
  cancelToolExecution,
  createChat,
  editChatUserMessage,
  getChat,
  getConnectorSettings,
  listChats,
  retryChatMessage,
  sendChatMessage,
  setSelectedModel,
} from "../../shared/api/mothership";
import { VirtualList } from "../../shared/ui/VirtualList";
import { SolidMarkdown } from "solid-markdown";
import remarkGfm from "remark-gfm";
import mothershipLogoUrl from "../../assets/mothership-logo-sm.png";

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
  const [editingMessageId, setEditingMessageId] = createSignal<string>();
  const [editingDraft, setEditingDraft] = createSignal("");
  const [isSubmittingEdit, setIsSubmittingEdit] = createSignal(false);
  const [branchingChatId, setBranchingChatId] = createSignal<string>();
  const [activeRunIds, setActiveRunIds] = createSignal<Record<string, string>>(
    {},
  );
  const [runMessageIds, setRunMessageIds] = createSignal<Record<string, string>>(
    {},
  );
  const [expandedInlineTools, setExpandedInlineTools] = createSignal<
    Record<string, boolean>
  >({});
  const [toolExecutions, setToolExecutions] = createSignal<
    Record<string, ToolExecutionView>
  >({});
  let unlistenChatRun: (() => void) | undefined;
  let unlistenConnectorSettings: (() => void) | undefined;
  let unlistenToolExecution: (() => void) | undefined;

  const activeChat = () =>
    chats().find((chat) => chat.id === activeChatId()) ?? null;
  const activeRunId = () => {
    const chatId = activeChatId();
    return chatId ? activeRunIds()[chatId] : undefined;
  };
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

      void listen<ConnectorSettingsEvent>("connector-settings-event", (event) => {
        setConnectorSettings(event.payload.snapshot);
      }).then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          unlistenConnectorSettings = unlisten;
        }
      });

      void listen<ToolExecutionEvent>("tool-execution-event", (event) => {
        applyToolExecutionEvent(event.payload);
      }).then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          unlistenToolExecution = unlisten;
        }
      });

      onCleanup(() => {
        disposed = true;
      });
    }
  });

  onCleanup(() => {
    unlistenChatRun?.();
    unlistenConnectorSettings?.();
    unlistenToolExecution?.();
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
      hydrateToolExecutions(conversation.toolExecutions ?? []);
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
    if (!content || isSending() || isSubmittingEdit() || isChatRunning()) {
      return;
    }

    const currentChatId = activeChatId();
    setDraft("");
    setIsSending(true);
    setError("");

    try {
      const result = await sendChatMessage(currentChatId, content);

      setActiveRunIds((current) => ({
        ...current,
        [result.chat.id]: result.runId,
      }));
      rememberRunMessage(result.runId, result.assistantMessage.id);
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

  function handleStartEdit(message: ChatMessage) {
    if (message.role !== "user" || message.status !== "complete" || isChatRunning()) {
      return;
    }

    setEditingMessageId(message.id);
    setEditingDraft(message.content);
    setError("");
  }

  function handleCancelEdit() {
    setEditingMessageId(undefined);
    setEditingDraft("");
  }

  async function handleSubmitEdit(messageId: string) {
    const chatId = activeChatId();
    const content = editingDraft().trim();
    const targetMessage = messages().find((message) => message.id === messageId);
    if (
      !chatId ||
      !targetMessage ||
      !content ||
      isSubmittingEdit() ||
      isSending() ||
      isChatRunning()
    ) {
      return;
    }

    const removedMessageIds = messages()
      .filter((message) => message.position > targetMessage.position)
      .map((message) => message.id);

    setIsSubmittingEdit(true);
    setError("");

    try {
      const result = await editChatUserMessage(chatId, messageId, content);
      setActiveRunIds((current) => ({
        ...current,
        [result.chat.id]: result.runId,
      }));
      clearToolExecutionsForMessageIds(removedMessageIds);
      rememberRunMessage(result.runId, result.assistantMessage.id);
      setChats((current) => bumpChat(current, result.chat));
      setMessages((current) =>
        mergeMessages(
          current.filter(
            (message) =>
              message.chatId === result.chat.id &&
              message.position < result.userMessage.position,
          ),
          [result.userMessage, result.assistantMessage],
        ),
      );
      setEditingMessageId(undefined);
      setEditingDraft("");
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    } finally {
      setIsSubmittingEdit(false);
    }
  }

  async function handleBranchMessage(message: ChatMessage) {
    const sourceChatId = activeChatId();
    if (
      !sourceChatId ||
      message.role !== "assistant" ||
      message.status === "sending" ||
      isChatRunning() ||
      branchingChatId()
    ) {
      return;
    }

    const sourceMessages = messages();
    const sourceChat = activeChat();
    const tempChatId = `branching-${message.id}`;
    const now = currentUnixTimestamp();
    const tempChat: ChatThreadSummary = {
      id: tempChatId,
      title: sourceChat ? `${sourceChat.title} branch` : "Creating branch",
      preview: "Copying chat history...",
      messageCount: 0,
      createdAt: now,
      updatedAt: now,
    };

    setBranchingChatId(tempChatId);
    setEditingMessageId(undefined);
    setEditingDraft("");
    setError("");
    setChats((current) => bumpChat(current, tempChat));
    setActiveChatId(tempChatId);
    setMessages([]);
    setIsLoadingMessages(true);

    try {
      const conversation = await branchChatFromMessage(sourceChatId, message.id);
      setChats((current) =>
        bumpChat(
          current.filter((chat) => chat.id !== tempChatId),
          conversation.chat,
        ),
      );
      setActiveChatId(conversation.chat.id);
      setMessages(normalizeMessages(conversation.messages));
      hydrateToolExecutions(conversation.toolExecutions ?? []);
    } catch (caughtError) {
      setChats((current) => current.filter((chat) => chat.id !== tempChatId));
      setActiveChatId(sourceChatId);
      setMessages(sourceMessages);
      setError(errorMessage(caughtError));
    } finally {
      setBranchingChatId(undefined);
      setIsLoadingMessages(false);
    }
  }

  async function handleRetry() {
    const chatId = activeChatId();
    if (!chatId || isChatRunning()) {
      return;
    }
    setError("");

    try {
      const result = await retryChatMessage(chatId);
      setActiveRunIds((current) => ({
        ...current,
        [result.chat.id]: result.runId,
      }));
      clearMessageToolExecutions(result.assistantMessage.id);
      rememberRunMessage(result.runId, result.assistantMessage.id);
      setChats((current) => bumpChat(current, result.chat));
      setMessages((current) =>
        mergeMessages(current, [result.userMessage, result.assistantMessage]),
      );
    } catch (caughtError) {
      setError(errorMessage(caughtError));
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

  async function handleCancelRun() {
    const runId = activeRunId();
    if (!runId) {
      return;
    }
    setError("");

    try {
      await cancelChatRun(runId);
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function handleApproveTool(toolCallId: string) {
    setError("");
    try {
      await approveToolExecution(toolCallId, true);
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function handleDenyTool(toolCallId: string) {
    setError("");
    try {
      await approveToolExecution(toolCallId, false, "Denied by user");
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function handleCancelTool(toolCallId: string) {
    setError("");
    try {
      await cancelToolExecution(toolCallId);
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  function applyChatRunEvent(event: ChatRunEvent) {
    rememberRunMessage(event.runId, event.messageId);

    if (event.kind === "started") {
      setActiveRunIds((current) => ({
        ...current,
        [event.chatId]: event.runId,
      }));
    }

    if (event.chat) {
      setChats((current) => bumpChat(current, event.chat!));
    }

    if (event.transport) {
      setRunTransports((current) => ({
        ...current,
        [event.messageId]: event.transport!,
      }));
    }

    if (
      event.kind === "completed" ||
      event.kind === "failed" ||
      event.kind === "cancelled"
    ) {
      setRunTransports((current) => {
        const next = { ...current };
        delete next[event.messageId];
        return next;
      });
      setActiveRunIds((current) => {
        if (current[event.chatId] !== event.runId) {
          return current;
        }
        const next = { ...current };
        delete next[event.chatId];
        return next;
      });
    }

    if (event.chatId !== activeChatId()) {
      return;
    }

    if (event.message) {
      setMessages((current) => mergeMessages(current, [event.message!]));
    } else if (event.kind === "delta" && event.delta) {
      setMessages((current) =>
        appendMessageDelta(current, event.messageId, event.delta!),
      );
    }
    // A failed run is rendered inline as an error card on the failed assistant
    // message (see MessageRow), not as a bubble or the bottom error bar — the
    // bar is reserved for app-level errors (load / send / auth).
  }

  function rememberRunMessage(runId: string, messageId: string) {
    if (runMessageIds()[runId] === messageId) {
      return;
    }

    setRunMessageIds((current) => ({
      ...current,
      [runId]: messageId,
    }));
    setToolExecutions((current) =>
      attachMessageIdToToolExecutions(current, runId, messageId),
    );
  }

  function hydrateToolExecutions(records: ToolExecutionRecord[]) {
    if (records.length === 0) {
      return;
    }

    setRunMessageIds((current) => {
      const next = { ...current };
      for (const record of records) {
        if (record.runId) {
          next[record.runId] = record.messageId;
        }
      }
      return next;
    });
    setToolExecutions((current) => ({
      ...current,
      ...Object.fromEntries(
        records.map((record) => [
          record.toolCallId,
          toolExecutionViewFromRecord(record),
        ]),
      ),
    }));
  }

  function clearMessageToolExecutions(messageId: string) {
    clearToolExecutionsForMessageIds([messageId]);
  }

  function clearToolExecutionsForMessageIds(messageIds: string[]) {
    if (messageIds.length === 0) {
      return;
    }

    const removedMessageIds = new Set(messageIds);
    const toolCallIds = Object.values(toolExecutions())
      .filter((tool) => tool.messageId && removedMessageIds.has(tool.messageId))
      .map((tool) => tool.toolCallId);
    if (toolCallIds.length === 0) {
      return;
    }

    const removed = new Set(toolCallIds);
    setToolExecutions((current) =>
      Object.fromEntries(
        Object.entries(current).filter(
          ([toolCallId]) => !removed.has(toolCallId),
        ),
      ),
    );
    setExpandedInlineTools((current) =>
      Object.fromEntries(
        Object.entries(current).filter(
          ([toolCallId]) => !removed.has(toolCallId),
        ),
      ),
    );
  }

  function applyToolExecutionEvent(event: ToolExecutionEvent) {
    setToolExecutions((current) => {
      const previous = current[event.toolCallId];
      const now = Date.now();
      const runId = event.runId ?? previous?.runId;
      const output =
        event.kind === "output" && event.chunk
          ? appendToolOutput(previous?.output ?? "", event.stream, event.chunk)
          : previous?.output ?? "";

      return {
        ...current,
        [event.toolCallId]: {
          toolCallId: event.toolCallId,
          runId,
          messageId:
            previous?.messageId ?? (runId ? runMessageIds()[runId] : undefined),
          chatId: previous?.chatId,
          projectId: event.projectId ?? previous?.projectId,
          command: event.command ?? previous?.command,
          kind: event.kind,
          message: event.message ?? previous?.message,
          output,
          result: event.result ?? previous?.result,
          createdAt: previous?.createdAt ?? now,
          updatedAt: now,
        },
      };
    });
  }

  const visibleToolExecutions = () =>
    Object.values(toolExecutions())
      .sort((left, right) => right.updatedAt - left.updatedAt)
      .slice(0, 50);

  const inlineToolExecutionsByMessageId = () => {
    const messageIds = runMessageIds();
    const grouped: Record<string, ToolExecutionView[]> = {};

    for (const tool of Object.values(toolExecutions())) {
      const messageId =
        tool.messageId ?? (tool.runId ? messageIds[tool.runId] : undefined);
      if (!messageId) {
        continue;
      }

      grouped[messageId] ??= [];
      grouped[messageId].push(tool);
    }

    for (const tools of Object.values(grouped)) {
      tools.sort(compareToolExecutions);
    }

    return grouped;
  };

  function handleToggleInlineTool(toolCallId: string) {
    setExpandedInlineTools((current) => ({
      ...current,
      [toolCallId]: !current[toolCallId],
    }));
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
        editingDraft={editingDraft()}
        editingMessageId={editingMessageId()}
        isLoading={
          isLoadingMessages() || (isLoadingChats() && messages().length === 0)
        }
        isSending={isSending() || isSubmittingEdit() || isChatRunning()}
        messages={messages()}
        runTransports={runTransports()}
        toolExecutionsByMessageId={inlineToolExecutionsByMessageId()}
        expandedInlineTools={expandedInlineTools()}
        activeRunId={activeRunId()}
        onApproveTool={handleApproveTool}
        onCancelRun={handleCancelRun}
        onCancelTool={handleCancelTool}
        onBranchMessage={(message) => void handleBranchMessage(message)}
        onCancelEdit={handleCancelEdit}
        onDenyTool={handleDenyTool}
        onDraftChange={setDraft}
        onEditDraftChange={setEditingDraft}
        onRetry={handleRetry}
        onSelectModel={handleSelectModel}
        onSendMessage={handleSendMessage}
        onStartEdit={handleStartEdit}
        onSubmitEdit={(messageId) => void handleSubmitEdit(messageId)}
        onToggleInlineTool={handleToggleInlineTool}
      />
      <InspectorPane
        activeChat={activeChat()}
        messageCount={messages().length}
        toolExecutions={visibleToolExecutions()}
        onApproveTool={handleApproveTool}
        onCancelTool={handleCancelTool}
        onDenyTool={handleDenyTool}
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
  activeRunId?: string;
  connectorSettings?: ConnectorSettingsSnapshot;
  draft: string;
  editingDraft: string;
  editingMessageId?: string;
  error: string;
  isLoading: boolean;
  isSending: boolean;
  messages: ChatMessage[];
  runTransports: Record<string, string>;
  toolExecutionsByMessageId: Record<string, ToolExecutionView[]>;
  expandedInlineTools: Record<string, boolean>;
  onApproveTool: (toolCallId: string) => void;
  onBranchMessage: (message: ChatMessage) => void;
  onCancelEdit: () => void;
  onCancelRun: () => void;
  onCancelTool: (toolCallId: string) => void;
  onDenyTool: (toolCallId: string) => void;
  onDraftChange: (value: string) => void;
  onEditDraftChange: (value: string) => void;
  onRetry: () => void;
  onSelectModel: (providerId: string, modelId: string) => void;
  onSendMessage: () => void;
  onStartEdit: (message: ChatMessage) => void;
  onSubmitEdit: (messageId: string) => void;
  onToggleInlineTool: (toolCallId: string) => void;
}) {
  const timelineItems = () =>
    buildConversationTimeline(props.messages, props.toolExecutionsByMessageId);

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
        estimateSize={(item) => estimateTimelineItemSize(item, props.expandedInlineTools)}
        getItemKey={(item) => item.id}
        items={timelineItems()}
        overscan={4}
        scrollKey={props.activeChat?.id}
        stickToEnd
      >
        {(item) => (
          <Show
            when={item.kind === "tool"}
            fallback={
              <MessageRow
                message={(item as MessageTimelineItem).message}
                transport={
                  props.runTransports[(item as MessageTimelineItem).message.id]
                }
                editingDraft={props.editingDraft}
                isEditing={
                  props.editingMessageId ===
                  (item as MessageTimelineItem).message.id
                }
                isBusy={props.isSending}
                onBranchMessage={props.onBranchMessage}
                onCancelEdit={props.onCancelEdit}
                onEditDraftChange={props.onEditDraftChange}
                onRetry={props.onRetry}
                settings={props.connectorSettings}
                onStartEdit={props.onStartEdit}
                onSubmitEdit={props.onSubmitEdit}
              />
            }
          >
            <TimelineToolRow
              expanded={Boolean(
                props.expandedInlineTools[
                  (item as ToolTimelineItem).tool.toolCallId
                ],
              )}
              tool={(item as ToolTimelineItem).tool}
              onApproveTool={props.onApproveTool}
              onCancelTool={props.onCancelTool}
              onDenyTool={props.onDenyTool}
              onToggleTool={props.onToggleInlineTool}
            />
          </Show>
        )}
      </VirtualList>

      <Show when={props.error}>
        <div class="chat-error" role="alert">
          {props.error}
        </div>
      </Show>

      <Composer
        activeRunId={props.activeRunId}
        draft={props.draft}
        isSending={props.isSending}
        onCancelRun={props.onCancelRun}
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
    return (
      <div class="conversation-state conversation-state--loading">
        <div class="message-skeleton message-skeleton--assistant" />
        <div class="message-skeleton message-skeleton--user" />
        <div class="message-skeleton message-skeleton--assistant message-skeleton--short" />
      </div>
    );
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
  const isRefreshing = () =>
    props.settings?.providers.some(
      (provider) =>
        provider.refreshStatus === "pending" ||
        provider.refreshStatus === "refreshing",
    ) ?? true;
  const hasConnectorError = () =>
    props.settings?.providers.some(
      (provider) => provider.modelError && provider.models.length === 0,
    ) ?? false;
  const selectedValue = () => {
    const selected = props.settings?.selectedModel;
    return selected
      ? modelOptionValue(selected.providerId, selected.modelId)
      : "";
  };

  return (
    <select
      class="model-select"
      aria-label="Active model"
      value={selectedValue()}
      onChange={(event) => {
        const [providerId, modelId] = parseModelOptionValue(
          event.currentTarget.value,
        );
        if (providerId && modelId) {
          props.onSelectModel(providerId, modelId);
        }
      }}
    >
      <Show
        when={models().length > 0}
        fallback={
          <option value="">
            {isRefreshing()
              ? "Loading models..."
              : hasConnectorError()
                ? "Connector unavailable"
                : "No models connected"}
          </option>
        }
      >
        {models().map((model) => (
          <option value={modelOptionValue(model.providerId, model.id)}>
            {model.providerLabel} / {model.label}
          </option>
        ))}
      </Show>
    </select>
  );
}

function modelOptionValue(providerId: string, modelId: string) {
  return JSON.stringify([providerId, modelId]);
}

function parseModelOptionValue(value: string): [string, string] {
  try {
    const parsed = JSON.parse(value);
    return typeof parsed?.[0] === "string" && typeof parsed?.[1] === "string"
      ? [parsed[0], parsed[1]]
      : ["", ""];
  } catch {
    return ["", ""];
  }
}

function InspectorPane(props: {
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
              {activeTools() > 0 ? "Tools" : "Idle"}
            </span>
          }
        >
          <div class="run-summary">
            <div class="run-summary__title">
              <Terminal size={16} />
              <strong>No active run</strong>
            </div>
            <span>
              {activeTools() > 0
                ? `${activeTools()} tool job${activeTools() === 1 ? "" : "s"} active`
                : "Chat persistence is enabled"}
            </span>
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
              {props.toolExecutions.length} <ChevronDown size={13} />
            </button>
          }
        >
          <Show
            when={props.toolExecutions.length > 0}
            fallback={<div class="panel-empty">Tool calls will appear here.</div>}
          >
            <div class="tool-call-list">
              {props.toolExecutions.map((tool) => (
                <ToolCallRow
                  tool={tool}
                  onApprove={() => props.onApproveTool(tool.toolCallId)}
                  onCancel={() => props.onCancelTool(tool.toolCallId)}
                  onDeny={() => props.onDenyTool(tool.toolCallId)}
                />
              ))}
            </div>
          </Show>
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

function ToolCallRow(props: {
  tool: ToolExecutionView;
  onApprove: () => void;
  onCancel: () => void;
  onDeny: () => void;
}) {
  const command = () => formatToolCommand(props.tool.command);
  const canApprove = () => props.tool.kind === "permission_requested";
  const canCancel = () =>
    !canApprove() && !isTerminalToolKind(props.tool.kind);

  return (
    <div class="tool-call-row">
      <Terminal size={16} />
      <div class="tool-call-row__body">
        <strong title={command()}>{command()}</strong>
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

function MessageRow(props: {
  message: ChatMessage;
  transport?: string;
  editingDraft: string;
  isEditing: boolean;
  isBusy?: boolean;
  onBranchMessage: (message: ChatMessage) => void;
  onCancelEdit: () => void;
  onEditDraftChange: (value: string) => void;
  onRetry?: () => void;
  settings?: ConnectorSettingsSnapshot;
  onStartEdit: (message: ChatMessage) => void;
  onSubmitEdit: (messageId: string) => void;
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

  // Messenger layout: user on the right in a colored bubble, agent on the left
  // with an avatar. A failed run is not an agent message — it renders as a
  // dedicated red error card (human summary + raw details + retry) instead.
  // Both render Markdown (GFM) via solid-markdown (component output, not
  // innerHTML, so it is XSS-safe). Kept scroll-cheap.
  return (
    <Show
      when={isFailed()}
      fallback={
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
                  <div class="message-md">
                    <SolidMarkdown
                      renderingStrategy="reconcile"
                      remarkPlugins={[remarkGfm]}
                      children={body()}
                    />
                  </div>
                  <MessageActions
                    disabled={Boolean(props.isBusy)}
                    isUser={isUser()}
                    message={message()}
                    onBranch={props.onBranchMessage}
                    onEdit={props.onStartEdit}
                  />
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
          </div>
        </article>
      }
    >
      <article class="message-row message-row--error">
        <div class="message-row__content message-row__content--error">
          <ErrorCard
            error={message().content}
            disabled={Boolean(props.isBusy)}
            onRetry={props.onRetry}
          />
        </div>
      </article>
    </Show>
  );
}

function MessageActions(props: {
  disabled: boolean;
  isUser: boolean;
  message: ChatMessage;
  onBranch: (message: ChatMessage) => void;
  onEdit: (message: ChatMessage) => void;
}) {
  return (
    <div class="message-actions">
      <Show when={props.isUser}>
        <button
          class="message-action-button"
          type="button"
          title="Edit message"
          disabled={props.disabled || props.message.status !== "complete"}
          onClick={() => props.onEdit(props.message)}
        >
          <Pencil size={14} />
        </button>
      </Show>
      <Show when={!props.isUser}>
        <button
          class="message-action-button"
          type="button"
          title="Branch from this response"
          disabled={props.disabled || props.message.status === "sending"}
          onClick={() => props.onBranch(props.message)}
        >
          <GitBranch size={14} />
        </button>
      </Show>
    </div>
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

function TimelineToolRow(props: {
  expanded: boolean;
  tool: ToolExecutionView;
  onApproveTool: (toolCallId: string) => void;
  onCancelTool: (toolCallId: string) => void;
  onDenyTool: (toolCallId: string) => void;
  onToggleTool: (toolCallId: string) => void;
}) {
  return (
    <article class="message-row message-row--tool">
      <div class="message-row__avatar-spacer" aria-hidden="true" />
      <div class="message-row__content message-row__content--tool">
        <InlineToolCall
          expanded={props.expanded}
          tool={props.tool}
          onApprove={() => props.onApproveTool(props.tool.toolCallId)}
          onCancel={() => props.onCancelTool(props.tool.toolCallId)}
          onDeny={() => props.onDenyTool(props.tool.toolCallId)}
          onToggle={() => props.onToggleTool(props.tool.toolCallId)}
        />
      </div>
    </article>
  );
}

function InlineToolCall(props: {
  expanded: boolean;
  tool: ToolExecutionView;
  onApprove: () => void;
  onCancel: () => void;
  onDeny: () => void;
  onToggle: () => void;
}) {
  const command = () => formatToolCommand(props.tool.command);
  const output = () => formatToolOutput(props.tool);
  const canApprove = () => props.tool.kind === "permission_requested";
  const canCancel = () =>
    !canApprove() && !isTerminalToolKind(props.tool.kind);

  return (
    <div
      classList={{
        "inline-tool-call": true,
        "inline-tool-call--open": props.expanded,
      }}
    >
      <button
        class="inline-tool-call__summary"
        type="button"
        aria-expanded={props.expanded}
        onClick={props.onToggle}
      >
        <ChevronDown
          size={14}
          classList={{
            "inline-tool-call__chevron": true,
            "inline-tool-call__chevron--open": props.expanded,
          }}
        />
        <Terminal size={15} />
        <span class="inline-tool-call__title" title={command()}>
          {formatToolHeadline(props.tool.command)}
        </span>
        <span class={`tool-status tool-status--${toolTone(props.tool.kind)}`}>
          {toolStatusLabel(props.tool.kind)}
        </span>
      </button>

      <Show when={props.expanded}>
        <div class="inline-tool-call__body">
          <div class="inline-tool-call__label">{toolCommandLabel(props.tool.command)}</div>
          <pre class="inline-tool-call__command">$ {command()}</pre>
          <Show when={props.tool.message}>
            <p class="inline-tool-call__message">{props.tool.message}</p>
          </Show>
          <Show when={output()}>
            <pre class="inline-tool-call__output">{output()}</pre>
          </Show>
          <Show when={canApprove() || canCancel()}>
            <div class="inline-tool-call__actions">
              <Show when={canApprove()}>
                <button type="button" onClick={props.onApprove}>
                  <Check size={14} />
                  Approve
                </button>
                <button type="button" onClick={props.onDeny}>
                  <X size={14} />
                  Deny
                </button>
              </Show>
              <Show when={canCancel()}>
                <button type="button" onClick={props.onCancel}>
                  <Square size={12} />
                  Cancel
                </button>
              </Show>
            </div>
          </Show>
        </div>
      </Show>
    </div>
  );
}

function ErrorCard(props: {
  error: string;
  disabled: boolean;
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

function Composer(props: {
  activeRunId?: string;
  draft: string;
  isSending: boolean;
  onCancelRun: () => void;
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
          // Enter sends; Shift+Enter inserts a newline. (`isComposing` guards IME.)
          if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
            event.preventDefault();
            props.onSend();
          }
        }}
      />
      <div class="composer__actions">
        <button class="icon-button" type="button" title="Attach file">
          <Paperclip size={17} />
        </button>
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

function appendMessageDelta(
  current: ChatMessage[],
  messageId: string,
  delta: string,
): ChatMessage[] {
  let changed = false;
  const next = current.map((message) => {
    if (message.id !== messageId) {
      return message;
    }
    changed = true;
    return {
      ...message,
      content: `${message.content}${delta}`,
    };
  });

  return changed ? next : current;
}

function attachMessageIdToToolExecutions(
  current: Record<string, ToolExecutionView>,
  runId: string,
  messageId: string,
): Record<string, ToolExecutionView> {
  let changed = false;
  const next: Record<string, ToolExecutionView> = {};

  for (const [toolCallId, tool] of Object.entries(current)) {
    if (tool.runId === runId && tool.messageId !== messageId) {
      changed = true;
      next[toolCallId] = { ...tool, messageId };
    } else {
      next[toolCallId] = tool;
    }
  }

  return changed ? next : current;
}

function toolExecutionViewFromRecord(
  record: ToolExecutionRecord,
): ToolExecutionView {
  return {
    toolCallId: record.toolCallId,
    runId: record.runId,
    chatId: record.chatId,
    messageId: record.messageId,
    projectId: record.projectId,
    command: record.command,
    kind: record.kind,
    message: record.message,
    output: record.output,
    result: record.result,
    createdAt: timestampToMillis(record.createdAt),
    updatedAt: timestampToMillis(record.updatedAt),
  };
}

function compareToolExecutions(
  left: ToolExecutionView,
  right: ToolExecutionView,
) {
  if (left.createdAt !== right.createdAt) {
    return left.createdAt - right.createdAt;
  }

  return left.toolCallId.localeCompare(right.toolCallId);
}

function buildConversationTimeline(
  messages: ChatMessage[],
  toolExecutionsByMessageId: Record<string, ToolExecutionView[]>,
): ConversationTimelineItem[] {
  const items: ConversationTimelineItem[] = [];

  for (const message of messages) {
    const tools = toolExecutionsByMessageId[message.id] ?? [];
    if (message.role === "assistant" && tools.length > 0) {
      for (const tool of tools) {
        items.push({
          id: `tool:${tool.toolCallId}`,
          kind: "tool",
          messageId: message.id,
          tool,
        });
      }
    }

    items.push({
      id: `message:${message.id}`,
      kind: "message",
      message,
    });
  }

  return items;
}

function estimateTimelineItemSize(
  item: ConversationTimelineItem,
  expandedTools: Record<string, boolean>,
) {
  if (item.kind === "tool") {
    return expandedTools[item.tool.toolCallId] ? 340 : 56;
  }

  const message = item.message;
  return Math.max(120, Math.min(520, 100 + message.content.length * 0.4));
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

function timestampToMillis(timestamp: string) {
  const numericTimestamp = Number(timestamp);
  return Number.isFinite(numericTimestamp) ? numericTimestamp * 1000 : Date.now();
}

function currentUnixTimestamp() {
  return Math.floor(Date.now() / 1000).toString();
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Resolves an assistant message's provider/model into a display name + adapter
 * icon, using the live connector list. Falls back to "Mothership" (and the
 * built-in logo) for user messages or when the producing adapter is unknown.
 */
function resolveAttribution(
  settings: ConnectorSettingsSnapshot | undefined,
  providerId?: string | null,
  modelId?: string | null,
): { name: string; icon?: string | null } {
  if (!providerId) {
    return { name: "Mothership", icon: undefined };
  }
  const provider = settings?.providers.find((item) => item.id === providerId);
  const model = provider?.models.find((item) => item.id === modelId);
  const name = model?.label ?? modelId ?? provider?.label ?? "Mothership";
  return { name, icon: provider?.icon ?? undefined };
}

/**
 * Turns a raw run error into a one-line human summary: strips our internal
 * wrapper prefixes and, if the provider returned a JSON body, surfaces its
 * detail/message. The full original string is still shown under "Details".
 */
function humanizeError(raw: string): string {
  let message = (raw ?? "").trim();

  for (const prefix of [
    "invalid request:",
    "adapter chat failed:",
    "adapter chat error:",
  ]) {
    if (message.toLowerCase().startsWith(prefix)) {
      message = message.slice(prefix.length).trim();
    }
  }

  const jsonStart = message.indexOf("{");
  if (jsonStart !== -1) {
    try {
      const parsed = JSON.parse(message.slice(jsonStart));
      const detail =
        parsed?.detail ??
        parsed?.message ??
        parsed?.error?.message ??
        (typeof parsed?.error === "string" ? parsed.error : undefined);
      if (typeof detail === "string" && detail.trim()) {
        return detail.trim();
      }
    } catch {
      // Not JSON — fall through to the cleaned string.
    }
  }

  return message || "The model provider could not complete this request.";
}

function appendToolOutput(
  current: string,
  stream: "stdout" | "stderr" | null | undefined,
  chunk: string,
) {
  const prefix = stream === "stderr" ? "[stderr] " : "";
  const next = `${current}${prefix}${chunk}`;
  const max = 12_000;
  return next.length > max ? `... output trimmed ...\n${next.slice(-max)}` : next;
}

function formatToolCommand(command?: ToolCommand | null) {
  if (!command) {
    return "tool command";
  }
  return [command.program, ...(command.args ?? [])].join(" ");
}

function formatToolHeadline(command?: ToolCommand | null) {
  if (!command) {
    return "Used tool";
  }

  if (isPowerShellCommand(command.program)) {
    return "Used PowerShell";
  }

  return `Ran ${command.program}`;
}

function toolCommandLabel(command?: ToolCommand | null) {
  if (!command) {
    return "Command";
  }

  return isPowerShellCommand(command.program) ? "PowerShell" : "Command";
}

function isPowerShellCommand(program: string) {
  const normalized = program.toLowerCase();
  return (
    normalized === "powershell" ||
    normalized === "powershell.exe" ||
    normalized === "pwsh" ||
    normalized === "pwsh.exe"
  );
}

function formatToolOutput(tool: ToolExecutionView) {
  if (tool.output) {
    return tool.output;
  }

  if (!tool.result) {
    return "";
  }

  const chunks = [
    (tool.result.stdoutPreview || tool.result.stdoutTail || "").trim(),
    (tool.result.stderrPreview || tool.result.stderrTail || "").trim()
      ? `[stderr]\n${(tool.result.stderrPreview || tool.result.stderrTail || "").trim()}`
      : "",
    tool.result.message?.trim() ?? "",
  ].filter(Boolean);

  return chunks.join("\n\n");
}

function isTerminalToolKind(kind: ToolExecutionEventKind) {
  return (
    kind === "completed" ||
    kind === "failed" ||
    kind === "cancelled" ||
    kind === "timed_out" ||
    kind === "permission_denied"
  );
}

function toolStatusLabel(kind: ToolExecutionEventKind) {
  const labels: Record<ToolExecutionEventKind, string> = {
    queued: "Queued",
    permission_requested: "Needs approval",
    permission_denied: "Denied",
    waiting_for_resource: "Waiting",
    started: "Running",
    output: "Running",
    completed: "Completed",
    failed: "Failed",
    cancelled: "Cancelled",
    timed_out: "Timed out",
  };
  return labels[kind];
}

function toolTone(kind: ToolExecutionEventKind) {
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
  if (kind === "permission_requested" || kind === "waiting_for_resource") {
    return "pending";
  }
  return "running";
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

type RecentStatus = "active" | "branch" | "clock";

type ConversationTimelineItem = MessageTimelineItem | ToolTimelineItem;

interface MessageTimelineItem {
  id: string;
  kind: "message";
  message: ChatMessage;
}

interface ToolTimelineItem {
  id: string;
  kind: "tool";
  messageId: string;
  tool: ToolExecutionView;
}

interface ProjectItem {
  branch: string;
  id: string;
  name: string;
  path: string;
  tone: "blue" | "green" | "yellow";
}

interface ToolExecutionView {
  toolCallId: string;
  runId?: string | null;
  chatId?: string | null;
  messageId?: string | null;
  projectId?: string | null;
  command?: ToolCommand | null;
  kind: ToolExecutionEventKind;
  message?: string | null;
  output: string;
  result?: ToolExecutionResult | null;
  createdAt: number;
  updatedAt: number;
}
