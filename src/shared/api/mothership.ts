import { invoke } from "@tauri-apps/api/core";

export interface WorkspaceItem {
  id: number;
  kind: string;
  namespace: string;
  name: string;
  status: string;
  updatedAt: string;
}

export interface ActivityEvent {
  id: number;
  source: string;
  level: string;
  message: string;
  occurredAt: string;
}

export interface DashboardMetric {
  label: string;
  value: string;
  tone: string;
}

export interface SidecarStatus {
  healthy: boolean;
  databasePath: string;
  workspaceItems: number;
  activityEvents: number;
  databaseBytes: number;
}

export interface DashboardSnapshot {
  metrics: DashboardMetric[];
  workspaceItems: WorkspaceItem[];
  activityEvents: ActivityEvent[];
}

export type ChatMessageRole = "assistant" | "user";
export type ChatMessageStatus = "complete" | "failed" | "sending";

export interface ChatThreadSummary {
  id: string;
  title: string;
  preview: string;
  messageCount: number;
  createdAt: string;
  updatedAt: string;
}

export interface ChatMessage {
  id: string;
  chatId: string;
  position: number;
  role: ChatMessageRole;
  content: string;
  status: ChatMessageStatus;
  createdAt: string;
  /** For assistant messages: the provider/model that produced the reply, so the
   * UI can show the adapter icon + model name. Null for user messages. */
  providerId?: string | null;
  modelId?: string | null;
}

export interface ChatConversation {
  chat: ChatThreadSummary;
  messages: ChatMessage[];
}

export interface SendChatMessageResult {
  runId: string;
  chat: ChatThreadSummary;
  userMessage: ChatMessage;
  assistantMessage: ChatMessage;
}

export type ChatRunEventKind =
  | "started"
  | "transport_selected"
  | "delta"
  | "completed"
  | "failed";

export interface ChatRunEvent {
  runId: string;
  chatId: string;
  messageId: string;
  kind: ChatRunEventKind;
  delta?: string | null;
  message?: ChatMessage | null;
  chat?: ChatThreadSummary | null;
  transport?: string | null;
  error?: string | null;
}

export interface LlmModel {
  providerId: string;
  providerLabel: string;
  id: string;
  label: string;
  family: string;
  description: string;
  capabilities: string[];
  recommended: boolean;
}

export interface ConnectorSettingsSchema {
  modelManagement: {
    kind: "fixed_catalog" | "remote_catalog" | "editable_list";
    title: string;
    description: string;
    addModelLabel?: string | null;
  };
}

export interface SelectedLlmModel {
  providerId: string;
  modelId: string;
  updatedAt: string;
}

export type AdapterSettingsFieldKind = "text" | "secret" | "bool" | "string_list";

export interface AdapterSettingsField {
  key: string;
  label: string;
  kind: AdapterSettingsFieldKind;
  required: boolean;
}

export interface AdapterSettingsView {
  fields: AdapterSettingsField[];
  values: Record<string, string>;
}

export type AdapterAuthKind =
  | "none"
  | "api_key"
  | "oauth_internal"
  | "external_process";

export interface ConnectorProviderSummary {
  id: string;
  label: string;
  /** The adapter's own icon as a data URI, if it ships one. */
  icon?: string | null;
  settingsSchema: ConnectorSettingsSchema;
  models: LlmModel[];
  selectedModelId?: string | null;
  authKind: AdapterAuthKind;
  authenticated: boolean;
  adapterSettings?: AdapterSettingsView | null;
}

export interface ConnectorSettingsSnapshot {
  providers: ConnectorProviderSummary[];
  selectedModel: SelectedLlmModel;
}

export function getDashboardSnapshot(): Promise<DashboardSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewSnapshot());
  }

  return invoke<DashboardSnapshot>("get_dashboard_snapshot");
}

export function appendActivityEvent(
  message: string,
): Promise<DashboardSnapshot> {
  if (!isTauriRuntime()) {
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

    return Promise.resolve(previewSnapshot);
  }

  return invoke<DashboardSnapshot>("append_activity_event", { message });
}

export function runSidecarStatus(): Promise<SidecarStatus> {
  if (!isTauriRuntime()) {
    const snapshot = getPreviewSnapshot();
    return Promise.resolve({
      healthy: true,
      databasePath: "browser-preview.sqlite3",
      workspaceItems: snapshot.workspaceItems.length,
      activityEvents: snapshot.activityEvents.length,
      databaseBytes: 647168,
    });
  }

  return invoke<SidecarStatus>("run_sidecar_status");
}

export function listChats(limit = 100): Promise<ChatThreadSummary[]> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewChats().map(copyChat));
  }

  return invoke<ChatThreadSummary[]>("list_chats", { limit });
}

