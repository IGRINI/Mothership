// The chat workspace controller: owns the client-side state (projects, chats,
// messages, parts, tool executions, change sets, composer settings), applies
// the streamed core events, and wires the layout panes together. All rendering
// beyond the top-level layout lives in ./components; all pure data logic lives
// in ./message-model and ./model-options.

import {
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
} from "solid-js";
import { listen } from "@tauri-apps/api/event";

import {
  ChatRunEvent,
  ChatMessage,
  ChatThreadSummary,
  ChatUpdatedEvent,
  ChangeSetEvent,
  ChangeSetSummary,
  RevertOutcome,
  ConnectorSettingsEvent,
  ConnectorSettingsSnapshot,
  PromptPreview,
  ProjectSummary,
  ProjectSnapshot,
  ToolApprovalMode,
  ToolExecutionEvent,
  ToolExecutionRecord,
  approveToolExecution,
  branchChatFromMessage,
  cancelChatRun,
  cancelToolExecution,
  continueChatMessage,
  createChat,
  deleteChat,
  deleteProject,
  editChatUserMessage,
  getChat,
  getChatChangeSets,
  getConnectorSettings,
  getPromptPreview,
  isTauriRuntime,
  listChats,
  listProjects,
  openProject,
  pickProjectDirectory,
  renameChat,
  renameProject,
  retryChatMessage,
  revertChangeSet,
  sendChatMessage,
  setChatModel,
  setChatState,
  setProjectAppearance,
  setSelectedModel,
  steerChatRun,
} from "../../shared/api/mothership";
import {
  lastChatId,
  lastProjectId,
  rememberChat,
  rememberProject,
} from "../../shared/session";
import {
  agentFocusRequest,
  consumeAgentFocusRequest,
  forgetChatActivity,
  forgetProjectActivity,
  reportViewedChat,
} from "../../shared/agentActivity";
import {
  modelKey,
  parseReasoningMap,
  serializeReasoningMap,
} from "../../shared/reasoningMap";
import type { ToolImagePreviewItem } from "./ToolCards";
import { type ReasoningOptionId } from "./components/Composer";
import {
  CHAT_SCROLL_BOTTOM_THRESHOLD_PX,
  ConversationPane,
} from "./components/ConversationPane";
import {
  ImagePreviewOverlay,
  type ImagePreviewState,
} from "./components/ImagePreviewOverlay";
import { InspectorPane } from "./components/InspectorPane";
import { Sidebar } from "./components/Sidebar";
import {
  CHAT_MESSAGE_PAGE_SIZE,
  appendMessageDelta,
  appendToolOutput,
  attachMessageIdToToolExecutions,
  bumpChat,
  compareToolExecutions,
  currentUnixTimestamp,
  errorMessage,
  limitChatMessages,
  mergeChatInPlace,
  mergeMessages,
  mergeToolArtifacts,
  mergeTouchedPaths,
  messagePartsByMessageId,
  normalizeMessages,
  payloadObject,
  removedMessageIdsForRetry,
  toolExecutionViewFromRecord,
} from "./message-model";
import {
  coerceReasoningOptionId,
  modelSupportsFastMode,
  reasoningConfigForOption,
  selectableConnectorModelFor,
} from "./model-options";
import type { MessagePartView, ToolExecutionView } from "./types";

const CHAT_SCROLL_RESTORE_FRAMES = 12;

interface ChatScrollPosition {
  top: number;
  fromBottom: number;
}

