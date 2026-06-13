// The browser-preview fake backend. Outside the Tauri runtime there is no
// sidecar, so `mothership.ts` routes every API call here instead — an
// in-memory, session-lifetime implementation that is just real enough to
// exercise the UI (virtualized lists, chats, settings forms). One exported
// function per API operation; all mutable state and copy/derive helpers are
// private to this module. Never imported in the desktop runtime path.

import type {
  ActivityEvent,
  AdapterSettingPatchValue,
  AdapterSettingsView,
  ChatConversation,
  ChatMessage,
  ChatThreadSummary,
  ConnectorSettingsSnapshot,
  DashboardSnapshot,
  FastModeCapabilities,
  FeatureRoute,
  LlmModel,
  PersonalizationSettings,
  ProjectSnapshot,
  ProjectSummary,
  PromptPreview,
  PromptPreviewSection,
  ReasoningCapabilities,
  SecretSettingState,
  SendChatMessageResult,
  SidecarStatus,
  ToolPolicySettings,
} from "./generated";
import type { JsonValue } from "./generated/serde_json/JsonValue";

let previewChangeJournalRetention = 10;
let previewSnapshot: DashboardSnapshot | null = null;
let previewChats: ChatThreadSummary[] | null = null;
const previewMessages = new Map<string, ChatMessage[]>();
let previewConnectorSettings: ConnectorSettingsSnapshot | null = null;
let previewProjectSnapshot: ProjectSnapshot | null = null;
let previewPersonalization: PersonalizationSettings | null = null;
let previewToolPolicy: ToolPolicySettings | null = null;
let previewChatSequence = 0;
let previewMessageSequence = 0;

// --- Dashboard ----------------------------------------------------------------

export function getPreviewDashboardSnapshot(): DashboardSnapshot {
  return getPreviewSnapshot();
}

export function appendPreviewActivityEvent(message: string): DashboardSnapshot {
  const snapshot = getPreviewSnapshot();
  const nextEvent: ActivityEvent = {
    id: snapshot.activityEvents.length + 1,
    source: "browser-preview",
    level: "info",
    message: message.trim(),
    occurredAt: currentTimestamp(),
  };

  previewSnapshot = withMetrics({
    ...snapshot,
    activityEvents: [nextEvent, ...snapshot.activityEvents],
  });

  return previewSnapshot;
}

export function previewSidecarStatus(): SidecarStatus {
  const snapshot = getPreviewSnapshot();
  return {
    healthy: true,
    databasePath: "browser-preview.sqlite3",
    workspaceItems: snapshot.workspaceItems.length,
    activityEvents: snapshot.activityEvents.length,
    databaseBytes: 647168,
  };
}

function getPreviewSnapshot() {
  previewSnapshot ??= withMetrics({
    metrics: [],
    workspaceItems: Array.from({ length: 2500 }, (_, index) => {
      const kinds = ["agent", "source", "queue", "index", "task"];
      const namespaces = ["core", "tauri", "solid", "sidecar", "research"];
      const statuses = ["active", "idle", "queued", "research", "blocked"];

      return {
        id: index + 1,
        kind: kinds[index % kinds.length],
        namespace: namespaces[index % namespaces.length],
        name: `mothership-unit-${index.toString().padStart(4, "0")}`,
        status: statuses[index % statuses.length],
        updatedAt: currentTimestamp(),
      };
    }),
    activityEvents: Array.from({ length: 5000 }, (_, index) => ({
      id: index + 1,
      source: index % 3 === 0 ? "sidecar" : "app",
      level: index % 17 === 0 ? "warn" : "info",
      message: `virtualized pipeline event #${index.toString().padStart(4, "0")}`,
      occurredAt: currentTimestamp(),
    })).reverse(),
  });

  return previewSnapshot;
}

function withMetrics(snapshot: DashboardSnapshot): DashboardSnapshot {
  return {
    ...snapshot,
    metrics: [
      {
        label: "Workspace rows",
        value: snapshot.workspaceItems.length.toString(),
        tone: "data",
      },
      {
        label: "Activity events",
        value: snapshot.activityEvents.length.toString(),
        tone: "signal",
      },
      {
        label: "Virtualized rows",
        value: (
          snapshot.workspaceItems.length + snapshot.activityEvents.length
        ).toString(),
        tone: "compute",
      },
      {
        label: "SQLite mode",
        value: "preview",
        tone: "storage",
      },
    ],
  };
}

function currentTimestamp() {
  return Math.floor(Date.now() / 1000).toString();
}

// --- Projects -------------------------------------------------------------------

export function listPreviewProjects(): ProjectSnapshot {
  return copyProjectSnapshot(getPreviewProjectSnapshot());
}

