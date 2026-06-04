import {
  createEffect,
  createMemo,
  createSignal,
  For,
  JSX,
  Match,
  onCleanup,
  onMount,
  Show,
  Switch,
} from "solid-js";
import {
  AlertTriangle,
  BrainCircuit,
  Check,
  ChevronDown,
  Circle,
  Clock,
  FileText,
  Folder,
  GitBranch,
  MoreVertical,
  Paperclip,
  Play,
  Plus,
  RefreshCw,
  Search,
  Send,
  Square,
  Terminal,
  X,
} from "lucide-solid";
import { listen } from "@tauri-apps/api/event";
import { SolidMarkdown } from "solid-markdown";
import remarkGfm from "remark-gfm";

import {
  ChatRunEvent,
  ChatMessage,
  ChatMessagePart,
  ChatThreadSummary,
  ChatUpdatedEvent,
  ConnectorSettingsEvent,
  ConnectorProviderSummary,
  ConnectorSettingsSnapshot,
  LlmModel,
  ProjectSummary,
  ProjectSnapshot,
  ReasoningConfig,
  ReasoningOption,
  SendChatMessageResult,
  ToolArtifact,
  ToolCommand,
  ToolExecutionEvent,
  ToolExecutionEventKind,
  ToolExecutionRecord,
  ToolExecutionResult,
  ToolKind,
  approveToolExecution,
  branchChatFromMessage,
  cancelChatRun,
  cancelToolExecution,
  continueChatMessage,
  createChat,
  editChatUserMessage,
  getChat,
  getConnectorSettings,
  listChats,
  listProjects,
  openProject,
  pickProjectDirectory,
  retryChatMessage,
  sendChatMessage,
  setChatModel,
  setSelectedModel,
} from "../../shared/api/mothership";
import { ApprovalPreview, ToolCard } from "./ToolCards";
import { VirtualList } from "../../shared/ui/VirtualList";
import { startWindowDrag } from "../../shared/window-drag";
import mothershipLogoUrl from "../../assets/mothership-logo-sm.png";

const CHAT_MESSAGE_PAGE_SIZE = 60;
const CHAT_SCROLL_BOTTOM_THRESHOLD_PX = 8;
const CHAT_SCROLL_TOP_PADDING_PX = 12;
const CHAT_SCROLL_BOTTOM_PADDING_PX = 32;
const CHAT_SCROLL_RESTORE_FRAMES = 12;
const TOOL_OUTPUT_MAX_VISIBLE_LINES = 10;

type ReasoningOptionId = string;

interface ChatScrollPosition {
  top: number;
  fromBottom: number;
}