export function Dashboard() {
  const [projects, setProjects] = createSignal<ProjectSummary[]>([]);
  const [activeProjectId, setActiveProjectId] = createSignal<string>();
  const [chats, setChats] = createSignal<ChatThreadSummary[]>([]);
  const [messages, setMessages] = createSignal<ChatMessage[]>([]);
  const [messageParts, setMessageParts] = createSignal<
    Record<string, MessagePartView[]>
  >({});
  const [connectorSettings, setConnectorSettings] =
    createSignal<ConnectorSettingsSnapshot>();
  const [toolApprovalMode, setToolApprovalModeSignal] =
    createSignal<ToolApprovalMode>("manual");
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
  const [isSteeringRun, setIsSteeringRun] = createSignal(false);
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
  const [promptPreview, setPromptPreview] =
    createSignal<PromptPreview | null>(null);
  const [isLoadingPromptPreview, setIsLoadingPromptPreview] =
    createSignal(false);
  const [promptPreviewError, setPromptPreviewError] = createSignal("");
  const [expandedInlineTools, setExpandedInlineTools] = createSignal<
    Record<string, boolean>
  >({});
  const [imagePreview, setImagePreview] =
    createSignal<ImagePreviewState | null>(null);
  // Workspace change sets for the ACTIVE chat, keyed by change-set id. Hydrated
  // on chat open and kept live via `change-set-event`. Bounded to one chat.
  const [changeSets, setChangeSets] = createSignal<
    Record<string, ChangeSetSummary>
  >({});
  let unlistenChatRun: (() => void) | undefined;
  let unlistenConnectorSettings: (() => void) | undefined;
  let unlistenToolExecution: (() => void) | undefined;
  let unlistenChatUpdated: (() => void) | undefined;
  let unlistenChangeSet: (() => void) | undefined;
  let messageScrollElement: HTMLDivElement | undefined;
  let restoreScrollFrame = 0;
  let openChatRequestId = 0;
  let promptPreviewRequestId = 0;
  let liveMessagePartSequence = 0;
  let draftCommitTimer: number | undefined;
  // The (chat, model) the reasoning signal is currently synced to, so the
  // per-chat-per-model restore effect only re-loads reasoning when the active
  // chat OR its model actually changes (not on its own writes).
  let lastReasoningKey: string | undefined;
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
  const [fastModeEnabled, setFastModeEnabled] = createSignal(false);
  const isChatRunning = () =>
    messages().some(
      (message) => message.role === "assistant" && message.status === "sending",
    );

  // Agent-activity wiring: report which chat is on screen (finished runs in it
  // count as reviewed — messenger-style unread), and clear it on unmount
  // (Settings open) so completions while away stay unread.
  createEffect(() => {
    reportViewedChat(activeChatId());
  });
  onCleanup(() => reportViewedChat(undefined));

  // Follow a status-pill click: open the agent's project + chat. Deferred
  // until projects are loaded — on a cold mount the session-restore pass
  // (requestAgentFocus pre-seeded it) usually lands on the right chat already,
  // making this a no-op.
  createEffect(() => {
    const request = agentFocusRequest();
    if (!request || isLoadingProjects()) {
      return;
    }
    consumeAgentFocusRequest(request.token);
    void (async () => {
      if (request.projectId && request.projectId !== activeProjectId()) {
        // Pre-seed the per-project chat memory so loadChats opens this chat.
        rememberChat(request.projectId, request.chatId);
        await handleSelectProject(request.projectId);
      } else if (request.chatId !== activeChatId()) {
        await openChat(request.chatId);
      }
    })();
  });

  // Reasoning is remembered PER CHAT, PER MODEL: each chat carries a
  // { model → reasoning } map. Whenever the active chat or its model changes,
  // restore THIS chat's reasoning for THIS model (or the model's recommended
  // default). Wait for the model to be known — on a cold start the chat can
  // resolve before connectors load, and an empty option set would blank it.
  createEffect(() => {
    const model = selectedModel();
    const chat = activeChat();
    if (!model || !chat) {
      return;
    }
    const mk = modelKey(model.providerId, model.id);
    const key = `${chat.id}::${mk}`;
    if (key === lastReasoningKey) {
      // Same chat+model — keep the selected reasoning (a user pick, or what we
      // already loaded). Don't clobber it (this effect also re-runs when the
      // chat's own map is written).
      return;
    }
    lastReasoningKey = key;
    setReasoningOptionId(
      coerceReasoningOptionId(parseReasoningMap(chat.reasoning)[mk], model),
    );
  });

  createEffect(() => {
    const model = selectedModel();
    const chat = activeChat();
    if (!modelSupportsFastMode(model)) {
      setFastModeEnabled(false);
      return;
    }

    setFastModeEnabled(Boolean(chat?.fastMode));
  });

  createEffect(() => {
    activeChatId();
    promptPreviewRequestId += 1;
    setPromptPreview(null);
    setPromptPreviewError("");
    setIsLoadingPromptPreview(false);
  });

  function fastModeForSelectedModel() {
    return fastModeEnabled() && modelSupportsFastMode(selectedModel());
  }

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

      void listen<ChangeSetEvent>("change-set-event", (event) => {
        applyChangeSetEvent(event.payload);
      }).then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          unlistenChangeSet = unlisten;
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
    unlistenChangeSet?.();
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
      // must be able to view different projects independently). Prefer the
      // client's last-open project (session restore); otherwise the snapshot's
      // hint, otherwise the first project.
      const remembered = lastProjectId();
      const projectId =
        (remembered && snapshot.projects.some((project) => project.id === remembered)
          ? remembered
          : undefined) ??
        snapshot.activeProjectId ??
        snapshot.projects[0]?.id;
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
    rememberProject(projectId);
    setIsLoadingChats(true);
    setError("");

    try {
      const loadedChats = await listChats(100, projectId);
      if (activeProjectId() !== projectId) {
        return;
      }
      setChats(loadedChats);

      // Reopen the chat this client last had open in this project; otherwise the
      // most recent one.
      const rememberedChatId = lastChatId(projectId);
      const chatToOpen =
        rememberedChatId && loadedChats.some((chat) => chat.id === rememberedChatId)
          ? rememberedChatId
          : loadedChats[0]?.id;

      if (chatToOpen) {
        await openChat(chatToOpen);
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
    // Persist the current chat's unsent draft before leaving it, then clear the
    // composer so the previous chat's text doesn't linger in the new project.
    flushDraftCommit(activeChatId());
    setDraft("");
    openChatRequestId += 1;
    setActiveProjectId(projectId);
    setActiveChatId(undefined);
    setChats([]);
    setMessages([]);
    setMessageParts({});
    setIsLoadingMessages(false);
    setToolExecutions({});
    setChangeSets({});
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
    flushDraftCommit(activeChatId());
    setDraft("");
    openChatRequestId += 1;
    setActiveChatId(undefined);
    setMessages([]);
    setMessageParts({});
    setIsLoadingMessages(false);
    setToolExecutions({});
    setChangeSets({});
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

  async function handleRenameChat(chatId: string, title: string) {
    const previous = chats().find((chat) => chat.id === chatId);
    if (!previous) {
      return;
    }
    // Next-frame feedback; reconcile with the persisted summary or roll back.
    setChats((current) => mergeChatInPlace(current, { ...previous, title }));
    setError("");
    try {
      const updated = await renameChat(chatId, title);
      setChats((current) => mergeChatInPlace(current, updated));
    } catch (caughtError) {
      setChats((current) => mergeChatInPlace(current, previous));
      setError(errorMessage(caughtError));
    }
  }

  async function handleDeleteChat(chatId: string) {
    const previousChats = chats();
    const wasActive = activeChatId() === chatId;
    const fallbackChatId = previousChats.find((chat) => chat.id !== chatId)?.id;

    // Optimistic removal; the agent pill forgets the chat immediately too.
    setChats((current) => current.filter((chat) => chat.id !== chatId));
    forgetChatActivity(chatId);
    setError("");
    if (wasActive) {
      if (fallbackChatId) {
        void openChat(fallbackChatId);
      } else {
        openChatRequestId += 1;
        setActiveChatId(undefined);
        setMessages([]);
        setMessageParts({});
        setToolExecutions({});
        setChangeSets({});
        setIsLoadingMessages(false);
        const projectId = activeProjectId();
        if (projectId) {
          rememberChat(projectId, undefined);
        }
      }
    }

    try {
      await deleteChat(chatId);
    } catch (caughtError) {
      setChats(previousChats);
      if (wasActive) {
        void openChat(chatId);
      }
      setError(errorMessage(caughtError));
    }
  }

  async function handleRenameProject(projectId: string, name: string) {
    const previous = projects();
    setProjects((current) =>
      current.map((project) =>
        project.id === projectId ? { ...project, name } : project,
      ),
    );
    setError("");
    try {
      applyProjectSnapshot(await renameProject(projectId, name));
    } catch (caughtError) {
      setProjects(previous);
      setError(errorMessage(caughtError));
    }
  }

  async function handleDeleteProject(projectId: string) {
    const previousProjects = projects();
    const wasActive = activeProjectId() === projectId;
    const fallbackProjectId = previousProjects.find(
      (project) => project.id !== projectId,
    )?.id;

    setProjects((current) =>
      current.filter((project) => project.id !== projectId),
    );
    forgetProjectActivity(projectId);
    setError("");
    if (wasActive) {
      if (fallbackProjectId) {
        void handleSelectProject(fallbackProjectId);
      } else {
        setActiveProjectId(undefined);
        setActiveChatId(undefined);
        setChats([]);
        setMessages([]);
        setMessageParts({});
        setToolExecutions({});
        setChangeSets({});
        setIsLoadingMessages(false);
      }
    }

    try {
      applyProjectSnapshot(await deleteProject(projectId));
    } catch (caughtError) {
      setProjects(previousProjects);
      setError(errorMessage(caughtError));
    }
  }

  async function handleSetProjectAppearance(
    projectId: string,
    icon: string | null,
    iconColor: string | null,
  ) {
    const previous = projects();
    setProjects((current) =>
      current.map((project) =>
        project.id === projectId ? { ...project, icon, iconColor } : project,
      ),
    );
    try {
      applyProjectSnapshot(await setProjectAppearance(projectId, icon, iconColor));
    } catch (caughtError) {
      setProjects(previous);
      setError(errorMessage(caughtError));
    }
  }

  async function openChat(chatId: string) {
    saveActiveChatScroll();
    // Save the previous chat's unsent draft before switching away.
    const previousChatId = activeChatId();
    if (previousChatId && previousChatId !== chatId) {
      flushDraftCommit(previousChatId);
    } else {
      cancelDraftCommit();
    }
    const requestId = ++openChatRequestId;
    setActiveChatId(chatId);
    setMessages([]);
    setMessageParts({});
    setChangeSets({});
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
      // Restore this chat's approval mode / reasoning / draft into the composer.
      seedChatStateFromChat(conversation.chat);
      const projectId = conversation.chat.projectId ?? activeProjectId();
      if (projectId) {
        rememberChat(projectId, chatId);
      }
      setMessages(normalizeMessages(conversation.messages));
      setMessageParts(messagePartsByMessageId(conversation.messageParts ?? []));
      hydrateToolExecutions(conversation.toolExecutions ?? []);
      void loadChatChangeSets(chatId, requestId);
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

  function applyChangeSetEvent(event: ChangeSetEvent) {
    const summary = event.summary;
    // Only track change sets for the chat currently open; others are loaded on
    // demand when their chat is opened, so memory stays bounded to one chat.
    if (summary.chatId && summary.chatId !== activeChatId()) {
      return;
    }
    setChangeSets((current) => ({ ...current, [summary.id]: summary }));
  }

  async function loadChatChangeSets(chatId: string, requestId: number) {
    try {
      const sets = await getChatChangeSets(chatId);
      if (requestId !== openChatRequestId || activeChatId() !== chatId) {
        return;
      }
      const indexed: Record<string, ChangeSetSummary> = {};
      for (const set of sets) {
        indexed[set.id] = set;
      }
      setChangeSets(indexed);
    } catch {
      // A failed change-set hydrate must never block opening the chat.
    }
  }

  async function handleLoadPromptPreview() {
    const chatId = activeChatId();
    if (!chatId) {
      setPromptPreviewError("No chat selected.");
      return;
    }

    const requestId = ++promptPreviewRequestId;
    setIsLoadingPromptPreview(true);
    setPromptPreviewError("");

    try {
      const preview = await getPromptPreview(chatId);
      if (requestId !== promptPreviewRequestId || activeChatId() !== chatId) {
        return;
      }
      setPromptPreview(preview);
    } catch (caughtError) {
      if (requestId === promptPreviewRequestId) {
        setPromptPreviewError(errorMessage(caughtError));
      }
    } finally {
      if (requestId === promptPreviewRequestId) {
        setIsLoadingPromptPreview(false);
      }
    }
  }

  async function handleCopyPromptPreview() {
    const preview = promptPreview();
    if (!preview) {
      return;
    }

    try {
      await navigator.clipboard.writeText(preview.renderedText);
      setPromptPreviewError("");
    } catch (caughtError) {
      setPromptPreviewError(errorMessage(caughtError));
    }
  }

  async function handleRevertChangeSet(
    changeSetId: string,
  ): Promise<RevertOutcome> {
    const outcome = await revertChangeSet(changeSetId);
    setChangeSets((current) => ({
      ...current,
      [outcome.changeSet.id]: outcome.changeSet,
    }));
    return outcome;
  }

  // Set while an optimistically shown New Chat is still being created in Core,
  // so flows that need the REAL chat id (e.g. send) can await it.
  let pendingChatCreation: Promise<unknown> | undefined;

  async function handleNewChat() {
    const projectId = activeProjectId();
    if (!projectId) {
      setError("Open a project before starting a chat.");
      return;
    }

    saveActiveChatScroll();
    const sourceChatId = activeChatId();
    cancelDraftCommit();
    openChatRequestId += 1;
    setIsLoadingMessages(false);
    setError("");

    // Persist the current chat's latest state (approval/reasoning/draft) BEFORE
    // copying it into the new chat. The values are captured synchronously here —
    // the optimistic switch below resets the composer signals, so this must not
    // read them later — and create_chat is sequenced after the write.
    const sourceChat = chats().find((chat) => chat.id === sourceChatId);
    const sourceCommit = sourceChatId
      ? setChatState(
          sourceChatId,
          toolApprovalMode(),
          sourceChat?.reasoning ?? null,
          fastModeForSelectedModel() ? true : null,
          draft() || null,
        )
          .then((updated) => {
            setChats((current) => mergeChatInPlace(current, updated));
          })
          .catch((caughtError: unknown) => {
            setError(errorMessage(caughtError));
          })
      : Promise.resolve();

    // Next-frame feedback: show and activate the new chat immediately; the
    // created chat replaces it (or the switch rolls back) when the IPC settles.
    const now = new Date().toISOString();
    const optimisticChat: ChatThreadSummary = {
      id: `optimistic:chat:${Date.now()}`,
      projectId,
      title: "New Chat",
      preview: "",
      messageCount: 0,
      providerId: sourceChat?.providerId ?? null,
      modelId: sourceChat?.modelId ?? null,
      approvalMode: sourceChat?.approvalMode ?? null,
      reasoning: sourceChat?.reasoning ?? null,
      fastMode: sourceChat?.fastMode ?? null,
      draft: null,
      createdAt: now,
      updatedAt: now,
    };
    const sourceMessages = messages();
    setChats((current) => bumpChat(current, optimisticChat));
    setActiveChatId(optimisticChat.id);
    seedChatStateFromChat(optimisticChat);
    setMessages([]);
    setMessageParts({});

    const creation = sourceCommit.then(() =>
      createChat(projectId, sourceChatId),
    );
    pendingChatCreation = creation;
    try {
      const conversation = await creation;
      setChats((current) =>
        bumpChat(
          current.filter((chat) => chat.id !== optimisticChat.id),
          conversation.chat,
        ),
      );
      // Adopt only if the user hasn't already switched somewhere else.
      if (activeChatId() === optimisticChat.id) {
        setActiveChatId(conversation.chat.id);
        seedChatStateFromChat(conversation.chat);
        rememberChat(projectId, conversation.chat.id);
        setMessages(normalizeMessages(conversation.messages));
        setMessageParts({});
        restoreChatScroll(conversation.chat.id);
      }
    } catch (caughtError) {
      setChats((current) =>
        current.filter((chat) => chat.id !== optimisticChat.id),
      );
      if (activeChatId() === optimisticChat.id) {
        setActiveChatId(sourceChatId);
        setMessages(sourceMessages);
        if (sourceChatId) {
          restoreChatScroll(sourceChatId);
        }
      }
      setError(errorMessage(caughtError));
    } finally {
      if (pendingChatCreation === creation) {
        pendingChatCreation = undefined;
      }
    }
  }

  async function handleSendMessage() {
    const content = draft().trim();
    if (!content || isSubmittingEdit()) {
      return;
    }
    const runId = activeRunId();
    if (runId) {
      if (isSteeringRun()) {
        return;
      }
      cancelDraftCommit();
      setDraft("");
      setError("");
      setIsSteeringRun(true);
      try {
        await steerChatRun(runId, content);
      } catch (caughtError) {
        setDraft(content);
        setError(errorMessage(caughtError));
      } finally {
        setIsSteeringRun(false);
      }
      return;
    }
    if (isSending() || isChatRunning()) {
      return;
    }
    const projectId = activeProjectId();
    if (!projectId) {
      setError("Open a project before sending a message.");
      return;
    }

    const currentChatId = activeChatId();
    cancelDraftCommit();
    setDraft("");
    setIsSending(true);
    setError("");

    // Next-frame feedback: the user's bubble appears immediately; the persisted
    // pair (user + assistant placeholder) replaces it when the send settles.
    const optimisticId = `optimistic:user:${Date.now()}`;
    const optimisticMessage: ChatMessage = {
      id: optimisticId,
      chatId: currentChatId ?? "",
      position:
        messages().reduce(
          (max, message) => Math.max(max, message.position),
          0,
        ) + 1,
      role: "user",
      content,
      status: "complete",
      createdAt: new Date().toISOString(),
    };
    setMessages((current) => mergeMessages(current, [optimisticMessage]));

    try {
      let chatId = currentChatId;
      // A New Chat shown optimistically may still be settling in Core — wait
      // for the real id instead of sending into the temporary one.
      if (chatId?.startsWith("optimistic:")) {
        await pendingChatCreation?.catch(() => undefined);
        chatId = activeChatId();
        if (chatId?.startsWith("optimistic:")) {
          chatId = undefined;
        }
      }
      // The run seeds its approval mode from the CHAT's stored value, so the
      // chat must carry the composer's current settings BEFORE the run starts.
      // For an existing chat they're already persisted (set on change); for a
      // not-yet-created one, create it and persist the settings first — otherwise
      // the first message would run under the default mode, ignoring a selected
      // Auto/YOLO. (The draft column is cleared transactionally by the send.)
      if (!chatId) {
        const created = await createChat(projectId);
        // Seed the new chat with the composer's current settings: approval mode
        // and the current model's reasoning (as a one-entry per-model map).
        const model = selectedModel();
        const currentReasoning = reasoningOptionId();
        const reasoningJson =
          model && currentReasoning
            ? serializeReasoningMap({
                [modelKey(model.providerId, model.id)]: currentReasoning,
              })
            : null;
        const seeded = await setChatState(
          created.chat.id,
          toolApprovalMode(),
          reasoningJson,
          fastModeForSelectedModel() ? true : null,
          null,
        );
        setChats((current) => bumpChat(current, seeded));
        setActiveChatId(seeded.id);
        chatId = seeded.id;
      }

      const result = await sendChatMessage(
        chatId,
        content,
        projectId,
        reasoningConfigForOption(reasoningOptionId(), selectedModel()),
        fastModeForSelectedModel(),
      );

      setActiveRunIds((current) => ({
        ...current,
        [result.chat.id]: result.runId,
      }));
      rememberRunMessage(result.runId, result.assistantMessage.id);
      setChats((current) => bumpChat(current, result.chat));
      setActiveChatId(result.chat.id);
      rememberChat(projectId, result.chat.id);
      setMessages((current) => {
        const base = (
          currentChatId === result.chat.id ? current : []
        ).filter((message) => message.id !== optimisticId);
        return mergeMessages(base, [result.userMessage, result.assistantMessage]);
      });
      if (currentChatId !== result.chat.id) {
        setMessageParts({});
      }
    } catch (caughtError) {
      setMessages((current) =>
        current.filter((message) => message.id !== optimisticId),
      );
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

  // Retry/Continue in-flight state: folded into the conversation's `isSending`
  // so the ErrorCard buttons and the composer disable on the next frame.
  const [isRecoveringRun, setIsRecoveringRun] = createSignal(false);

  async function handleRetry() {
    const chatId = activeChatId();
    if (!chatId || isChatRunning() || isRecoveringRun()) {
      return;
    }
    setError("");
    setIsRecoveringRun(true);

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
    } finally {
      setIsRecoveringRun(false);
    }
  }

  async function handleContinue() {
    const chatId = activeChatId();
    if (!chatId || isChatRunning() || isRecoveringRun()) {
      return;
    }
    setError("");
    setIsRecoveringRun(true);

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
    } finally {
      setIsRecoveringRun(false);
    }
  }

  async function loadConnectorSettings() {
    try {
      setConnectorSettings(await getConnectorSettings());
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  // Per-chat session state (approval mode / reasoning / fast mode / draft) lives
  // on the chat now. The composer's local signals are SEEDED from the active chat
  // on open and WRITTEN back via `commitChatState`.
  function seedChatStateFromChat(chat: ChatThreadSummary) {
    const mode = chat.approvalMode;
    setToolApprovalModeSignal(
      mode === "auto_safe" || mode === "yolo" ? mode : "manual",
    );
    // Reasoning is restored PER MODEL by the reasoning effect (keyed on the
    // chat's model), not from a per-chat value — so it's intentionally not set
    // here.
    setDraft(chat.draft ?? "");
  }

  // Persist the current approval/reasoning/fast-mode/draft to a chat (full
  // overwrite). The returned summary is merged so the local chat reflects the
  // saved state.
  async function commitChatState(chatId: string) {
    // Approval/draft commits preserve the chat's per-model reasoning map as-is
    // (reasoning is written by handleReasoningOptionChange, not here).
    const reasoning = chats().find((chat) => chat.id === chatId)?.reasoning ?? null;
    try {
      const updated = await setChatState(
        chatId,
        toolApprovalMode(),
        reasoning,
        fastModeForSelectedModel() ? true : null,
        draft() || null,
      );
      setChats((current) => mergeChatInPlace(current, updated));
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  function cancelDraftCommit() {
    if (draftCommitTimer !== undefined) {
      window.clearTimeout(draftCommitTimer);
      draftCommitTimer = undefined;
    }
  }
  // Debounced draft persistence — typing shouldn't hit the DB on every keystroke.
  function scheduleDraftCommit(chatId: string) {
    cancelDraftCommit();
    draftCommitTimer = window.setTimeout(() => {
      draftCommitTimer = undefined;
      void commitChatState(chatId);
    }, 700);
  }
  // Flush any pending draft write immediately (before switching/creating a chat).
  function flushDraftCommit(chatId: string | undefined) {
    if (draftCommitTimer === undefined || !chatId) {
      cancelDraftCommit();
      return;
    }
    cancelDraftCommit();
    void commitChatState(chatId);
  }

  function handleDraftChange(value: string) {
    setDraft(value);
    const chatId = activeChatId();
    if (chatId) {
      scheduleDraftCommit(chatId);
    }
  }

  async function handleReasoningOptionChange(optionId: ReasoningOptionId) {
    setReasoningOptionId(optionId);
    // Reasoning is per chat, per model: record it in THIS chat's map against the
    // active model. (With no chat yet, the pre-create on send captures it.)
    const model = selectedModel();
    const chat = activeChat();
    if (!model || !chat) {
      return;
    }
    const mk = modelKey(model.providerId, model.id);
    const serialized = serializeReasoningMap({
      ...parseReasoningMap(chat.reasoning),
      [mk]: optionId,
    });
    // Optimistic local update so the restore effect (same chat+model key) won't
    // reload over the pick; then persist and adopt the canonical result.
    setChats((current) =>
      mergeChatInPlace(current, { ...chat, reasoning: serialized }),
    );
    try {
      const updated = await setChatState(
        chat.id,
        toolApprovalMode(),
        serialized,
        fastModeForSelectedModel() ? true : null,
        draft() || null,
      );
      setChats((current) => mergeChatInPlace(current, updated));
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function handleFastModeChange(enabled: boolean) {
    const nextFastMode = enabled && modelSupportsFastMode(selectedModel());
    setFastModeEnabled(nextFastMode);

    const chat = activeChat();
    if (!chat) {
      return;
    }

    const optimistic = {
      ...chat,
      fastMode: nextFastMode ? true : null,
    };
    setChats((current) => mergeChatInPlace(current, optimistic));
    try {
      const updated = await setChatState(
        chat.id,
        toolApprovalMode(),
        chat.reasoning ?? null,
        nextFastMode ? true : null,
        draft() || null,
      );
      setChats((current) => mergeChatInPlace(current, updated));
    } catch (caughtError) {
      setError(errorMessage(caughtError));
    }
  }

  async function handleToolApprovalModeChange(mode: ToolApprovalMode) {
    setError("");
    setToolApprovalModeSignal(mode);
    const chatId = activeChatId();
    if (chatId) {
      await commitChatState(chatId);
    }
  }

  async function handleSelectModel(providerId: string, modelId: string) {
    const chatId = activeChatId();
    const nextModel = selectableConnectorModelFor(
      connectorSettings(),
      providerId,
      modelId,
    );
    const supportsFast = modelSupportsFastMode(nextModel);
    if (chatId) {
      // Per-chat model: the header selector derives from the chat summary, so
      // patch it locally FIRST (next-frame feedback) and reconcile with the
      // persisted summary — or roll back — when the IPC settles.
      const previousChat = chats().find((chat) => chat.id === chatId);
      const previousFastEnabled = fastModeEnabled();
      if (previousChat) {
        setChats((current) =>
          mergeChatInPlace(current, {
            ...previousChat,
            providerId,
            modelId,
            fastMode: supportsFast ? previousChat.fastMode : false,
          }),
        );
      }
      setFastModeEnabled(Boolean(supportsFast && previousChat?.fastMode));
      try {
        let chat = await setChatModel(chatId, providerId, modelId);
        if (!supportsFast && chat.fastMode) {
          chat = await setChatState(
            chat.id,
            toolApprovalMode(),
            chat.reasoning ?? null,
            null,
            draft() || null,
          );
        }
        setFastModeEnabled(Boolean(chat.fastMode && supportsFast));
        setChats((current) => mergeChatInPlace(current, chat));
      } catch (caughtError) {
        if (previousChat) {
          setChats((current) => mergeChatInPlace(current, previousChat));
        }
        setFastModeEnabled(previousFastEnabled);
        setError(errorMessage(caughtError));
      }
    } else {
      // No active chat → set the global default applied to new chats. Patch the
      // snapshot's selected model locally first; reconcile or roll back after.
      const previousSettings = connectorSettings();
      const previousFastEnabled = fastModeEnabled();
      if (previousSettings) {
        setConnectorSettings({
          ...previousSettings,
          selectedModel: {
            providerId,
            modelId,
            updatedAt: new Date().toISOString(),
          },
          providers: previousSettings.providers.map((provider) => ({
            ...provider,
            selectedModelId: provider.id === providerId ? modelId : null,
          })),
        });
      }
      if (!supportsFast) {
        setFastModeEnabled(false);
      }
      try {
        setConnectorSettings(await setSelectedModel(providerId, modelId));
      } catch (caughtError) {
        setConnectorSettings(previousSettings);
        setFastModeEnabled(previousFastEnabled);
        setError(errorMessage(caughtError));
      }
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

  // Optimistically transition a tool record so the approval buttons react on
  // the next frame. The authoritative tool-execution event stream overwrites
  // the record; a rejected IPC call restores the prior state (only if no real
  // event got there first).
  function transitionToolExecution(
    toolCallId: string,
    patch: Partial<ToolExecutionView>,
  ): ToolExecutionView | undefined {
    const previous = toolExecutions()[toolCallId];
    if (!previous) {
      return undefined;
    }
    setToolExecutions((current) => ({
      ...current,
      [toolCallId]: { ...previous, ...patch, updatedAt: Date.now() },
    }));
    return previous;
  }

  function restoreToolExecution(
    toolCallId: string,
    previous: ToolExecutionView | undefined,
    optimisticKind: ToolExecutionView["kind"],
  ) {
    if (!previous) {
      return;
    }
    setToolExecutions((current) =>
      current[toolCallId]?.kind === optimisticKind
        ? { ...current, [toolCallId]: previous }
        : current,
    );
  }

  async function handleApproveTool(toolCallId: string) {
    setError("");
    const tool = toolExecutions()[toolCallId];
    const previous =
      tool?.kind === "permission_requested"
        ? transitionToolExecution(toolCallId, {
            kind: "queued",
            message: "Approving…",
          })
        : undefined;
    try {
      await approveToolExecution(toolCallId, true);
    } catch (caughtError) {
      restoreToolExecution(toolCallId, previous, "queued");
      setError(errorMessage(caughtError));
    }
  }

  async function handleDenyTool(toolCallId: string) {
    setError("");
    const tool = toolExecutions()[toolCallId];
    const previous =
      tool?.kind === "permission_requested"
        ? transitionToolExecution(toolCallId, {
            kind: "permission_denied",
            message: "Denied by user",
          })
        : undefined;
    try {
      await approveToolExecution(toolCallId, false, "Denied by user");
    } catch (caughtError) {
      restoreToolExecution(toolCallId, previous, "permission_denied");
      setError(errorMessage(caughtError));
    }
  }

  async function handleCancelTool(toolCallId: string) {
    setError("");
    // Cosmetic next-frame cue only — the tool keeps streaming until Core
    // confirms the cancellation, so the kind is left untouched.
    transitionToolExecution(toolCallId, { message: "Cancelling…" });
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

  function handlePreviewToolImage(
    image: ToolImagePreviewItem,
    images: ToolImagePreviewItem[],
  ) {
    if (images.length === 0) {
      return;
    }
    const index = Math.max(
      0,
      images.findIndex(
        (candidate) =>
          candidate.id === image.id && candidate.path === image.path,
      ),
    );
    setImagePreview({ items: images, index });
  }

  function handleSelectPreviewImage(index: number) {
    setImagePreview((current) => {
      if (!current) {
        return current;
      }
      return {
        ...current,
        index: Math.min(Math.max(index, 0), current.items.length - 1),
      };
    });
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
          toolKind: event.toolKind ?? previous?.toolKind ?? undefined,
          payload: payloadObject(event.payload) ?? previous?.payload,
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

  const changeSetsByMessageId = createMemo(() => {
    const messageIds = new Set(messages().map((message) => message.id));
    const grouped: Record<string, ChangeSetSummary[]> = {};

    for (const set of Object.values(changeSets())) {
      const messageId = set.messageId;
      if (!messageId || !messageIds.has(messageId)) {
        continue;
      }
      (grouped[messageId] ??= []).push(set);
    }

    for (const list of Object.values(grouped)) {
      list.sort(
        (a, b) =>
          Number(a.createdAt) - Number(b.createdAt) || a.id.localeCompare(b.id),
      );
    }

    return grouped;
  });

  return (
    <>
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
          onSelectProject={(projectId) => void handleSelectProject(projectId)}
          onRenameChat={(chatId, title) => void handleRenameChat(chatId, title)}
          onDeleteChat={(chatId) => void handleDeleteChat(chatId)}
          onRenameProject={(projectId, name) =>
            void handleRenameProject(projectId, name)
          }
          onDeleteProject={(projectId) => void handleDeleteProject(projectId)}
          onSetProjectAppearance={(projectId, icon, iconColor) =>
            void handleSetProjectAppearance(projectId, icon, iconColor)
          }
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
          isSending={
            isSending() ||
            isSubmittingEdit() ||
            isChatRunning() ||
            isRecoveringRun()
          }
          messages={messages()}
          runTransports={runTransports()}
          activeRunId={activeRunId()}
          toolApprovalMode={toolApprovalMode()}
          reasoningOptionId={reasoningOptionId()}
          fastModeEnabled={fastModeEnabled()}
          expandedInlineTools={expandedInlineTools()}
          messagePartsByMessageId={messageParts()}
          toolExecutionsByMessageId={toolExecutionsByMessageId()}
          changeSetsByMessageId={changeSetsByMessageId()}
          onApproveTool={handleApproveTool}
          onRevertChangeSet={handleRevertChangeSet}
          onMessageScrollElement={handleMessageScrollElement}
          onCancelRun={handleCancelRun}
          onCancelTool={handleCancelTool}
          onContinue={handleContinue}
          onBranchMessage={(message) => void handleBranchMessage(message)}
          onCancelEdit={handleCancelEdit}
          onDenyTool={handleDenyTool}
          onDraftChange={handleDraftChange}
          onError={setError}
          onEditDraftChange={setEditingDraft}
          onPreviewToolImage={handlePreviewToolImage}
          onRetry={handleRetry}
          onReasoningOptionChange={handleReasoningOptionChange}
          onFastModeChange={(enabled) => void handleFastModeChange(enabled)}
          onSelectModel={handleSelectModel}
          onToolApprovalModeChange={(mode) =>
            void handleToolApprovalModeChange(mode)
          }
          onSendMessage={handleSendMessage}
          onStartEdit={handleStartEdit}
          onSubmitEdit={(messageId) => void handleSubmitEdit(messageId)}
          onToggleInlineTool={handleToggleInlineTool}
        />
        <InspectorPane
          activeChat={activeChat()}
          messageCount={messages().length}
          promptError={promptPreviewError()}
          promptLoading={isLoadingPromptPreview()}
          promptPreview={promptPreview()}
          toolExecutions={visibleToolExecutions()}
          onApproveTool={handleApproveTool}
          onCancelTool={handleCancelTool}
          onCopyPrompt={handleCopyPromptPreview}
          onDenyTool={handleDenyTool}
          onLoadPrompt={() => void handleLoadPromptPreview()}
        />
      </main>
      <ImagePreviewOverlay
        state={imagePreview()}
        onClose={() => setImagePreview(null)}
        onSelectIndex={handleSelectPreviewImage}
      />
    </>
  );
}