function getPreviewProjectSnapshot() {
  previewProjectSnapshot ??= {
    projects: [
      {
        id: "preview-project-mothership",
        name: "Mothership",
        path: "E:/Mothership",
        chatCount: 0,
        createdAt: currentTimestamp(),
        updatedAt: currentTimestamp(),
        lastOpenedAt: currentTimestamp(),
      },
    ],
    activeProjectId: "preview-project-mothership",
  };
  return previewProjectSnapshot;
}

export function openPreviewProject(path: string): ProjectSnapshot {
  const normalizedPath = path.trim();
  if (!normalizedPath) {
    throw new Error("project path cannot be empty");
  }
  const now = currentTimestamp();
  const snapshot = getPreviewProjectSnapshot();
  const existing = snapshot.projects.find(
    (project) => project.path.toLowerCase() === normalizedPath.toLowerCase(),
  );
  if (existing) {
    existing.lastOpenedAt = now;
    existing.updatedAt = now;
    snapshot.activeProjectId = existing.id;
    previewProjectSnapshot = copyProjectSnapshot(snapshot);
    return copyProjectSnapshot(previewProjectSnapshot);
  }

  const project: ProjectSummary = {
    id: `preview-project-${snapshot.projects.length + 1}`,
    name: deriveProjectName(normalizedPath),
    path: normalizedPath,
    chatCount: 0,
    createdAt: now,
    updatedAt: now,
    lastOpenedAt: now,
  };
  previewProjectSnapshot = {
    projects: [project, ...snapshot.projects],
    activeProjectId: project.id,
  };
  return copyProjectSnapshot(previewProjectSnapshot);
}

export function selectPreviewProject(projectId: string): ProjectSnapshot {
  const snapshot = getPreviewProjectSnapshot();
  if (!snapshot.projects.some((project) => project.id === projectId)) {
    throw new Error(`project not found: ${projectId}`);
  }
  previewProjectSnapshot = {
    projects: snapshot.projects.map((project) => ({ ...project })),
    activeProjectId: projectId,
  };
  return copyProjectSnapshot(previewProjectSnapshot);
}

// --- Chats -----------------------------------------------------------------------

export function listPreviewChats(
  projectId?: string | null,
): ChatThreadSummary[] {
  return getPreviewChats(projectId).map(copyChat);
}

export function createPreviewChatConversation(
  projectId: string,
  copyFromChatId?: string | null,
): ChatConversation {
  const chat = createPreviewChat(projectId, copyFromChatId);
  return {
    chat: copyChat(chat),
    messages: [],
    toolExecutions: [],
    messageParts: [],
  };
}

export function getPreviewChat(chatId: string, limit: number): ChatConversation {
  const chat = findPreviewChat(chatId);
  return {
    chat: copyChat(chat),
    messages: getPreviewMessages(chatId).slice(-limit).map(copyMessage),
    toolExecutions: [],
    messageParts: [],
  };
}

export function setPreviewChatModel(
  chatId: string,
  providerId: string,
  modelId: string,
): ChatThreadSummary {
  const chat = findPreviewChat(chatId);
  chat.providerId = providerId;
  chat.modelId = modelId;
  return copyChat(chat);
}

export function renamePreviewChat(
  chatId: string,
  title: string,
): ChatThreadSummary {
  const trimmed = title.trim();
  if (!trimmed) {
    throw new Error("name cannot be empty");
  }
  const chat = findPreviewChat(chatId);
  chat.title = trimmed.slice(0, 200);
  return copyChat(chat);
}

export function deletePreviewChat(chatId: string): void {
  findPreviewChat(chatId);
  const projectId = (previewChats ?? []).find(
    (chat) => chat.id === chatId,
  )?.projectId;
  previewChats = (previewChats ?? []).filter((chat) => chat.id !== chatId);
  previewMessages.delete(chatId);
  updatePreviewProjectChatCount(projectId);
}

export function renamePreviewProject(
  projectId: string,
  name: string,
): ProjectSnapshot {
  const trimmed = name.trim();
  if (!trimmed) {
    throw new Error("name cannot be empty");
  }
  const snapshot = getPreviewProjectSnapshot();
  previewProjectSnapshot = {
    ...snapshot,
    projects: snapshot.projects.map((project) =>
      project.id === projectId
        ? { ...project, name: trimmed.slice(0, 120), updatedAt: currentTimestamp() }
        : project,
    ),
  };
  return copyProjectSnapshot(previewProjectSnapshot);
}