export function Dashboard(props: { onOpenSettings?: () => void }) {
  const [projects, setProjects] = createSignal<ProjectSummary[]>([]);
  const [activeProjectId, setActiveProjectId] = createSignal<string>();
  const [chats, setChats] = createSignal<ChatThreadSummary[]>([]);
  const [messages, setMessages] = createSignal<ChatMessage[]>([]);
  const [messageParts, setMessageParts] = createSignal<
    Record<string, MessagePartView[]>
  >({});
  const [connectorSettings, setConnectorSettings] =
    createSignal<ConnectorSettingsSnapshot>();
  const [activeChatId, setActiveChatId] = createSignal<string>();
  const [draft, setDraft] = createSignal("");
  const [error, setError] = createSignal("");
  const [runTransports, setRunTransports] = createSignal<Record<string, string>>(
    {},
  );
  const [isLoadingProjects, setIsLoadingProjects] = createSignal(true);
  const [isLoadingChats, setIsLoadingChats] = createSignal(true);
  const [isOpeningProject, setIsOpeningProject] = createSignal(false);
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
  const [toolExecutions, setToolExecutions] = createSignal<
    Record<string, ToolExecutionView>
  >({});
  const [expandedInlineTools, setExpandedInlineTools] = createSignal<
    Record<string, boolean>
  >({});
  let unlistenChatRun: (() => void) | undefined;
  let unlistenConnectorSettings: (() => void) | undefined;
  let unlistenToolExecution: (() => void) | undefined;
  let unlistenChatUpdated: (() => void) | undefined;
  let messageScrollElement: HTMLDivElement | undefined;
  let restoreScrollFrame = 0;
  let openChatRequestId = 0;
  let liveMessagePartSequence = 0;
  const chatScrollPositions = new Map<string, ChatScrollPosition>();

  const activeProject = () =>
    projects().find((project) => project.id === activeProjectId()) ?? null;
  const activeChat = () =>
    chats().find((chat) => chat.id === activeChatId()) ?? null;
  const activeRunId = () => {
    const chatId = activeChatId();
    return chatId ? activeRunIds()[chatId] : undefined;
  };
  // The "current model" follows the ACTIVE CHAT (provider/model persisted on the
  // chat), falling back to the global default-for-new-chats. This drives reasoning
  // coercion and the header selectors, so opening a chat restores its model.
  const selectedModel = createMemo(() => {
    const chat = activeChat();
    const settings = connectorSettings();
    return selectableConnectorModelFor(
      settings,
      chat?.providerId ?? settings?.selectedModel.providerId,
      chat?.modelId ?? settings?.selectedModel.modelId,
    );
  });
  const [reasoningOptionId, setReasoningOptionId] =
    createSignal<ReasoningOptionId>();
  const isChatRunning = () =>
    messages().some(
      (message) => message.role === "assistant" && message.status === "sending",
    );

  createEffect(() => {
    const coerced = coerceReasoningOptionId(reasoningOptionId(), selectedModel());
    if (coerced !== reasoningOptionId()) {
      setReasoningOptionId(coerced);
    }
  });

  onMount(() => {
    void loadProjectScope();
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

      void listen<ChatUpdatedEvent>("chat-updated", (event) => {
        // A chat's metadata changed (e.g. its model was set, possibly on another
        // client). Patch the summary in place — no reorder — so the active chat's
        // model and the header selectors update reactively.
        setChats((current) => mergeChatInPlace(current, event.payload.chat));
      }).then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          unlistenChatUpdated = unlisten;
        }
      });

      onCleanup(() => {
        disposed = true;
      });
    }
  });

  onCleanup(() => {
    cancelAnimationFrame(restoreScrollFrame);
    unlistenChatRun?.();
    unlistenConnectorSettings?.();
    unlistenToolExecution?.();
    unlistenChatUpdated?.();
  });

  async function loadProjectScope() {
    setIsLoadingProjects(true);
    setIsLoadingChats(true);
    setError("");

    try {
      const snapshot = await listProjects();
      applyProjectSnapshot(snapshot);
      // Which project this client views is purely local UI state — we never ask
      // the backend to switch a global "active" project (a phone and a desktop
      // must be able to view different projects independently). The snapshot's id
      // is only an initial hint; otherwise fall back to the first project.
      const projectId = snapshot.activeProjectId ?? snapshot.projects[0]?.id;
      if (projectId) {
        await loadChats(projectId);
      } else {
        setActiveProjectId(undefined);
        setChats([]);
        setMessages([]);
        setMessageParts({});
        setIsLoadingMessages(false);
      }
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    } finally {
      setIsLoadingProjects(false);
      setIsLoadingChats(false);
    }
  }

  // Single writer for project state. Trust the snapshot's active id when it gives
  // one, but never clear a still-valid selection just because a snapshot omitted
  // it — the active project is owned by the load/select flow (and, once wired, the
  // project-event), not by every snapshot echo. Only drop it if the project it
  // points at no longer exists.
  function applyProjectSnapshot(snapshot: ProjectSnapshot) {
    setProjects(snapshot.projects);
    if (snapshot.activeProjectId) {
      setActiveProjectId(snapshot.activeProjectId);
    } else if (
      !snapshot.projects.some((project) => project.id === activeProjectId())
    ) {
      setActiveProjectId(undefined);
    }
  }

  async function loadChats(projectId: string) {
    // The project whose chats we load IS the active project (UI source of truth),
    // regardless of whether the snapshot echoed an active id back.
    setActiveProjectId(projectId);
    setIsLoadingChats(true);
    setError("");

    try {
      const loadedChats = await listChats(100, projectId);
      if (activeProjectId() !== projectId) {
        return;
      }
      setChats(loadedChats);

      if (loadedChats[0]) {
        await openChat(loadedChats[0].id);
      } else {
        openChatRequestId += 1;
        setActiveChatId(undefined);
        setMessages([]);
        setMessageParts({});
        setIsLoadingMessages(false);
      }
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    } finally {
      setIsLoadingChats(false);
    }
  }

  async function handleSelectProject(projectId: string) {
    if (projectId === activeProjectId()) {
      return;
    }

    saveActiveChatScroll();
    openChatRequestId += 1;
    setActiveProjectId(projectId);
    setActiveChatId(undefined);
    setChats([]);
    setMessages([]);
    setMessageParts({});
    setIsLoadingMessages(false);
    setToolExecutions({});
    setExpandedInlineTools({});
    setEditingMessageId(undefined);
    setEditingDraft("");
    setError("");

    try {
      // Local view switch — load this project's chats by id. No backend call sets
      // a global active project.
      await loadChats(projectId);
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function openProjectPath(path: string) {
    const snapshot = await openProject(path);
    applyProjectSnapshot(snapshot);
    const projectId = snapshot.activeProjectId;
    if (!projectId) {
      return;
    }

    saveActiveChatScroll();
    openChatRequestId += 1;
    setActiveChatId(undefined);
    setMessages([]);
    setMessageParts({});
    setIsLoadingMessages(false);
    setToolExecutions({});
    setExpandedInlineTools({});
    await loadChats(projectId);
  }

  async function handlePickProjectDirectory() {
    if (isOpeningProject()) {
      return;
    }

    setIsOpeningProject(true);
    setError("");

    try {
      const path = await pickProjectDirectory();
      if (path) {
        await openProjectPath(path);
      }
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    } finally {
      setIsOpeningProject(false);
    }
  }

  async function openChat(chatId: string) {
    saveActiveChatScroll();
    const requestId = ++openChatRequestId;
    setActiveChatId(chatId);
    setMessages([]);
    setMessageParts({});
    setEditingMessageId(undefined);
    setEditingDraft("");
    setIsLoadingMessages(true);
    setError("");

    try {
      const conversation = await getChat(chatId, CHAT_MESSAGE_PAGE_SIZE);
      if (requestId !== openChatRequestId || activeChatId() !== chatId) {
        return;
      }
      setChats((current) => mergeChatInPlace(current, conversation.chat));
      setMessages(normalizeMessages(conversation.messages));
      setMessageParts(messagePartsByMessageId(conversation.messageParts ?? []));
      hydrateToolExecutions(conversation.toolExecutions ?? []);
      restoreChatScroll(chatId);
    } catch (caughtError) {
      if (requestId === openChatRequestId) {
        setError(errorMessage(caughtError));
      }
    } finally {
      if (requestId === openChatRequestId) {
        setIsLoadingMessages(false);
      }
    }
  }

  async function handleNewChat() {
    const projectId = activeProjectId();
    if (!projectId) {
      setError("Open a project before starting a chat.");
      return;
    }

    saveActiveChatScroll();
    openChatRequestId += 1;
    setIsLoadingMessages(false);
    setError("");

    try {
      const conversation = await createChat(projectId);
      setChats((current) => bumpChat(current, conversation.chat));
      setActiveChatId(conversation.chat.id);
      setMessages(normalizeMessages(conversation.messages));
      setMessageParts({});
      restoreChatScroll(conversation.chat.id);
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function handleSendMessage() {
    const content = draft().trim();
    if (!content || isSending() || isSubmittingEdit() || isChatRunning()) {
      return;
    }
    const projectId = activeProjectId();
    if (!projectId) {
      setError("Open a project before sending a message.");
      return;
    }

    const currentChatId = activeChatId();
    setDraft("");
    setIsSending(true);
    setError("");

    try {
      const result = await sendChatMessage(
        currentChatId,
        content,
        projectId,
        reasoningConfigForOption(reasoningOptionId(), selectedModel()),
      );

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
      if (currentChatId !== result.chat.id) {
        setMessageParts({});
      }
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
      clearMessagePartsForMessageIds(removedMessageIds);
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

    saveActiveChatScroll();
    openChatRequestId += 1;
    setBranchingChatId(tempChatId);
    setEditingMessageId(undefined);
    setEditingDraft("");
    setError("");
    setChats((current) => bumpChat(current, tempChat));
    setActiveChatId(tempChatId);
    setMessages([]);
    setMessageParts({});
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
      setMessages(limitChatMessages(normalizeMessages(conversation.messages)));
      setMessageParts(messagePartsByMessageId(conversation.messageParts ?? []));
      hydrateToolExecutions(conversation.toolExecutions ?? []);
      restoreChatScroll(conversation.chat.id);
    } catch (caughtError) {
      setChats((current) => current.filter((chat) => chat.id !== tempChatId));
      setActiveChatId(sourceChatId);
      setMessages(sourceMessages);
      setError(errorMessage(caughtError));
      restoreChatScroll(sourceChatId);
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
      const removedMessageIds = removedMessageIdsForRetry(result, messages());
      setActiveRunIds((current) => ({
        ...current,
        [result.chat.id]: result.runId,
      }));
      clearToolExecutionsForMessageIds(removedMessageIds);
      clearMessagePartsForMessageIds(removedMessageIds);
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
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function handleContinue() {
    const chatId = activeChatId();
    if (!chatId || isChatRunning()) {
      return;
    }
    setError("");

    try {
      const result = await continueChatMessage(chatId);
      setActiveRunIds((current) => ({
        ...current,
        [result.chat.id]: result.runId,
      }));
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
    const chatId = activeChatId();
    try {
      if (chatId) {
        // Per-chat model: persisted on the chat and synced to other clients via
        // the chat-updated event. We also patch the local summary in place so the
        // selector reflects it immediately.
        const chat = await setChatModel(chatId, providerId, modelId);
        setChats((current) => mergeChatInPlace(current, chat));
      } else {
        // No active chat → set the global default applied to new chats.
        setConnectorSettings(await setSelectedModel(providerId, modelId));
      }
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

  function handleToggleInlineTool(toolCallId: string) {
    setExpandedInlineTools((current) => ({
      ...current,
      [toolCallId]: !current[toolCallId],
    }));
  }

  function applyChatRunEvent(event: ChatRunEvent) {
    rememberRunMessage(event.runId, event.messageId);

    if (event.kind === "started") {
      setActiveRunIds((current) => ({
        ...current,
        [event.chatId]: event.runId,
      }));
      clearMessagePartsForMessageIds([event.messageId]);
    }

    if (event.removedMessageIds && event.removedMessageIds.length > 0) {
      const removedMessageIds = event.removedMessageIds;
      const removed = new Set(removedMessageIds);
      clearToolExecutionsForMessageIds(removedMessageIds);
      clearMessagePartsForMessageIds(removedMessageIds);
      setMessages((current) =>
        current.filter((message) => !removed.has(message.id)),
      );
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
      appendMessageTextPart(event.messageId, event.delta);
    } else if (event.kind === "tool_call" && event.toolCallId) {
      appendMessageToolPart(event.messageId, event.toolCallId);
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
    for (const tool of Object.values(toolExecutions())) {
      if (tool.runId === runId) {
        appendMessageToolPart(messageId, tool.toolCallId);
      }
    }
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

  function clearMessagePartsForMessageIds(messageIds: string[]) {
    if (messageIds.length === 0) {
      return;
    }

    const removed = new Set(messageIds);
    setMessageParts((current) =>
      Object.fromEntries(
        Object.entries(current).filter(([messageId]) => !removed.has(messageId)),
      ),
    );
  }

  function appendMessageTextPart(messageId: string, delta: string) {
    if (!delta) {
      return;
    }

    setMessageParts((current) => {
      const parts = current[messageId] ?? [];
      const lastPart = parts[parts.length - 1];
      if (lastPart?.kind === "text") {
        return {
          ...current,
          [messageId]: [
            ...parts.slice(0, -1),
            {
              ...lastPart,
              text: `${lastPart.text ?? ""}${delta}`,
            },
          ],
        };
      }

      return {
        ...current,
        [messageId]: [
          ...parts,
          {
            id: `live-text:${messageId}:${liveMessagePartSequence++}`,
            kind: "text",
            messageId,
            text: delta,
            createdAt: Date.now(),
          },
        ],
      };
    });
  }

  function appendMessageToolPart(messageId: string, toolCallId: string) {
    setMessageParts((current) => {
      const parts = current[messageId] ?? [];
      if (
        parts.some(
          (part) => part.kind === "tool" && part.toolCallId === toolCallId,
        )
      ) {
        return current;
      }

      return {
        ...current,
        [messageId]: [
          ...parts,
          {
            id: `live-tool:${toolCallId}`,
            kind: "tool",
            messageId,
            toolCallId,
            createdAt: Date.now(),
          },
        ],
      };
    });
  }

  function applyToolExecutionEvent(event: ToolExecutionEvent) {
    const previous = toolExecutions()[event.toolCallId];
    const runId = event.runId ?? previous?.runId;
    const messageId =
      previous?.messageId ?? (runId ? runMessageIds()[runId] : undefined);
    if (messageId && messages().some((message) => message.id === messageId)) {
      appendMessageToolPart(messageId, event.toolCallId);
    }

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
          // Typed-tool fields: keep the newest non-null value, never regress to
          // undefined when a later event (e.g. an `output` chunk) omits them.
          toolKind: event.toolKind ?? previous?.toolKind,
          payload: event.payload ?? previous?.payload,
          touchedPaths: mergeTouchedPaths(
            previous?.touchedPaths,
            event.touchedPaths,
          ),
          artifacts: mergeToolArtifacts(previous?.artifacts, event.artifacts),
          createdAt: previous?.createdAt ?? now,
          updatedAt: now,
        },
      };
    });
  }

  function handleMessageScrollElement(element: HTMLDivElement | undefined) {
    messageScrollElement = element;
  }

  function saveActiveChatScroll() {
    const chatId = activeChatId();
    const element = messageScrollElement;
    if (!chatId || !element) {
      return;
    }

    const maxTop = Math.max(0, element.scrollHeight - element.clientHeight);
    chatScrollPositions.set(chatId, {
      top: Math.min(element.scrollTop, maxTop),
      fromBottom: Math.max(0, maxTop - element.scrollTop),
    });
  }

  function restoreChatScroll(chatId: string) {
    cancelAnimationFrame(restoreScrollFrame);

    const position = chatScrollPositions.get(chatId);
    let frame = 0;
    const apply = () => {
      const element = messageScrollElement;
      if (!element || activeChatId() !== chatId) {
        return;
      }

      const maxTop = Math.max(0, element.scrollHeight - element.clientHeight);
      element.scrollTop =
        !position || position.fromBottom <= CHAT_SCROLL_BOTTOM_THRESHOLD_PX
          ? maxTop
          : Math.min(position.top, maxTop);

      frame += 1;
      if (frame < CHAT_SCROLL_RESTORE_FRAMES) {
        restoreScrollFrame = requestAnimationFrame(apply);
      }
    };

    restoreScrollFrame = requestAnimationFrame(apply);
  }

  const visibleToolExecutions = createMemo(() => {
    const chatId = activeChatId();
    const runId = activeRunId();
    const messageIds = new Set(messages().map((message) => message.id));

    return Object.values(toolExecutions())
      .filter((tool) => {
        if (chatId && tool.chatId) {
          return tool.chatId === chatId;
        }

        if (tool.messageId) {
          return messageIds.has(tool.messageId);
        }

        return Boolean(runId && tool.runId === runId);
      })
      .sort((left, right) => right.updatedAt - left.updatedAt)
      .slice(0, 50);
  });

  const toolExecutionsByMessageId = createMemo(() => {
    const messageIds = new Set(messages().map((message) => message.id));
    const grouped: Record<string, ToolExecutionView[]> = {};

    for (const tool of Object.values(toolExecutions())) {
      const messageId = tool.messageId;
      if (!messageId || !messageIds.has(messageId)) {
        continue;
      }

      grouped[messageId] ??= [];
      grouped[messageId].push(tool);
    }

    for (const tools of Object.values(grouped)) {
      tools.sort(compareToolExecutions);
    }

    return grouped;
  });

  return (
    <main class="workspace-shell">
      <Sidebar
        activeChatId={activeChatId()}
        activeProjectId={activeProjectId()}
        chats={chats()}
        isLoadingChats={isLoadingChats()}
        isLoadingProjects={isLoadingProjects()}
        isOpeningProject={isOpeningProject()}
        onNewChat={handleNewChat}
        onOpenChat={(chatId) => void openChat(chatId)}
        onOpenProject={() => void handlePickProjectDirectory()}
        onOpenSettings={props.onOpenSettings}
        onSelectProject={(projectId) => void handleSelectProject(projectId)}
        projects={projects()}
      />
      <ConversationPane
        activeChat={activeChat()}
        activeProject={activeProject()}
        connectorSettings={connectorSettings()}
        draft={draft()}
        error={error()}
        editingDraft={editingDraft()}
        editingMessageId={editingMessageId()}
        isLoading={isLoadingMessages()}
        isSending={isSending() || isSubmittingEdit() || isChatRunning()}
        messages={messages()}
        runTransports={runTransports()}
        activeRunId={activeRunId()}
        reasoningOptionId={reasoningOptionId()}
        expandedInlineTools={expandedInlineTools()}
        messagePartsByMessageId={messageParts()}
        toolExecutionsByMessageId={toolExecutionsByMessageId()}
        onApproveTool={handleApproveTool}
        onMessageScrollElement={handleMessageScrollElement}
        onCancelRun={handleCancelRun}
        onCancelTool={handleCancelTool}
        onContinue={handleContinue}
        onBranchMessage={(message) => void handleBranchMessage(message)}
        onCancelEdit={handleCancelEdit}
        onDenyTool={handleDenyTool}
        onDraftChange={setDraft}
        onEditDraftChange={setEditingDraft}
        onRetry={handleRetry}
        onReasoningOptionChange={setReasoningOptionId}
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
        <SectionHeader
          title="Recent"
          action={
            <button class="icon-button icon-button--ghost" type="button" title="Search chats">
              <Search size={16} />
            </button>
          }
        />
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
        <button class="text-button" type="button" onClick={props.onNewChat}>
          Start chat
          <ChevronDown size={14} />
        </button>
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

function ConversationPane(props: {
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
  runTransports: Record<string, string>;
  toolExecutionsByMessageId: Record<string, ToolExecutionView[]>;
  onApproveTool: (toolCallId: string) => void;
  onBranchMessage: (message: ChatMessage) => void;
  onCancelEdit: () => void;
  onCancelRun: () => void;
  onCancelTool: (toolCallId: string) => void;
  onContinue: () => void;
  onDenyTool: (toolCallId: string) => void;
  onDraftChange: (value: string) => void;
  onEditDraftChange: (value: string) => void;
  onMessageScrollElement: (element: HTMLDivElement | undefined) => void;
  onReasoningOptionChange: (optionId: ReasoningOptionId) => void;
  onRetry: () => void;
  onSelectModel: (providerId: string, modelId: string) => void;
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
          <button
            class="icon-button icon-button--ghost"
            type="button"
            title="Rename chat"
          >
            <FileText size={15} />
          </button>
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
        adjustScrollOnItemResize={false}
        ariaLabel="Chat messages"
        class="message-list"
        empty={
          <ConversationState
            error={props.error}
            hasProject={Boolean(props.activeProject)}
            isLoading={props.isLoading}
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
          const message = createMemo(() => messagesById()[item.messageId]);
          return (
            <MessageRow
              message={message()}
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
              onEditDraftChange={props.onEditDraftChange}
              onRetry={props.onRetry}
              settings={props.connectorSettings}
              onStartEdit={props.onStartEdit}
              onSubmitEdit={props.onSubmitEdit}
              onApproveTool={props.onApproveTool}
              onToggleInlineTool={props.onToggleInlineTool}
              tools={props.toolExecutionsByMessageId[item.messageId] ?? []}
              toolsById={toolsById()}
            />
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
        onCancelRun={props.onCancelRun}
        onDraftChange={props.onDraftChange}
        onReasoningOptionChange={props.onReasoningOptionChange}
        onSend={props.onSendMessage}
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
}) {
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
          <div class="message-skeleton message-skeleton--assistant" />
          <div class="message-skeleton message-skeleton--user" />
          <div class="message-skeleton message-skeleton--assistant message-skeleton--short" />
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

function ModelSelector(props: {
  onSelectModel: (providerId: string, modelId: string) => void;
  selected?: { providerId?: string | null; modelId?: string | null };
  settings?: ConnectorSettingsSnapshot;
}) {
  const activeProvider = () =>
    selectableConnectorProviderFor(
      props.settings,
      props.selected?.providerId ?? props.settings?.selectedModel.providerId,
    );
  const models = () => activeProvider()?.models ?? [];
  const isRefreshing = () => {
    const provider = activeProvider();
    const providers = props.settings?.providers;

    if (!provider) {
      return providers?.some(
        (item) =>
          item.refreshStatus === "pending" ||
          item.refreshStatus === "refreshing",
      ) ?? true;
    }

    return (
      provider.refreshStatus === "pending" ||
      provider.refreshStatus === "refreshing"
    );
  };
  const hasConnectorError = () => {
    const provider = activeProvider();
    if (provider) {
      return Boolean(provider.modelError && provider.models.length === 0);
    }

    return (
      props.settings?.providers.some(
        (item) => item.modelError && item.models.length === 0,
      ) ?? false
    );
  };
  const selectedValue = () => {
    const providerId =
      props.selected?.providerId ?? props.settings?.selectedModel.providerId;
    const modelId =
      props.selected?.modelId ?? props.settings?.selectedModel.modelId;
    return providerId && modelId
      ? modelOptionValue(providerId, modelId)
      : "";
  };
  const placeholder = () =>
    isRefreshing()
      ? "Loading models..."
      : hasConnectorError()
        ? "Connector unavailable"
        : activeProvider()
          ? "No models for provider"
          : "No models connected";
  const options = createMemo<SearchSelectOption[]>(() =>
    withSelectedCustomModelOption(
      models().map((model) => ({
        detail: model.id === model.label ? model.providerLabel : model.id,
        label: model.label,
        searchText: `${model.providerLabel} ${model.label} ${model.id}`,
        value: modelOptionValue(model.providerId, model.id),
      })),
      activeProvider(),
      props.selected?.modelId ?? props.settings?.selectedModel.modelId,
    ),
  );

  return (
    <SearchSelect
      ariaLabel="Active model"
      class="model-search-select"
      emptyLabel={placeholder()}
      options={options()}
      placeholder={placeholder()}
      value={selectedValue()}
      createOption={(query) => customModelOption(activeProvider(), models(), query)}
      onSelect={(value) => {
        const [providerId, modelId] = parseModelOptionValue(value);
        if (providerId && modelId) {
          props.onSelectModel(providerId, modelId);
        }
      }}
    />
  );
}

function ProviderStatusDot(props: { provider?: ConnectorProviderSummary }) {
  const status = () => providerStatusSummary(props.provider);

  return (
    <span
      class={`provider-status-dot provider-status-dot--${status().tone}`}
      title={status().tooltip}
      aria-label={status().label}
    />
  );
}

function ProviderSelector(props: {
  onSelectModel: (providerId: string, modelId: string) => void;
  selected?: { providerId?: string | null; modelId?: string | null };
  settings?: ConnectorSettingsSnapshot;
}) {
  const providers = () =>
    (props.settings?.providers ?? []).filter(isProviderSelectableForChat);
  const currentProviderId = () =>
    props.selected?.providerId ?? props.settings?.selectedModel.providerId;
  const selectedProviderId = () => currentProviderId() ?? "";
  const activeProvider = () =>
    providers().find((provider) => provider.id === currentProviderId());
  const status = () => providerStatusSummary(activeProvider());
  const options = createMemo<SearchSelectOption[]>(() =>
    providers().map((provider) => {
      const providerStatus = providerStatusSummary(provider);
      const modelId = selectableProviderModelId(provider);
      const modelCount = provider.models.length;

      return {
        detail: modelId
          ? `${modelCount} ${modelCount === 1 ? "model" : "models"}`
          : providerStatus.tooltip,
        disabled: !modelId,
        label: provider.label,
        searchText: [
          provider.label,
          providerStatus.label,
          providerStatus.tooltip,
          ...provider.models.map((model) => `${model.label} ${model.id}`),
        ].join(" "),
        status: {
          label: providerStatusBadgeLabel(providerStatus),
          tone: providerStatus.tone,
        },
        title: providerStatus.tooltip,
        value: provider.id,
      };
    }),
  );

  return (
    <SearchSelect
      ariaLabel="Active provider"
      class="provider-search-select"
      emptyLabel="No providers"
      options={options()}
      placeholder="No provider"
      title={status().tooltip}
      value={selectedProviderId()}
      onSelect={(providerId) => {
        const provider = providers().find((item) => item.id === providerId);
        const modelId = provider ? selectableProviderModelId(provider) : undefined;
        if (provider && modelId) {
          props.onSelectModel(provider.id, modelId);
        }
      }}
    />
  );
}

interface SearchSelectOption {
  detail?: string;
  disabled?: boolean;
  label: string;
  searchText?: string;
  status?: {
    label: string;
    tone: ProviderStatusTone;
  };
  title?: string;
  value: string;
}

function SearchSelect(props: {
  ariaLabel: string;
  class?: string;
  emptyLabel: string;
  options: SearchSelectOption[];
  placeholder: string;
  title?: string;
  value?: string;
  createOption?: (query: string) => SearchSelectOption | undefined;
  onSelect: (value: string) => void;
}) {
  const [isOpen, setIsOpen] = createSignal(false);
  const [query, setQuery] = createSignal("");
  const [activeIndex, setActiveIndex] = createSignal(-1);
  let rootRef: HTMLDivElement | undefined;
  let inputRef: HTMLInputElement | undefined;

  const selectedOption = () =>
    props.options.find((option) => option.value === props.value);
  const emptyLabel = () =>
    query().trim().length > 0 ? "No matches" : props.emptyLabel;
  const filteredOptions = createMemo(() => {
    const normalizedQuery = normalizeSearchQuery(query());
    const filtered = normalizedQuery
      ? props.options.filter((option) =>
          normalizeSearchQuery(
            [option.label, option.detail, option.searchText, option.status?.label]
              .filter(Boolean)
              .join(" "),
          ).includes(normalizedQuery),
        )
      : props.options;
    const created = normalizedQuery ? props.createOption?.(query().trim()) : undefined;
    if (!created || filtered.some((option) => option.value === created.value)) {
      return filtered;
    }
    return [...filtered, created];
  });

  createEffect(() => {
    if (!isOpen()) {
      return;
    }

    const options = filteredOptions();
    const current = activeIndex();
    if (current >= 0 && current < options.length && !options[current]?.disabled) {
      return;
    }

    setActiveIndex(firstSelectableOptionIndex(options));
  });

  onMount(() => {
    const handlePointerDown = (event: PointerEvent) => {
      if (!isOpen() || !rootRef) {
        return;
      }

      if (event.target instanceof Node && !rootRef.contains(event.target)) {
        closeDropdown();
      }
    };

    document.addEventListener("pointerdown", handlePointerDown);
    onCleanup(() => {
      document.removeEventListener("pointerdown", handlePointerDown);
    });
  });

  const openDropdown = () => {
    setIsOpen(true);
    setQuery("");
    setActiveIndex(firstSelectableOptionIndex(filteredOptions()));
    window.setTimeout(() => inputRef?.focus(), 0);
  };
  const closeDropdown = () => {
    setIsOpen(false);
    setQuery("");
    setActiveIndex(-1);
  };
  const toggleDropdown = () => {
    if (isOpen()) {
      closeDropdown();
    } else {
      openDropdown();
    }
  };
  const selectOption = (option: SearchSelectOption) => {
    if (option.disabled) {
      return;
    }

    props.onSelect(option.value);
    closeDropdown();
  };
  const moveActiveOption = (delta: number) => {
    const options = filteredOptions();
    if (options.length === 0) {
      setActiveIndex(-1);
      return;
    }

    let nextIndex = activeIndex();
    for (let attempts = 0; attempts < options.length; attempts += 1) {
      nextIndex = (nextIndex + delta + options.length) % options.length;
      if (!options[nextIndex]?.disabled) {
        setActiveIndex(nextIndex);
        return;
      }
    }

    setActiveIndex(-1);
  };
  const selectActiveOption = () => {
    const option = filteredOptions()[activeIndex()];
    if (option) {
      selectOption(option);
    }
  };
  const handleTriggerKeyDown = (event: KeyboardEvent) => {
    if (event.key === "ArrowDown" || event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      openDropdown();
    }
  };
  const handleSearchKeyDown = (event: KeyboardEvent) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      moveActiveOption(1);
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      moveActiveOption(-1);
    }
    if (event.key === "Enter") {
      event.preventDefault();
      selectActiveOption();
    }
    if (event.key === "Escape") {
      event.preventDefault();
      closeDropdown();
    }
  };

  return (
    <div class={`search-select ${props.class ?? ""}`} ref={rootRef}>
      <button
        class="search-select__trigger"
        type="button"
        aria-expanded={isOpen()}
        aria-haspopup="listbox"
        aria-label={props.ariaLabel}
        title={props.title ?? selectedOption()?.label ?? props.placeholder}
        onClick={toggleDropdown}
        onKeyDown={handleTriggerKeyDown}
      >
        <span class="search-select__value">
          {selectedOption()?.label ?? props.placeholder}
        </span>
        <ChevronDown
          classList={{
            "search-select__chevron": true,
            "search-select__chevron--open": isOpen(),
          }}
          size={14}
        />
      </button>

      <Show when={isOpen()}>
        <div class="search-select__popover">
          <label class="search-select__search">
            <Search size={13} />
            <input
              ref={inputRef}
              aria-label={`Search ${props.ariaLabel.toLowerCase()}`}
              autocomplete="off"
              spellcheck={false}
              placeholder="Search..."
              value={query()}
              onInput={(event) => setQuery(event.currentTarget.value)}
              onKeyDown={handleSearchKeyDown}
            />
          </label>

          <div class="search-select__list" role="listbox">
            <For
              each={filteredOptions()}
              fallback={<div class="search-select__empty">{emptyLabel()}</div>}
            >
              {(option, index) => (
                <button
                  classList={{
                    "search-select__option": true,
                    "search-select__option--active": index() === activeIndex(),
                    "search-select__option--selected": option.value === props.value,
                  }}
                  type="button"
                  role="option"
                  aria-selected={option.value === props.value}
                  disabled={option.disabled}
                  title={option.title}
                  onMouseEnter={() => {
                    if (!option.disabled) {
                      setActiveIndex(index());
                    }
                  }}
                  onClick={() => selectOption(option)}
                >
                  <span class="search-select__option-text">
                    <span class="search-select__option-label">{option.label}</span>
                    <Show when={option.detail}>
                      <span class="search-select__option-detail">
                        {option.detail}
                      </span>
                    </Show>
                  </span>
                  <Show when={option.status}>
                    <span
                      class={`search-select__option-status search-select__option-status--${option.status!.tone}`}
                    >
                      {option.status!.label}
                    </span>
                  </Show>
                  <Show when={option.value === props.value}>
                    <Check class="search-select__check" size={14} />
                  </Show>
                </button>
              )}
            </For>
          </div>
        </div>
      </Show>
    </div>
  );
}

function firstSelectableOptionIndex(options: SearchSelectOption[]) {
  return options.findIndex((option) => !option.disabled);
}

function normalizeSearchQuery(value: string) {
  return value.trim().toLowerCase();
}

function providerStatusBadgeLabel(status: ProviderStatusSummary) {
  const labels: Record<ProviderStatusTone, string> = {
    error: "Error",
    idle: "Setup",
    loading: "Loading",
    ready: "Ready",
    warmup: "Warmup",
  };

  return labels[status.tone];
}

function modelOptionValue(providerId: string, modelId: string) {
  return JSON.stringify([providerId, modelId]);
}

function customModelOption(
  provider: ConnectorProviderSummary | undefined,
  models: LlmModel[],
  query: string,
): SearchSelectOption | undefined {
  if (!provider || !providerAcceptsCustomModelIds(provider)) {
    return undefined;
  }

  const modelId = normalizeCustomModelId(query);
  if (!modelId || models.some((model) => model.id === modelId)) {
    return undefined;
  }
  if (providerVisibilityModelIds(provider).has(modelId)) {
    return undefined;
  }

  return {
    detail: "Custom model id",
    label: `Use ${modelId}`,
    searchText: modelId,
    value: modelOptionValue(provider.id, modelId),
  };
}

function withSelectedCustomModelOption(
  options: SearchSelectOption[],
  provider: ConnectorProviderSummary | undefined,
  selectedModelId: string | null | undefined,
) {
  if (
    !provider ||
    !providerAcceptsCustomModelIds(provider) ||
    !selectedModelId ||
    providerVisibilityModelIds(provider).has(selectedModelId) ||
    options.some((option) => option.value === modelOptionValue(provider.id, selectedModelId))
  ) {
    return options;
  }

  return [
    ...options,
    {
      detail: "Custom model id",
      label: selectedModelId,
      searchText: selectedModelId,
      value: modelOptionValue(provider.id, selectedModelId),
    },
  ];
}

function normalizeCustomModelId(value: string) {
  const modelId = value.trim();
  if (
    modelId.length === 0 ||
    modelId.length > 256 ||
    /[\s\x00-\x1f\x7f]/.test(modelId)
  ) {
    return "";
  }
  return modelId;
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

type ProviderStatusTone = "ready" | "warmup" | "loading" | "error" | "idle";

interface ProviderStatusSummary {
  label: string;
  tone: ProviderStatusTone;
  tooltip: string;
}

function connectorProviderFor(
  settings: ConnectorSettingsSnapshot | undefined,
  providerId: string | null | undefined,
) {
  if (!providerId) {
    return undefined;
  }
  return settings?.providers.find((provider) => provider.id === providerId);
}

function selectableConnectorProviderFor(
  settings: ConnectorSettingsSnapshot | undefined,
  providerId: string | null | undefined,
) {
  const provider = connectorProviderFor(settings, providerId);
  return provider && isProviderSelectableForChat(provider) ? provider : undefined;
}

function selectableConnectorModelFor(
  settings: ConnectorSettingsSnapshot | undefined,
  providerId: string | null | undefined,
  modelId: string | null | undefined,
) {
  const provider = selectableConnectorProviderFor(settings, providerId);
  if (!provider || !modelId) {
    return undefined;
  }

  return (
    provider.models.find((model) => model.id === modelId) ??
    customConnectorModelFor(provider, modelId)
  );
}

function customConnectorModelFor(
  provider: ConnectorProviderSummary,
  modelId: string,
): LlmModel | undefined {
  if (!providerAcceptsCustomModelIds(provider) || !normalizeCustomModelId(modelId)) {
    return undefined;
  }
  if (providerVisibilityModelIds(provider).has(modelId)) {
    return undefined;
  }

  const template = customModelCapabilityTemplate(provider, modelId);
  return {
    providerId: provider.id,
    providerLabel: provider.label,
    id: modelId,
    label: modelId,
    family: template?.family ?? "Custom",
    description: "Custom model id",
    capabilities: template?.capabilities ? [...template.capabilities] : ["text"],
    reasoning: template?.reasoning,
    recommended: false,
  };
}

function customModelCapabilityTemplate(
  provider: ConnectorProviderSummary,
  modelId: string,
) {
  const targetTokens = significantModelTokens(modelId);
  if (targetTokens.size === 0) {
    return undefined;
  }

  let bestModel: LlmModel | undefined;
  let bestScore = 0;
  for (const model of provider.models) {
    const modelTokens = significantModelTokens(
      `${model.id} ${model.label} ${model.family}`,
    );
    let score = 0;
    for (const token of targetTokens) {
      if (modelTokens.has(token)) {
        score += 1;
      }
    }
    if (score > bestScore) {
      bestScore = score;
      bestModel = model;
    }
  }

  return bestScore > 0 ? bestModel : undefined;
}

function significantModelTokens(value: string) {
  const ignored = new Set([
    "claude",
    "model",
    "default",
    "recommended",
    "context",
    "with",
    "custom",
  ]);
  return new Set(
    value
      .toLowerCase()
      .split(/[^a-z0-9]+/)
      .map((token) => token.trim())
      .filter((token) => token.length >= 3 && !ignored.has(token)),
  );
}

interface ReasoningSelectorOption {
  label: string;
  option: ReasoningOption;
  title: string;
  value: ReasoningOptionId;
}

function reasoningSelectorOptions(model?: LlmModel): ReasoningSelectorOption[] {
  const reasoning = model?.reasoning;
  if (!reasoning?.supported) {
    return [];
  }

  return (reasoning.options ?? [])
    .filter((option) => option.id.trim().length > 0)
    .map((option) => {
      const label = reasoningOptionLabel(option);
      return {
        label,
        option,
        title: reasoningOptionTitle(option, label),
        value: option.id,
      };
    });
}

function coerceReasoningOptionId(
  optionId: ReasoningOptionId | undefined,
  model?: LlmModel,
): ReasoningOptionId | undefined {
  const options = reasoningSelectorOptions(model);
  if (options.length === 0) {
    return undefined;
  }

  if (optionId && options.some((option) => option.value === optionId)) {
    return optionId;
  }

  return options.find((option) => option.option.recommended)?.value ?? options[0].value;
}

function reasoningConfigForOption(
  optionId: ReasoningOptionId | undefined,
  model?: LlmModel,
): ReasoningConfig | null {
  const selected = reasoningSelectorOptions(model).find(
    (option) => option.value === optionId,
  );
  if (!selected || isReasoningConfigEmpty(selected.option.config)) {
    return null;
  }

  return selected.option.config;
}

function reasoningOptionLabel(option: ReasoningOption) {
  const id = option.id.trim().toLowerCase();
  const effort = option.config?.effort?.trim().toLowerCase();

  switch (id || effort) {
    case "auto":
      return "Авто";
    case "none":
      return "Отключено";
    case "minimal":
      return "Минимальный";
    case "low":
      return "Низкий";
    case "medium":
      return "Средний";
    case "high":
      return "Высокий";
    case "xhigh":
      return "Очень высокий";
    case "max":
      return "Максимальный";
    default:
      return option.label.trim() || option.id;
  }
}

function reasoningOptionTitle(option: ReasoningOption, label: string) {
  const description = option.description?.trim();
  if (description) {
    return description;
  }

  const id = option.id.trim().toLowerCase();
  if (id === "auto") {
    return "Дефолтный режим выбранной модели";
  }
  if (id === "none") {
    return "Не отправлять настройку рассуждения";
  }

  return `${label} уровень рассуждения`;
}

function isReasoningConfigEmpty(config?: ReasoningConfig | null) {
  return !config?.effort && !config?.budgetTokens && !config?.summary;
}

function selectableProviderModelId(provider: ConnectorProviderSummary) {
  const selectedModelId = provider.selectedModelId;
  if (selectedModelId) {
    if (provider.models.some((model) => model.id === selectedModelId)) {
      return selectedModelId;
    }
    if (
      providerAcceptsCustomModelIds(provider) &&
      !providerVisibilityModelIds(provider).has(selectedModelId)
    ) {
      return selectedModelId;
    }
  }

  return provider.models[0]?.id;
}

function isProviderSelectableForChat(provider: ConnectorProviderSummary) {
  if (requiresInteractiveAuth(provider)) {
    return false;
  }

  if (missingRequiredAdapterSettings(provider).length > 0) {
    return false;
  }

  return Boolean(selectableProviderModelId(provider));
}

function providerAcceptsCustomModelIds(provider: ConnectorProviderSummary | undefined) {
  return Boolean(provider?.settingsSchema.modelManagement.acceptsCustomModelIds);
}

function providerVisibilityModelIds(provider: ConnectorProviderSummary | undefined) {
  const fields = provider?.adapterSettings?.fields ?? [];
  return new Set(
    fields
      .filter((field) => field.kind === "model_visibility_list")
      .flatMap((field) => field.options.map((option) => option.value)),
  );
}

function providerStatusSummary(
  provider: ConnectorProviderSummary | undefined,
): ProviderStatusSummary {
  if (!provider) {
    return {
      label: "No provider",
      tone: "idle",
      tooltip: "No model provider is selected.",
    };
  }

  const name = provider.label;
  if (provider.refreshStatus === "failed") {
    return {
      label: "Provider failed",
      tone: "error",
      tooltip: provider.modelError
        ? `${name}: ${provider.modelError}`
        : `${name}: provider refresh failed.`,
    };
  }

  if (provider.refreshStatus === "pending") {
    return {
      label: "Provider pending",
      tone: "loading",
      tooltip: `${name}: provider catalog has not loaded yet.`,
    };
  }

  if (provider.refreshStatus === "refreshing") {
    return {
      label: "Provider loading",
      tone: "loading",
      tooltip: `${name}: loading provider catalog and settings.`,
    };
  }

  if (requiresInteractiveAuth(provider)) {
    return {
      label: "Provider needs auth",
      tone: "warmup",
      tooltip: `${name}: authorization is required before chat.`,
    };
  }

  const missingSettings = missingRequiredAdapterSettings(provider);
  if (missingSettings.length > 0) {
    return {
      label: "Provider needs settings",
      tone: "warmup",
      tooltip: `${name}: required settings missing (${missingSettings.join(", ")}).`,
    };
  }

  if (provider.models.length === 0) {
    return {
      label: "Provider needs model",
      tone: "warmup",
      tooltip: `${name}: no chat model is available yet.`,
    };
  }

  if (!provider.runtimeReady) {
    return {
      label: "Provider needs warmup",
      tone: "warmup",
      tooltip: `${name}: catalog is ready, adapter will warm up on the next request.`,
    };
  }

  return {
    label: "Provider ready",
    tone: "ready",
    tooltip: `${name}: adapter is loaded and ready.`,
  };
}

function requiresInteractiveAuth(provider: ConnectorProviderSummary) {
  return (
    (provider.authKind === "oauth_internal" ||
      provider.authKind === "external_process") &&
    !provider.authenticated
  );
}

function missingRequiredAdapterSettings(provider: ConnectorProviderSummary) {
  const settings = provider.adapterSettings;
  if (!settings) {
    return [];
  }

  return settings.fields
    .filter((field) => field.required)
    .filter((field) => {
      if (field.kind === "secret") {
        return !settings.secrets[field.key]?.hasValue;
      }

      if (field.kind === "bool") {
        return false;
      }

      return !settings.values[field.key]?.trim();
    })
    .map((field) => field.label);
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
      <div
        class="inspector-tabs"
        role="tablist"
        aria-label="Inspector tabs"
        onMouseDown={startWindowDrag}
      >
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
  const command = () => formatToolCommand(props.tool);
  const headline = () => formatToolHeadline(props.tool);
  const canApprove = () => props.tool.kind === "permission_requested";
  const canCancel = () =>
    !canApprove() && !isTerminalToolKind(props.tool.kind);

  return (
    <div class="tool-call-row">
      <Terminal size={16} />
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

// A small, kind-specific glyph for a tool-call spoiler row (the terminal icon
// doubles as the "ran" verb for run_command).
function ToolKindIcon(props: { kind?: ToolKind }) {
  return (
    <Switch fallback={<Terminal size={14} />}>
      <Match when={props.kind === "list_files"}>
        <Folder size={14} />
      </Match>
      <Match when={props.kind === "search_text"}>
        <Search size={14} />
      </Match>
      <Match when={props.kind === "apply_patch"}>
        <GitBranch size={14} />
      </Match>
      <Match
        when={
          props.kind === "read_file" ||
          props.kind === "write_file" ||
          props.kind === "edit_file"
        }
      >
        <FileText size={14} />
      </Match>
    </Switch>
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
  const command = () => formatToolCommand(props.tool);
  const output = () => formatToolOutput(props.tool);
  const outputLineCount = () => countTextLines(output());
  const isOutputScrollable = () =>
    outputLineCount() > TOOL_OUTPUT_MAX_VISIBLE_LINES;
  const canApprove = () => props.tool.kind === "permission_requested";
  const canCancel = () =>
    !canApprove() && !isTerminalToolKind(props.tool.kind);
  // Prefer the typed semantic card whenever the backend supplied both a tool
  // kind and a payload; otherwise fall back to the generic text rendering.
  const hasSemanticCard = () =>
    Boolean(props.tool.toolKind && props.tool.payload);

  return (
    <div class="inline-tool-call">
      <button
        class="inline-tool-call__summary"
        type="button"
        aria-expanded={props.expanded}
        onClick={props.onToggle}
      >
        <span class="inline-tool-call__icon">
          <ToolKindIcon kind={props.tool.toolKind} />
        </span>
        <span
          class="inline-tool-call__title"
          title={formatToolHeadline(props.tool)}
        >
          {formatToolHeadline(props.tool)}
        </span>
        {/* A successful "Completed" is implied by the row; only surface a status
            that actually needs attention (running, denied, failed, cancelled). */}
        <Show when={props.tool.kind !== "completed"}>
          <span class={`tool-status tool-status--${toolTone(props.tool.kind)}`}>
            {toolStatusLabel(props.tool.kind)}
          </span>
        </Show>
        <ChevronDown
          classList={{
            "inline-tool-call__chevron": true,
            "inline-tool-call__chevron--open": props.expanded,
          }}
          size={14}
        />
      </button>

      <Show when={props.expanded}>
        <div class="inline-tool-call__body">
          <Show
            when={hasSemanticCard()}
            fallback={
              <>
                <Show when={command()}>
                  <pre class="inline-tool-call__command">{command()}</pre>
                </Show>

                <Show when={!canApprove() && props.tool.message}>
                  <p class="inline-tool-call__message">{props.tool.message}</p>
                </Show>

                <Show when={shouldShowInlineToolOutput(props.tool, output())}>
                  <pre
                    classList={{
                      "inline-tool-call__output": true,
                      "inline-tool-call__output--scrollable":
                        isOutputScrollable(),
                    }}
                    style={`--tool-output-lines: ${TOOL_OUTPUT_MAX_VISIBLE_LINES}`}
                  >
                    {output()}
                  </pre>
                </Show>
              </>
            }
          >
            <ToolCard tool={props.tool} />
          </Show>

          {/* Show the pending change prominently before the human approves it,
              in both the semantic and fallback paths. */}
          <Show when={canApprove() && props.tool.message}>
            {(message) => (
              <ApprovalPreview
                message={message()}
                artifacts={props.tool.artifacts}
              />
            )}
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
                  <Square size={13} />
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

function MessageRow(props: {
  message: ChatMessage;
  transport?: string;
  expandedInlineTools: Record<string, boolean>;
  editingDraft: string;
  isEditing: boolean;
  isBusy?: boolean;
  parts: MessagePartView[];
  tools: ToolExecutionView[];
  toolsById: Record<string, ToolExecutionView>;
  onApproveTool: (toolCallId: string) => void;
  onBranchMessage: (message: ChatMessage) => void;
  onCancelEdit: () => void;
  onCancelTool: (toolCallId: string) => void;
  onContinue?: () => void;
  onDenyTool: (toolCallId: string) => void;
  onEditDraftChange: (value: string) => void;
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
                fallback={<MessageMarkdown content={body()} />}
              >
                <MessageParts
                  expandedInlineTools={props.expandedInlineTools}
                  parts={assistantParts()}
                  onApproveTool={props.onApproveTool}
                  onCancelTool={props.onCancelTool}
                  onDenyTool={props.onDenyTool}
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

function MessageParts(props: {
  expandedInlineTools: Record<string, boolean>;
  parts: RenderableMessagePart[];
  onApproveTool: (toolCallId: string) => void;
  onCancelTool: (toolCallId: string) => void;
  onDenyTool: (toolCallId: string) => void;
  onToggleTool: (toolCallId: string) => void;
}) {
  return (
    <div class="message-parts">
      <For each={props.parts}>
        {(part) => (
          <Show
            when={part.kind === "tool"}
            fallback={
              <div class="message-part message-part--text">
                <MessageMarkdown content={part.text ?? ""} />
              </div>
            }
          >
            <div class="message-part message-part--tool">
              <Show
                when={part.tool}
                fallback={
                  <div class="inline-tool-call inline-tool-call--pending">
                    <div class="inline-tool-call__summary">
                      <ChevronDown
                        class="inline-tool-call__chevron"
                        size={14}
                      />
                      <Terminal size={15} />
                      <span class="inline-tool-call__title">Tool call</span>
                      <span class="tool-status tool-status--pending">
                        Queued
                      </span>
                    </div>
                  </div>
                }
              >
                {(tool) => (
                  <InlineToolCall
                    expanded={Boolean(
                      props.expandedInlineTools[tool().toolCallId],
                    )}
                    tool={tool()}
                    onApprove={() => props.onApproveTool(tool().toolCallId)}
                    onCancel={() => props.onCancelTool(tool().toolCallId)}
                    onDeny={() => props.onDenyTool(tool().toolCallId)}
                    onToggle={() => props.onToggleTool(tool().toolCallId)}
                  />
                )}
              </Show>
            </div>
          </Show>
        )}
      </For>
    </div>
  );
}

function MessageMarkdown(props: { content: string }) {
  return (
    <div class="message-md">
      <SolidMarkdown
        renderingStrategy="reconcile"
        remarkPlugins={[remarkGfm]}
        children={props.content}
      />
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

function Composer(props: {
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
}) {
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
          // Enter sends; Shift+Enter inserts a newline. (`isComposing` guards IME.)
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
      {props.action}
    </div>
  );
}

function ReasoningSelector(props: {
  disabled: boolean;
  model?: LlmModel;
  value?: ReasoningOptionId;
  onChange: (optionId: ReasoningOptionId) => void;
}) {
  const [isOpen, setIsOpen] = createSignal(false);
  const [activeIndex, setActiveIndex] = createSignal(-1);
  let rootRef: HTMLDivElement | undefined;

  const options = createMemo(() => reasoningSelectorOptions(props.model));
  const supported = () => options().length > 0;
  const selectedOption = () =>
    options().find((option) => option.value === props.value) ??
    options().find((option) => option.option.recommended) ??
    options()[0];

  createEffect(() => {
    if (!isOpen()) {
      return;
    }

    const currentOptions = options();
    const current = activeIndex();
    if (current >= 0 && current < currentOptions.length) {
      return;
    }

    setActiveIndex(firstSelectableReasoningOptionIndex(currentOptions));
  });

  onMount(() => {
    const handlePointerDown = (event: PointerEvent) => {
      if (!isOpen() || !rootRef) {
        return;
      }

      if (event.target instanceof Node && !rootRef.contains(event.target)) {
        closeMenu();
      }
    };

    document.addEventListener("pointerdown", handlePointerDown);
    onCleanup(() => {
      document.removeEventListener("pointerdown", handlePointerDown);
    });
  });

  const closeMenu = () => {
    setIsOpen(false);
    setActiveIndex(-1);
  };
  const openMenu = () => {
    if (props.disabled || !supported()) {
      return;
    }

    setIsOpen(true);
    setActiveIndex(firstSelectableReasoningOptionIndex(options()));
  };
  const toggleMenu = () => {
    if (isOpen()) {
      closeMenu();
    } else {
      openMenu();
    }
  };
  const selectOption = (option: ReasoningSelectorOption) => {
    props.onChange(option.value);
    closeMenu();
  };
  const moveActiveOption = (delta: number) => {
    const currentOptions = options();
    if (currentOptions.length === 0) {
      setActiveIndex(-1);
      return;
    }

    let nextIndex = activeIndex();
    for (let attempts = 0; attempts < currentOptions.length; attempts += 1) {
      nextIndex =
        (nextIndex + delta + currentOptions.length) % currentOptions.length;
      if (currentOptions[nextIndex]) {
        setActiveIndex(nextIndex);
        return;
      }
    }

    setActiveIndex(-1);
  };
  const selectActiveOption = () => {
    const option = options()[activeIndex()];
    if (option) {
      selectOption(option);
    }
  };
  const handleTriggerKeyDown = (event: KeyboardEvent) => {
    if (!isOpen()) {
      if (
        event.key === "ArrowDown" ||
        event.key === "Enter" ||
        event.key === " "
      ) {
        event.preventDefault();
        openMenu();
      }
      return;
    }

    if (event.key === "ArrowDown") {
      event.preventDefault();
      moveActiveOption(1);
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      moveActiveOption(-1);
    }
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      selectActiveOption();
    }
    if (event.key === "Escape") {
      event.preventDefault();
      closeMenu();
    }
  };

  return (
    <div
      ref={rootRef}
      classList={{
        "reasoning-selector": true,
        "reasoning-selector--disabled": !supported(),
      }}
    >
      <button
        class="reasoning-selector__trigger"
        type="button"
        aria-expanded={isOpen()}
        aria-haspopup="menu"
        disabled={props.disabled || !supported()}
        title={
          supported()
            ? "Рассуждение для следующего сообщения"
            : "Выбранная модель не поддерживает настройку рассуждения"
        }
        onClick={toggleMenu}
        onKeyDown={handleTriggerKeyDown}
      >
        <BrainCircuit size={15} />
        <span>{selectedOption()?.label ?? "Рассуждение"}</span>
        <ChevronDown
          classList={{
            "reasoning-selector__chevron": true,
            "reasoning-selector__chevron--open": isOpen(),
          }}
          size={13}
        />
      </button>

      <Show when={isOpen()}>
        <div class="reasoning-selector__popover" role="menu">
          <div class="reasoning-selector__heading">Рассуждение</div>
          <For each={options()}>
            {(option, index) => (
              <button
                classList={{
                  "reasoning-selector__item": true,
                  "reasoning-selector__item--active": index() === activeIndex(),
                  "reasoning-selector__item--selected":
                    option.value === selectedOption()?.value,
                }}
                type="button"
                role="menuitemradio"
                aria-checked={option.value === selectedOption()?.value}
                title={option.title}
                onMouseEnter={() => setActiveIndex(index())}
                onClick={() => selectOption(option)}
              >
                <span>{option.label}</span>
                <Show when={option.value === selectedOption()?.value}>
                  <Check size={14} />
                </Show>
              </button>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

function firstSelectableReasoningOptionIndex(options: ReasoningSelectorOption[]) {
  return options.length > 0 ? 0 : -1;
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
      <span class="branch-dot branch-dot--green" />
      <span>{props.project.chatCount}</span>
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

function removedMessageIdsForRetry(
  result: SendChatMessageResult,
  current: ChatMessage[],
): string[] {
  if (result.removedMessageIds && result.removedMessageIds.length > 0) {
    return result.removedMessageIds;
  }

  return current
    .filter(
      (message) =>
        message.chatId === result.chat.id &&
        message.position > result.userMessage.position,
    )
    .map((message) => message.id);
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

function messagePartsByMessageId(
  parts: ChatMessagePart[],
): Record<string, MessagePartView[]> {
  const grouped: Record<string, MessagePartView[]> = {};

  for (const part of parts) {
    grouped[part.messageId] ??= [];
    grouped[part.messageId].push(messagePartViewFromApi(part));
  }

  for (const messageParts of Object.values(grouped)) {
    messageParts.sort(compareMessageParts);
  }

  return grouped;
}

function messagePartViewFromApi(part: ChatMessagePart): MessagePartView {
  return {
    id: `part:${part.id}`,
    kind: part.kind,
    messageId: part.messageId,
    text: part.text ?? undefined,
    toolCallId: part.toolCallId ?? undefined,
    createdAt: timestampToMillis(part.createdAt),
    sequence: part.id,
  };
}

function compareMessageParts(left: MessagePartView, right: MessagePartView) {
  if (left.sequence !== undefined && right.sequence !== undefined) {
    return left.sequence - right.sequence;
  }

  if (left.createdAt !== right.createdAt) {
    return left.createdAt - right.createdAt;
  }

  return left.id.localeCompare(right.id);
}

function buildRenderableMessageParts(
  message: ChatMessage,
  parts: MessagePartView[],
  tools: ToolExecutionView[],
  toolsById: Record<string, ToolExecutionView>,
  transport?: string,
): RenderableMessagePart[] {
  const orderedParts = [...parts].sort(compareMessageParts);
  const renderedTools = new Set<string>();
  const renderedParts: RenderableMessagePart[] = [];

  for (const part of orderedParts) {
    if (part.kind === "text") {
      if (part.text) {
        renderedParts.push({
          id: part.id,
          kind: "text",
          text: part.text,
        });
      }
      continue;
    }

    if (!part.toolCallId) {
      continue;
    }

    renderedTools.add(part.toolCallId);
    renderedParts.push({
      id: part.id,
      kind: "tool",
      tool: toolsById[part.toolCallId],
      toolCallId: part.toolCallId,
    });
  }

  if (orderedParts.length === 0) {
    const fallback = fallbackMessageBody(message, transport);
    if (fallback) {
      renderedParts.push({
        id: `text:${message.id}:fallback`,
        kind: "text",
        text: fallback,
      });
    }
  }

  for (const tool of tools) {
    if (renderedTools.has(tool.toolCallId)) {
      continue;
    }

    renderedParts.push({
      id: `tool:${tool.toolCallId}`,
      kind: "tool",
      tool,
      toolCallId: tool.toolCallId,
    });
  }

  if (renderedParts.length === 0) {
    const fallback = fallbackMessageBody(message, transport);
    if (fallback) {
      renderedParts.push({
        id: `text:${message.id}:fallback`,
        kind: "text",
        text: fallback,
      });
    }
  }

  return renderedParts;
}

function fallbackMessageBody(message: ChatMessage, transport?: string) {
  return (
    message.content ||
    (message.status === "cancelled"
      ? "Response cancelled."
      : message.status === "sending"
        ? thinkingLabel(transport)
        : message.status === "failed"
          ? ""
        : "No content.")
  );
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
    toolKind: record.toolKind,
    payload: record.payload,
    touchedPaths: record.touchedPaths,
    artifacts: record.artifacts,
    createdAt: timestampToMillis(record.createdAt),
    updatedAt: timestampToMillis(record.updatedAt),
  };
}

// Accumulate workspace-relative paths across events, preserving first-seen
// order and dropping duplicates. Returns undefined when nothing is known yet.
function mergeTouchedPaths(
  previous: string[] | undefined,
  incoming: string[] | undefined,
): string[] | undefined {
  if (!incoming || incoming.length === 0) {
    return previous;
  }
  if (!previous || previous.length === 0) {
    return [...incoming];
  }

  const merged = [...previous];
  const seen = new Set(previous);
  for (const path of incoming) {
    if (!seen.has(path)) {
      seen.add(path);
      merged.push(path);
    }
  }
  return merged;
}

// Accumulate artifacts across events, deduping by artifactId and letting a
// later event replace an earlier artifact with the same id (e.g. a diff that
// grew and spilled to a logRef).
function mergeToolArtifacts(
  previous: ToolArtifact[] | undefined,
  incoming: ToolArtifact[] | undefined,
): ToolArtifact[] | undefined {
  if (!incoming || incoming.length === 0) {
    return previous;
  }
  if (!previous || previous.length === 0) {
    return [...incoming];
  }

  const merged = [...previous];
  for (const artifact of incoming) {
    const index = merged.findIndex(
      (existing) => existing.artifactId === artifact.artifactId,
    );
    if (index === -1) {
      merged.push(artifact);
    } else {
      merged[index] = artifact;
    }
  }
  return merged;
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

function buildConversationTimeline(messages: ChatMessage[]): ConversationTimelineItem[] {
  return messages.map((message) => ({
    id: `message:${message.id}`,
    kind: "message",
    messageId: message.id,
  }));
}

function normalizeMessages(messages: ChatMessage[]) {
  return mergeMessages([], messages);
}

function limitChatMessages(messages: ChatMessage[]) {
  return messages.slice(-CHAT_MESSAGE_PAGE_SIZE);
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

// The full `program arg1 arg2 …` line for a run_command call. Sourced from the
// legacy `command` field when present, otherwise from the typed `payload`
// (program/args) — the unified orchestrator carries the command in the payload,
// not the legacy field, so reading only `command` showed nothing.
function formatToolCommand(tool: ToolExecutionView): string {
  if (tool.command) {
    return [tool.command.program, ...(tool.command.args ?? [])].join(" ");
  }
  const program = toolProgram(tool);
  if (!program) {
    return "";
  }
  return [program, ...toolPayloadArgs(tool)].join(" ").trim();
}

// The program a run_command call ran, from the legacy command field or the typed
// payload (the payload arrives on the terminal event; command is null on the new
// orchestrator path).
function toolProgram(tool: ToolExecutionView): string | undefined {
  if (tool.command?.program) {
    return tool.command.program;
  }
  const program = tool.payload?.["program"];
  return typeof program === "string" && program ? program : undefined;
}

function toolPayloadArgs(tool: ToolExecutionView): string[] {
  const args = tool.payload?.["args"];
  return Array.isArray(args)
    ? args.filter((arg): arg is string => typeof arg === "string")
    : [];
}

function toolPath(tool: ToolExecutionView): string | undefined {
  const path = tool.payload?.["path"];
  return typeof path === "string" && path ? path : undefined;
}

// A compact, single-line label for a tool-call spoiler row: the command line for
// run_command (the terminal icon is the implicit verb), or "verb + path" for the
// file/search tools, falling back to a generic verb when the target is unknown.
function formatToolHeadline(tool: ToolExecutionView) {
  if (tool.toolKind === "run_command" || tool.command) {
    return formatToolCommand(tool) || "Ran command";
  }

  const path = toolPath(tool);
  switch (tool.toolKind) {
    case "read_file":
      return path ? `Read ${path}` : "Read file";
    case "write_file":
      return path ? `Wrote ${path}` : "Wrote file";
    case "edit_file":
      return path ? `Edited ${path}` : "Edited file";
    case "apply_patch":
      return "Applied patch";
    case "list_files":
      return "Listed files";
    case "search_text":
      return "Searched";
    default:
      return "Used tool";
  }
}

function formatToolOutput(tool: ToolExecutionView) {
  if (tool.output) {
    return tool.output;
  }

  if (!tool.result) {
    return "";
  }

  const stdout = (tool.result.stdoutPreview || tool.result.stdoutTail || "").trim();
  const stderr = (tool.result.stderrPreview || tool.result.stderrTail || "").trim();

  // The result message is rendered on its own line (inline-tool-call__message),
  // so it must NOT be repeated inside the output block — otherwise a denied /
  // failed tool shows its reason twice.
  return [stdout, stderr ? `[stderr]\n${stderr}` : ""]
    .filter(Boolean)
    .join("\n\n");
}

function countTextLines(text: string) {
  const visibleText = text.replace(/(?:\r\n|\r|\n)+$/, "");
  if (visibleText.length === 0) {
    return 1;
  }

  return visibleText.split(/\r\n|\r|\n/).length;
}

function shouldShowInlineToolOutput(tool: ToolExecutionView, output: string) {
  if (output.trim().length > 0) {
    return true;
  }

  return tool.kind === "started" || tool.kind === "output";
}

function isTerminalToolKind(kind: ToolExecutionEventKind) {
  return (
    kind === "completed" ||
    kind === "failed" ||
    kind === "cancelled" ||
    kind === "timed_out" ||
    kind === "permission_denied" ||
    kind === "loop_blocked"
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
    loop_blocked: "Blocked",
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
  if (
    kind === "permission_requested" ||
    kind === "waiting_for_resource" ||
    kind === "loop_blocked"
  ) {
    return "pending";
  }
  return "running";
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

type RecentStatus = "active" | "branch" | "clock";

type ConversationTimelineItem = MessageTimelineItem;

// Timeline items intentionally carry only ids — never the message/tool objects
// themselves. That keeps an item's identity stable across content changes (a
// streaming delta, a tool-output chunk), so the row stays mounted and updates in
// place. Rows read the live message/tool by id from a reactive lookup.
interface MessageTimelineItem {
  id: string;
  kind: "message";
  messageId: string;
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
  toolKind?: ToolKind;
  payload?: Record<string, unknown> | null;
  touchedPaths?: string[];
  artifacts?: ToolArtifact[];
  createdAt: number;
  updatedAt: number;
}

interface MessagePartView {
  id: string;
  kind: "text" | "tool";
  messageId: string;
  text?: string;
  toolCallId?: string;
  createdAt: number;
  sequence?: number;
}

interface RenderableMessagePart {
  id: string;
  kind: "text" | "tool";
  text?: string;
  tool?: ToolExecutionView;
  toolCallId?: string;
}