export function createChat(): Promise<ChatConversation> {
  if (!isTauriRuntime()) {
    const chat = createPreviewChat();
    return Promise.resolve({ chat: copyChat(chat), messages: [] });
  }

  return invoke<ChatConversation>("create_chat");
}

export function getChat(
  chatId: string,
  limit = 200,
): Promise<ChatConversation> {
  if (!isTauriRuntime()) {
    const chat = findPreviewChat(chatId);
    return Promise.resolve({
      chat: copyChat(chat),
      messages: getPreviewMessages(chatId).slice(-limit).map(copyMessage),
    });
  }

  return invoke<ChatConversation>("get_chat", { chatId, limit });
}

export function sendChatMessage(
  chatId: string | undefined,
  content: string,
): Promise<SendChatMessageResult> {
  if (!isTauriRuntime()) {
    return Promise.resolve(sendPreviewChatMessage(chatId, content));
  }

  return invoke<SendChatMessageResult>("send_chat_message", {
    chatId,
    content,
  });
}

/**
 * Re-runs the last failed assistant message in a chat. The failed message is
 * rolled back in place (no duplicate exchange) and re-streamed, so the caller
 * just merges the returned messages back in and listens for run events.
 */
export function retryChatMessage(
  chatId: string,
): Promise<SendChatMessageResult> {
  if (!isTauriRuntime()) {
    return Promise.reject(new Error("Retry requires the desktop app."));
  }

  return invoke<SendChatMessageResult>("retry_chat_message", { chatId });
}

export function getConnectorSettings(): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(
      copyConnectorSettings(getPreviewConnectorSettings()),
    );
  }

  return invoke<ConnectorSettingsSnapshot>("get_connector_settings");
}

export function setSelectedModel(
  providerId: string,
  modelId: string,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    const snapshot = getPreviewConnectorSettings();
    snapshot.selectedModel = {
      providerId,
      modelId,
      updatedAt: currentTimestamp(),
    };
    previewConnectorSettings = markSelectedModel(snapshot);
    return Promise.resolve(copyConnectorSettings(previewConnectorSettings));
  }

  return invoke<ConnectorSettingsSnapshot>("set_selected_model", {
    providerId,
    modelId,
  });
}

export function saveAdapterSettings(
  providerId: string,
  values: Record<string, string>,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    const snapshot = getPreviewConnectorSettings();
    snapshot.providers = snapshot.providers.map((provider) =>
      provider.id === providerId && provider.adapterSettings
        ? {
            ...provider,
            adapterSettings: {
              ...provider.adapterSettings,
              values: { ...values },
            },
          }
        : provider,
    );
    previewConnectorSettings = markSelectedModel(snapshot);
    return Promise.resolve(copyConnectorSettings(previewConnectorSettings));
  }

  return invoke<ConnectorSettingsSnapshot>("save_adapter_settings", {
    providerId,
    values,
  });
}

export function authenticateAdapter(
  providerId: string,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(copyConnectorSettings(getPreviewConnectorSettings()));
  }

  return invoke<ConnectorSettingsSnapshot>("authenticate_adapter", {
    providerId,
  });
}

export function cancelAuthenticateAdapter(
  providerId: string,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(copyConnectorSettings(getPreviewConnectorSettings()));
  }

  return invoke<ConnectorSettingsSnapshot>("cancel_authenticate_adapter", {
    providerId,
  });
}

export function logoutAdapter(
  providerId: string,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(copyConnectorSettings(getPreviewConnectorSettings()));
  }

  return invoke<ConnectorSettingsSnapshot>("logout_adapter", { providerId });
}

let previewSnapshot: DashboardSnapshot | null = null;
let previewChats: ChatThreadSummary[] | null = null;
const previewMessages = new Map<string, ChatMessage[]>();
let previewConnectorSettings: ConnectorSettingsSnapshot | null = null;
let previewChatSequence = 0;
let previewMessageSequence = 0;

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
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
        value: isTauriRuntime() ? "WAL" : "preview",
        tone: "storage",
      },
    ],
  };
}

function currentTimestamp() {
  return Math.floor(Date.now() / 1000).toString();
}

function getPreviewChats() {
  previewChats ??= [];
  return previewChats;
}

function createPreviewChat() {
  const now = currentTimestamp();
  const chat: ChatThreadSummary = {
    id: `preview-chat-${++previewChatSequence}`,
    title: "New chat",
    preview: "",
    messageCount: 0,
    createdAt: now,
    updatedAt: now,
  };

  previewChats = [chat, ...getPreviewChats()];
  previewMessages.set(chat.id, []);

  return chat;
}