export function deletePreviewProject(projectId: string): ProjectSnapshot {
  const snapshot = getPreviewProjectSnapshot();
  for (const chat of (previewChats ?? []).filter(
    (chat) => chat.projectId === projectId,
  )) {
    previewMessages.delete(chat.id);
  }
  previewChats = (previewChats ?? []).filter(
    (chat) => chat.projectId !== projectId,
  );
  previewProjectSnapshot = {
    projects: snapshot.projects.filter((project) => project.id !== projectId),
    activeProjectId:
      snapshot.activeProjectId === projectId ? null : snapshot.activeProjectId,
  };
  return copyProjectSnapshot(previewProjectSnapshot);
}

export function setPreviewProjectAppearance(
  projectId: string,
  icon: string | null,
  iconColor: string | null,
): ProjectSnapshot {
  const snapshot = getPreviewProjectSnapshot();
  previewProjectSnapshot = {
    ...snapshot,
    projects: snapshot.projects.map((project) =>
      project.id === projectId
        ? {
            ...project,
            icon: icon?.trim() || null,
            iconColor: iconColor?.trim() || null,
            updatedAt: currentTimestamp(),
          }
        : project,
    ),
  };
  return copyProjectSnapshot(previewProjectSnapshot);
}

export function setPreviewChatState(
  chatId: string,
  approvalMode: string | null,
  reasoning: string | null,
  fastMode: boolean | null,
  draft: string | null,
): ChatThreadSummary {
  const chat = findPreviewChat(chatId);
  chat.approvalMode = approvalMode?.trim() ? approvalMode : null;
  chat.reasoning = reasoning?.trim() ? reasoning : null;
  chat.fastMode = fastMode ? true : null;
  chat.draft = draft && draft.trim() ? draft : null;
  return copyChat(chat);
}

function getPreviewChats(projectId?: string | null) {
  previewChats ??= [];
  if (!projectId) {
    return previewChats;
  }
  return previewChats.filter((chat) => chat.projectId === projectId);
}

function createPreviewChat(projectId: string, copyFromChatId?: string | null) {
  if (!getPreviewProjectSnapshot().projects.some((project) => project.id === projectId)) {
    throw new Error(`project not found: ${projectId}`);
  }
  const source = copyFromChatId
    ? (previewChats ?? []).find((item) => item.id === copyFromChatId)
    : undefined;
  const now = currentTimestamp();
  const chat: ChatThreadSummary = {
    id: `preview-chat-${++previewChatSequence}`,
    projectId,
    title: "New chat",
    preview: "",
    messageCount: 0,
    // New chat inherits the source chat's settings (NOT its draft).
    providerId: source?.providerId ?? null,
    modelId: source?.modelId ?? null,
    approvalMode: source?.approvalMode ?? null,
    reasoning: source?.reasoning ?? null,
    fastMode: source?.fastMode ?? null,
    draft: null,
    createdAt: now,
    updatedAt: now,
  };

  previewChats = [chat, ...(previewChats ?? [])];
  previewMessages.set(chat.id, []);
  updatePreviewProjectChatCount(projectId);

  return chat;
}

function findPreviewChat(chatId: string) {
  const chat = (previewChats ?? []).find((item) => item.id === chatId);
  if (!chat) {
    throw new Error(`chat not found: ${chatId}`);
  }

  return chat;
}

function getPreviewMessages(chatId: string) {
  if (!previewMessages.has(chatId)) {
    previewMessages.set(chatId, []);
  }

  return previewMessages.get(chatId)!;
}

export function sendPreviewChatMessage(
  chatId: string | undefined,
  content: string,
  projectId?: string | null,
  fastMode = false,
): SendChatMessageResult {
  const message = content.trim();
  if (!message) {
    throw new Error("chat message cannot be empty");
  }

  const targetProjectId = projectId ?? getPreviewProjectSnapshot().activeProjectId;
  if (!targetProjectId) {
    throw new Error("select or open a project before starting a chat");
  }
  const chat = chatId ? findPreviewChat(chatId) : createPreviewChat(targetProjectId);
  if (chat.projectId && chat.projectId !== targetProjectId) {
    throw new Error("chat does not belong to the selected project");
  }
  const now = currentTimestamp();
  const userMessage: ChatMessage = {
    id: `preview-message-${++previewMessageSequence}`,
    chatId: chat.id,
    position: previewMessageSequence,
    role: "user",
    content: message,
    status: "complete",
    createdAt: now,
  };
  const assistantMessage: ChatMessage = {
    id: `preview-message-${++previewMessageSequence}`,
    chatId: chat.id,
    position: previewMessageSequence,
    role: "assistant",
    content:
      "Preview mode cannot reach the desktop LLM runtime. Run the Tauri app to test provider-backed chat.",
    status: "complete",
    createdAt: now,
    providerId: "codex",
    modelId: "gpt-5.5",
  };

  getPreviewMessages(chat.id).push(userMessage, assistantMessage);
  chat.title =
    chat.messageCount === 0 && chat.title === "New chat"
      ? derivePreviewTitle(message)
      : chat.title;
  chat.preview = derivePreviewPreview(message);
  chat.messageCount += 2;
  chat.fastMode = fastMode ? true : null;
  chat.updatedAt = now;
  previewChats = [
    chat,
    ...(previewChats ?? []).filter((item) => item.id !== chat.id),
  ];
  updatePreviewProjectChatCount(chat.projectId);

  return {
    runId: `preview-run-${previewMessageSequence}`,
    chat: copyChat(chat),
    userMessage: copyMessage(userMessage),
    assistantMessage: copyMessage(assistantMessage),
    removedMessageIds: [],
  };
}

export function editPreviewChatUserMessage(
  chatId: string,
  messageId: string,
  content: string,
): SendChatMessageResult {
  const message = content.trim();
  if (!message) {
    throw new Error("chat message cannot be empty");
  }

  const chat = findPreviewChat(chatId);
  const messages = getPreviewMessages(chat.id);
  const messageIndex = messages.findIndex((item) => item.id === messageId);
  if (messageIndex === -1) {
    throw new Error(`chat message not found: ${messageId}`);
  }
  const target = messages[messageIndex];
  if (target.role !== "user") {
    throw new Error("only user messages can be edited");
  }

  const now = currentTimestamp();
  const userMessage: ChatMessage = {
    ...target,
    content: message,
    status: "complete",
    createdAt: now,
  };
  const assistantMessage: ChatMessage = {
    id: `preview-message-${++previewMessageSequence}`,
    chatId: chat.id,
    position: userMessage.position + 1,
    role: "assistant",
    content:
      "Preview mode cannot reach the desktop LLM runtime. Run the Tauri app to test provider-backed chat.",
    status: "complete",
    createdAt: now,
    providerId: "codex",
    modelId: "gpt-5.5",
  };

  messages.splice(messageIndex, messages.length - messageIndex, userMessage, assistantMessage);
  chat.title = messageIndex === 0 ? derivePreviewTitle(message) : chat.title;
  chat.preview = derivePreviewPreview(message);
  chat.messageCount = messages.length;
  chat.updatedAt = now;
  previewChats = [
    chat,
    ...(previewChats ?? []).filter((item) => item.id !== chat.id),
  ];
  updatePreviewProjectChatCount(chat.projectId);

  return {
    runId: `preview-run-${previewMessageSequence}`,
    chat: copyChat(chat),
    userMessage: copyMessage(userMessage),
    assistantMessage: copyMessage(assistantMessage),
    removedMessageIds: [],
  };
}

export function branchPreviewChatFromMessage(
  chatId: string,
  messageId: string,
): ChatConversation {
  const sourceChat = findPreviewChat(chatId);
  const sourceMessages = getPreviewMessages(sourceChat.id);
  const messageIndex = sourceMessages.findIndex((item) => item.id === messageId);
  if (messageIndex === -1) {
    throw new Error(`chat message not found: ${messageId}`);
  }
  if (sourceMessages[messageIndex].role !== "assistant") {
    throw new Error("chat branches can only start from assistant messages");
  }

  const now = currentTimestamp();
  const chat: ChatThreadSummary = {
    id: `preview-chat-${++previewChatSequence}`,
    projectId: sourceChat.projectId,
    title: truncatePreview(`${sourceChat.title} branch`, 64),
    preview: derivePreviewPreview(sourceMessages[messageIndex].content),
    messageCount: messageIndex + 1,
    createdAt: now,
    updatedAt: now,
  };
  const messages = sourceMessages.slice(0, messageIndex + 1).map((message) => ({
    ...copyMessage(message),
    id: `preview-message-${++previewMessageSequence}`,
    chatId: chat.id,
  }));

  previewChats = [chat, ...(previewChats ?? [])];
  previewMessages.set(chat.id, messages);
  updatePreviewProjectChatCount(chat.projectId);

  return {
    chat: copyChat(chat),
    messages: messages.map(copyMessage),
    toolExecutions: [],
    messageParts: [],
  };
}

function derivePreviewTitle(content: string) {
  return truncatePreview(compactPreview(content), 64) || "New chat";
}

function derivePreviewPreview(content: string) {
  return truncatePreview(compactPreview(content), 140);
}

function compactPreview(content: string) {
  return content.split(/\s+/).filter(Boolean).join(" ");
}