function findPreviewChat(chatId: string) {
  const chat = getPreviewChats().find((item) => item.id === chatId);
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

function sendPreviewChatMessage(
  chatId: string | undefined,
  content: string,
): SendChatMessageResult {
  const message = content.trim();
  if (!message) {
    throw new Error("chat message cannot be empty");
  }

  const chat = chatId ? findPreviewChat(chatId) : createPreviewChat();
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
  chat.updatedAt = now;
  previewChats = [
    chat,
    ...getPreviewChats().filter((item) => item.id !== chat.id),
  ];

  return {
    runId: `preview-run-${previewMessageSequence}`,
    chat: copyChat(chat),
    userMessage: copyMessage(userMessage),
    assistantMessage: copyMessage(assistantMessage),
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

function copyChat(chat: ChatThreadSummary): ChatThreadSummary {
  return { ...chat };
}

function copyMessage(message: ChatMessage): ChatMessage {
  return { ...message };
}

function getPreviewConnectorSettings() {
  previewConnectorSettings ??= markSelectedModel({
    providers: [
      {
        id: "codex",
        label: "Codex",
        settingsSchema: {
          modelManagement: {
            kind: "remote_catalog",
            title: "Models",
            description:
              "Models are fetched from the Codex backend after you authorize.",
            addModelLabel: null,
          },
        },
        models: previewModels,
        selectedModelId: "gpt-5.5",
        authKind: "oauth_internal",
        authenticated: true,
        adapterSettings: { fields: [], values: {} },
      },
      {
        id: "openrouter",
        label: "OpenRouter",
        settingsSchema: {
          modelManagement: {
            kind: "editable_list",
            title: "Models",
            description: "Add the OpenRouter models you want to use.",
            addModelLabel: "Add model",
          },
        },
        models: [],
        selectedModelId: null,
        authKind: "api_key",
        authenticated: false,
        adapterSettings: {
          fields: [
            {
              key: "api_key",
              label: "OpenRouter API key",
              kind: "secret",
              required: true,
            },
            {
              key: "base_url",
              label: "Base URL (optional)",
              kind: "text",
              required: false,
            },
            {
              key: "models",
              label: "Models",
              kind: "string_list",
              required: false,
            },
          ],
          values: {},
        },
      },
    ],
    selectedModel: {
      providerId: "codex",
      modelId: "gpt-5.5",
      updatedAt: currentTimestamp(),
    },
  });

  return previewConnectorSettings;
}

const previewModels: LlmModel[] = [
  {
    providerId: "openai",
    providerLabel: "OpenAI",
    id: "gpt-5.5",
    label: "GPT-5.5",
    family: "GPT-5",
    description: "Current high-capability Codex model for complex agent work.",
    capabilities: ["text", "reasoning", "tools", "code"],
    recommended: true,
  },
  {
    providerId: "openai",
    providerLabel: "OpenAI",
    id: "gpt-5.4",
    label: "GPT-5.4",
    family: "GPT-5",
    description: "Balanced Codex model for everyday coding sessions.",
    capabilities: ["text", "reasoning", "tools", "code"],
    recommended: false,
  },
  {
    providerId: "openai",
    providerLabel: "OpenAI",
    id: "gpt-5.4-mini",
    label: "GPT-5.4 Mini",
    family: "GPT-5",
    description:
      "Lower-latency Codex model for smaller edits and quick checks.",
    capabilities: ["text", "reasoning", "tools", "code"],
    recommended: false,
  },
  {
    providerId: "openai",
    providerLabel: "OpenAI",
    id: "gpt-5.3-codex",
    label: "GPT-5.3 Codex",
    family: "GPT-5 Codex",
    description:
      "Codex-specialized model kept for compatibility with existing workflows.",
    capabilities: ["text", "reasoning", "tools", "code"],
    recommended: false,
  },
  {
    providerId: "openai",
    providerLabel: "OpenAI",
    id: "gpt-5.3-codex-spark",
    label: "GPT-5.3 Codex Spark",
    family: "GPT-5 Codex",
    description: "Fast Codex-specialized model for lightweight agent tasks.",
    capabilities: ["text", "reasoning", "tools", "code"],
    recommended: false,
  },
  {
    providerId: "openai",
    providerLabel: "OpenAI",
    id: "gpt-5.2",
    label: "GPT-5.2",
    family: "GPT-5",
    description:
      "Older Codex-compatible model kept for account catalogs that still expose it.",
    capabilities: ["text", "reasoning", "tools", "code"],
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
    providers: snapshot.providers.map((provider) => ({
      ...provider,
      settingsSchema: {
        modelManagement: { ...provider.settingsSchema.modelManagement },
      },
      models: provider.models.map((model) => ({
        ...model,
        capabilities: [...model.capabilities],
      })),
      adapterSettings: provider.adapterSettings
        ? {
            fields: provider.adapterSettings.fields.map((field) => ({
              ...field,
            })),
            values: { ...provider.adapterSettings.values },
          }
        : provider.adapterSettings,
    })),
  };
}