function truncatePreview(content: string, maxLength: number) {
  return content.length > maxLength
    ? `${content.slice(0, maxLength).trimEnd()}...`
    : content;
}

function deriveProjectName(path: string) {
  const normalized = path.replace(/\\/g, "/").replace(/\/+$/, "");
  return normalized.split("/").pop() || normalized || "Project";
}

function updatePreviewProjectChatCount(projectId?: string | null) {
  if (!projectId || !previewProjectSnapshot) {
    return;
  }

  previewProjectSnapshot = {
    ...previewProjectSnapshot,
    projects: previewProjectSnapshot.projects.map((project) =>
      project.id === projectId
        ? {
            ...project,
            chatCount: (previewChats ?? []).filter(
              (chat) => chat.projectId === projectId,
            ).length,
            updatedAt: currentTimestamp(),
          }
        : project,
    ),
  };
}

function copyProjectSnapshot(snapshot: ProjectSnapshot): ProjectSnapshot {
  return {
    activeProjectId: snapshot.activeProjectId ?? null,
    projects: snapshot.projects.map((project) => ({ ...project })),
  };
}

function copyChat(chat: ChatThreadSummary): ChatThreadSummary {
  return { ...chat };
}

function copyMessage(message: ChatMessage): ChatMessage {
  return { ...message };
}

// --- Prompt preview ---------------------------------------------------------------

export function getPreviewPromptPreview(chatId: string): PromptPreview {
  const chat = findPreviewChat(chatId);
  const snapshot = getPreviewConnectorSettings();
  const providerId = chat.providerId?.trim() || snapshot.selectedModel.providerId;
  const modelId = chat.modelId?.trim() || snapshot.selectedModel.modelId;
  const project = getPreviewProjectSnapshot().projects.find(
    (item) => item.id === chat.projectId,
  );
  const sections: PromptPreviewSection[] = [
    {
      id: "core.base",
      source: "core",
      priority: 0,
      locked: true,
      content:
        "Preview mode prompt. Run the desktop app to inspect the exact Core prompt bundle.",
    },
  ];

  if (project) {
    sections.push({
      id: "core.project",
      source: "core",
      priority: 10,
      locked: true,
      content: `Active project:\n- Project id: ${project.id}\n- Name: ${project.name}\n- Root directory: ${project.path}`,
    });
  }

  const personalization = getPreviewPersonalization();
  const personalized = [
    ["global", personalization.global] as const,
    [
      "provider",
      personalization.providers.find((item) => item.providerId === providerId)
        ?.content ?? "",
    ] as const,
    [
      "model",
      personalization.models.find(
        (item) => item.providerId === providerId && item.modelId === modelId,
      )?.content ?? "",
    ] as const,
  ].filter(([, content]) => content.trim().length > 0);
  for (const [index, [scope, content]] of personalized.entries()) {
    sections.push({
      id: `user.${scope}`,
      source: "user",
      priority: 100 + index,
      locked: false,
      content,
    });
  }

  return {
    chatId,
    providerId,
    modelId,
    runtimeKind: "core_managed",
    projectId: project?.id ?? null,
    projectPath: project?.path ?? null,
    sections,
    renderedText: sections.map((section) => section.content).join("\n\n"),
  };
}

// --- Connector settings -------------------------------------------------------------

export function getPreviewConnectorSettingsSnapshot(): ConnectorSettingsSnapshot {
  return copyConnectorSettings(getPreviewConnectorSettings());
}

export function setPreviewSelectedModel(
  providerId: string,
  modelId: string,
): ConnectorSettingsSnapshot {
  const snapshot = getPreviewConnectorSettings();
  snapshot.selectedModel = {
    providerId,
    modelId,
    updatedAt: currentTimestamp(),
  };
  previewConnectorSettings = markSelectedModel(snapshot);
  return copyConnectorSettings(previewConnectorSettings);
}

export function setPreviewFeatureRoute(
  feature: string,
  providerId: string,
  modelId: string,
  options: JsonValue,
): ConnectorSettingsSnapshot {
  const snapshot = getPreviewConnectorSettings();
  const route: FeatureRoute = {
    feature,
    providerId,
    modelId,
    options,
    updatedAt: currentTimestamp(),
  };
  snapshot.featureRoutes = [
    ...snapshot.featureRoutes.filter((item) => item.feature !== feature),
    route,
  ];
  previewConnectorSettings = copyConnectorSettings(snapshot);
  return copyConnectorSettings(previewConnectorSettings);
}

export function setPreviewProviderEnabled(
  providerId: string,
  enabled: boolean,
): ConnectorSettingsSnapshot {
  const snapshot = getPreviewConnectorSettings();
  snapshot.providers = snapshot.providers.map((provider) =>
    provider.id === providerId ? { ...provider, enabled } : provider,
  );
  previewConnectorSettings = markSelectedModel(snapshot);
  return copyConnectorSettings(previewConnectorSettings);
}

export function savePreviewAdapterSettings(
  providerId: string,
  patch: Record<string, AdapterSettingPatchValue>,
): ConnectorSettingsSnapshot {
  const snapshot = getPreviewConnectorSettings();
  snapshot.providers = snapshot.providers.map((provider) =>
    provider.id === providerId && provider.adapterSettings
      ? {
          ...provider,
          adapterSettings: applyAdapterSettingsPatch(
            provider.adapterSettings,
            patch,
          ),
        }
      : provider,
  );
  previewConnectorSettings = markSelectedModel(snapshot);
  return copyConnectorSettings(previewConnectorSettings);
}

function getPreviewConnectorSettings() {
  previewConnectorSettings ??= markSelectedModel({
    providers: [
      {
        id: "codex",
        label: "Codex",
        enabled: true,
        runtimeKind: "core_managed",
        settingsSchema: {
          modelManagement: {
            kind: "remote_catalog",
            title: "Models",
            description:
              "Models are fetched from the Codex backend after you authorize.",
            addModelLabel: null,
            acceptsCustomModelIds: false,
          },
        },
        models: previewModels,
        refreshStatus: "ready",
        runtimeReady: true,
        runtimeStatus: {
          health: "healthy",
          active: 0,
          completed: 12,
          failed: 0,
          cancelled: 0,
          consecutiveFailures: 0,
          retryAfterMs: null,
          lastError: null,
        },
        selectedModelId: "gpt-5.5",
        authKind: "oauth_internal",
        authStatus: {
          kind: "authenticated",
          accountLabel: "preview",
          expiresAt: null,
          detail: null,
        },
        authenticated: true,
        adapterSettings: { fields: [], values: {}, secrets: {} },
        services: [
          {
            feature: "media.image.generate",
            label: "Image generation",
            mode: "sync",
            optionsSchema: {},
            models: [
              {
                id: "gpt-5.5",
                label: "GPT-5.5",
                recommended: true,
                optionsSchema: {},
              },
            ],
          },
        ],
      },
      {
        id: "openrouter",
        label: "OpenRouter",
        enabled: true,
        runtimeKind: "core_managed",
        settingsSchema: {
          modelManagement: {
            kind: "editable_list",
            title: "Models",
            description: "Add the OpenRouter models you want to use.",
            addModelLabel: "Add model",
            acceptsCustomModelIds: false,
          },
        },
        models: [],
        refreshStatus: "ready",
        runtimeReady: false,
        runtimeStatus: {
          health: "healthy",
          active: 0,
          completed: 0,
          failed: 0,
          cancelled: 0,
          consecutiveFailures: 0,
          retryAfterMs: null,
          lastError: null,
        },
        selectedModelId: null,
        authKind: "api_key",
        authStatus: {
          kind: "missing",
          accountLabel: null,
          expiresAt: null,
          detail: "OpenRouter API key is not configured",
        },
        authenticated: false,
        services: [],
        adapterSettings: {
          fields: [
            {
              key: "api_key",
              label: "OpenRouter API key",
              kind: "secret",
              required: true,
              options: [],
            },
            {
              key: "base_url",
              label: "Base URL (optional)",
              kind: "text",
              required: false,
              options: [],
            },
            {
              key: "models",
              label: "Models",
              kind: "string_list",
              required: false,
              options: [],
            },
            {
              key: "image_models",
              label: "Image models",
              kind: "string_list",
              required: false,
              options: [],
            },
          ],
          values: {},
          secrets: {
            api_key: { hasValue: false, fingerprint: null, last4: null },
          },
        },
      },
    ],
    selectedModel: {
      providerId: "codex",
      modelId: "gpt-5.5",
      updatedAt: currentTimestamp(),
    },
    featureRoutes: [
      {
        feature: "media.image.generate",
        providerId: "codex",
        modelId: "gpt-5.5",
        options: {},
        updatedAt: currentTimestamp(),
      },
    ],
  });

  return previewConnectorSettings;
}

function applyAdapterSettingsPatch(
  view: AdapterSettingsView,
  patch: Record<string, AdapterSettingPatchValue>,
): AdapterSettingsView {
  const values = { ...view.values };
  const secrets = { ...view.secrets };

  for (const field of view.fields) {
    const update = patch[field.key];
    if (!update) {
      continue;
    }

    if (field.kind === "secret") {
      delete values[field.key];

      if (update.action === "set") {
        secrets[field.key] = previewSecretState(update.value);
      } else if (update.action === "clear") {
        secrets[field.key] = {
          hasValue: false,
          fingerprint: null,
          last4: null,
        };
      }

      continue;
    }

    if (update.action === "set") {
      values[field.key] = update.value;
    } else if (update.action === "clear") {
      delete values[field.key];
    }
  }

  return { ...view, values, secrets };
}

function previewSecretState(value: string): SecretSettingState {
  return {
    hasValue: true,
    fingerprint: `preview-${hashSecret(value)}`,
    last4: value.length > 0 ? value.slice(-4) : null,
  };
}

function hashSecret(value: string) {
  let hash = 2166136261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }

  return (hash >>> 0).toString(16).padStart(8, "0");
}

const previewReasoning: ReasoningCapabilities = {
  supported: true,
  efforts: ["none", "minimal", "low", "medium", "high", "xhigh", "max"],
  options: [
    { id: "auto", label: "auto", recommended: true, config: {} },
    { id: "none", label: "none", recommended: false, config: { effort: "none" } },
    {
      id: "minimal",
      label: "minimal",
      recommended: false,
      config: { effort: "minimal" },
    },
    { id: "low", label: "low", recommended: false, config: { effort: "low" } },
    {
      id: "medium",
      label: "medium",
      recommended: false,
      config: { effort: "medium" },
    },
    { id: "high", label: "high", recommended: false, config: { effort: "high" } },
    {
      id: "xhigh",
      label: "xhigh",
      recommended: false,
      config: { effort: "xhigh" },
    },
    { id: "max", label: "max", recommended: false, config: { effort: "max" } },
  ],
  supportsBudget: false,
  supportsExclusion: false,
  supportsSummary: true,
};

const previewFastMode: FastModeCapabilities = {
  supported: true,
  label: "Fast",
  description: "1.5x speed, increased usage",
};

const previewModels: LlmModel[] = [
  {
    providerId: "codex",
    providerLabel: "OpenAI",
    id: "gpt-5.5",
    label: "GPT-5.5",
    family: "GPT-5",
    description: "Current high-capability Codex model for complex agent work.",
    capabilities: ["text", "reasoning", "tools", "code"],
    reasoning: previewReasoning,
    fastMode: previewFastMode,
    recommended: true,
  },
  {
    providerId: "codex",
    providerLabel: "OpenAI",
    id: "gpt-5.4",
    label: "GPT-5.4",
    family: "GPT-5",
    description: "Balanced Codex model for everyday coding sessions.",
    capabilities: ["text", "reasoning", "tools", "code"],
    reasoning: previewReasoning,
    fastMode: previewFastMode,
    recommended: false,
  },
  {
    providerId: "codex",
    providerLabel: "OpenAI",
    id: "gpt-5.4-mini",
    label: "GPT-5.4 Mini",
    family: "GPT-5",
    description:
      "Lower-latency Codex model for smaller edits and quick checks.",
    capabilities: ["text", "reasoning", "tools", "code"],
    reasoning: previewReasoning,
    recommended: false,
  },
  {
    providerId: "codex",
    providerLabel: "OpenAI",
    id: "gpt-5.3-codex",
    label: "GPT-5.3 Codex",
    family: "GPT-5 Codex",
    description:
      "Codex-specialized model kept for compatibility with existing workflows.",
    capabilities: ["text", "reasoning", "tools", "code"],
    reasoning: previewReasoning,
    recommended: false,
  },
  {
    providerId: "codex",
    providerLabel: "OpenAI",
    id: "gpt-5.3-codex-spark",
    label: "GPT-5.3 Codex Spark",
    family: "GPT-5 Codex",
    description: "Fast Codex-specialized model for lightweight agent tasks.",
    capabilities: ["text", "reasoning", "tools", "code"],
    reasoning: previewReasoning,
    recommended: false,
  },
  {
    providerId: "codex",
    providerLabel: "OpenAI",
    id: "gpt-5.2",
    label: "GPT-5.2",
    family: "GPT-5",
    description:
      "Older Codex-compatible model kept for account catalogs that still expose it.",
    capabilities: ["text", "reasoning", "tools", "code"],
    reasoning: previewReasoning,
    recommended: false,
  },
];

function markSelectedModel(
  snapshot: ConnectorSettingsSnapshot,
): ConnectorSettingsSnapshot {
  return {
    ...snapshot,
    providers: snapshot.providers.map((provider) => ({
      ...provider,
      selectedModelId:
        provider.id === snapshot.selectedModel.providerId
          ? snapshot.selectedModel.modelId
          : null,
    })),
  };
}

function copyConnectorSettings(
  snapshot: ConnectorSettingsSnapshot,
): ConnectorSettingsSnapshot {
  return {
    selectedModel: { ...snapshot.selectedModel },
    featureRoutes: snapshot.featureRoutes.map((route) => ({
      ...route,
      options: cloneUnknown(route.options),
    })),
    providers: snapshot.providers.map((provider) => ({
      ...provider,
      authStatus: { ...provider.authStatus },
      runtimeStatus: { ...provider.runtimeStatus },
      settingsSchema: {
        modelManagement: { ...provider.settingsSchema.modelManagement },
      },
      models: provider.models.map((model) => ({
        ...model,
        capabilities: [...model.capabilities],
        reasoning: model.reasoning
          ? {
              ...model.reasoning,
              efforts: [...model.reasoning.efforts],
              options: (model.reasoning.options ?? []).map((option) => ({
                ...option,
                config: { ...option.config },
              })),
            }
          : model.reasoning,
        fastMode: model.fastMode ? { ...model.fastMode } : model.fastMode,
      })),
      services: provider.services.map((service) => ({
        ...service,
        optionsSchema: cloneUnknown(service.optionsSchema),
        models: service.models.map((model) => ({
          ...model,
          optionsSchema: cloneUnknown(model.optionsSchema),
        })),
      })),
      adapterSettings: provider.adapterSettings
        ? {
            fields: provider.adapterSettings.fields.map((field) => ({
              ...field,
              options: field.options.map((option) => ({ ...option })),
            })),
            values: { ...provider.adapterSettings.values },
            secrets: copySecretSettings(provider.adapterSettings.secrets),
          }
        : provider.adapterSettings,
    })),
  };
}

function cloneUnknown<T>(value: T): T {
  if (value === undefined || value === null) {
    return value;
  }
  if (typeof structuredClone === "function") {
    return structuredClone(value);
  }
  return JSON.parse(JSON.stringify(value)) as T;
}

function copySecretSettings(
  secrets: Record<string, SecretSettingState> | undefined,
): Record<string, SecretSettingState> {
  return Object.fromEntries(
    Object.entries(secrets ?? {}).map(([key, state]) => [key, { ...state }]),
  );
}

// --- Personalization / tool policy / retention ----------------------------------------

export function getPreviewPersonalizationSnapshot(): PersonalizationSettings {
  return copyPersonalization(getPreviewPersonalization());
}

function getPreviewPersonalization(): PersonalizationSettings {
  previewPersonalization ??= { global: "", providers: [], models: [] };
  return previewPersonalization;
}

export function setPreviewPersonalization(
  providerId: string | null,
  modelId: string | null,
  content: string,
): PersonalizationSettings {
  const settings = getPreviewPersonalization();
  const trimmed = content.trim();
  if (providerId && modelId) {
    settings.models = settings.models.filter(
      (model) => !(model.providerId === providerId && model.modelId === modelId),
    );
    if (trimmed) {
      settings.models.push({ providerId, modelId, content: trimmed });
    }
  } else if (providerId) {
    settings.providers = settings.providers.filter(
      (provider) => provider.providerId !== providerId,
    );
    if (trimmed) {
      settings.providers.push({ providerId, content: trimmed });
    }
  } else {
    settings.global = trimmed;
  }
  previewPersonalization = settings;
  return copyPersonalization(settings);
}

function copyPersonalization(
  settings: PersonalizationSettings,
): PersonalizationSettings {
  return {
    global: settings.global,
    providers: settings.providers.map((provider) => ({ ...provider })),
    models: settings.models.map((model) => ({ ...model })),
  };
}

export function getPreviewToolPolicySnapshot(): ToolPolicySettings {
  return copyToolPolicy(getPreviewToolPolicy());
}

export function setPreviewToolPolicy(
  settings: ToolPolicySettings,
): ToolPolicySettings {
  previewToolPolicy = copyToolPolicy(settings);
  return copyToolPolicy(previewToolPolicy);
}

function getPreviewToolPolicy(): ToolPolicySettings {
  previewToolPolicy ??= { commandAllow: [], commandDeny: [], disabledTools: [] };
  return previewToolPolicy;
}

function copyToolPolicy(settings: ToolPolicySettings): ToolPolicySettings {
  return {
    commandAllow: [...settings.commandAllow],
    commandDeny: [...settings.commandDeny],
    disabledTools: [...settings.disabledTools],
  };
}

export function getPreviewChangeJournalRetention(): number {
  return previewChangeJournalRetention;
}

export function setPreviewChangeJournalRetention(value: number): number {
  previewChangeJournalRetention = value;
  return previewChangeJournalRetention;
}
