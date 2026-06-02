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
export type ChatMessageStatus = "complete" | "cancelled" | "failed" | "sending";

export interface ProjectSummary {
  id: string;
  name: string;
  path: string;
  chatCount: number;
  createdAt: string;
  updatedAt: string;
  lastOpenedAt: string;
}

export interface ProjectSnapshot {
  projects: ProjectSummary[];
  activeProjectId?: string | null;
}

export interface ChatThreadSummary {
  id: string;
  projectId?: string | null;
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
  error?: string | null;
  /** For assistant messages: the provider/model that produced the reply, so the
   * UI can show the adapter icon + model name. Null for user messages. */
  providerId?: string | null;
  modelId?: string | null;
}

export interface ChatConversation {
  chat: ChatThreadSummary;
  messages: ChatMessage[];
  toolExecutions?: ToolExecutionRecord[];
  messageParts?: ChatMessagePart[];
}

export type ChatMessagePartKind = "text" | "tool";

export interface ChatMessagePart {
  id: number;
  chatId: string;
  messageId: string;
  kind: ChatMessagePartKind;
  text?: string | null;
  toolCallId?: string | null;
  createdAt: string;
}

export interface SendChatMessageResult {
  runId: string;
  chat: ChatThreadSummary;
  userMessage: ChatMessage;
  assistantMessage: ChatMessage;
}

export interface ChatRunCancellationResult {
  runId: string;
  accepted: boolean;
}

export type ChatRunEventKind =
  | "started"
  | "transport_selected"
  | "delta"
  | "tool_call"
  | "cancelled"
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
  toolCallId?: string | null;
  error?: string | null;
}

export interface ToolCommand {
  program: string;
  args: string[];
  env?: Record<string, string>;
}

export interface ToolOutputPolicy {
  memoryPreviewBytes: number;
  uiStreamBytesPerSec: number;
  agentTailBytes: number;
  spillToFile: boolean;
}

export interface ToolExecutionRequest {
  toolCallId: string;
  runId?: string | null;
  projectId?: string | null;
  cwd?: string | null;
  command: ToolCommand;
  timeoutMs?: number | null;
  outputPolicy?: ToolOutputPolicy;
}

export interface ToolExecutionAccepted {
  toolCallId: string;
}

export interface ToolExecutionCancellationResult {
  toolCallId: string;
  accepted: boolean;
}

export interface ToolApprovalAnswer {
  toolCallId: string;
  accepted: boolean;
}

export type ToolOutputStream = "stdout" | "stderr";

export type ToolExecutionStatus =
  | "completed"
  | "failed"
  | "cancelled"
  | "timed_out"
  | "permission_denied"
  | "loop_blocked";

export interface ToolExecutionResult {
  toolCallId: string;
  status: ToolExecutionStatus;
  exitCode?: number | null;
  stdoutPreview: string;
  stderrPreview: string;
  stdoutTail: string;
  stderrTail: string;
  stdoutBytes: number;
  stderrBytes: number;
  truncatedForDisplay: boolean;
  truncatedForAgent: boolean;
  logRef?: string | null;
  message?: string | null;
}

export type ToolExecutionEventKind =
  | "queued"
  | "permission_requested"
  | "permission_denied"
  | "waiting_for_resource"
  | "started"
  | "output"
  | "completed"
  | "failed"
  | "cancelled"
  | "timed_out"
  | "loop_blocked";

export interface ToolExecutionEvent {
  toolCallId: string;
  runId?: string | null;
  projectId?: string | null;
  command?: ToolCommand | null;
  kind: ToolExecutionEventKind;
  stream?: ToolOutputStream | null;
  chunk?: string | null;
  message?: string | null;
  result?: ToolExecutionResult | null;
}

export interface ToolExecutionRecord {
  toolCallId: string;
  runId?: string | null;
  chatId: string;
  messageId: string;
  projectId?: string | null;
  command?: ToolCommand | null;
  kind: ToolExecutionEventKind;
  message?: string | null;
  output: string;
  result?: ToolExecutionResult | null;
  createdAt: string;
  updatedAt: string;
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

export interface SecretSettingState {
  hasValue: boolean;
  fingerprint?: string | null;
  last4?: string | null;
}

export interface AdapterSettingsView {
  fields: AdapterSettingsField[];
  values: Record<string, string>;
  secrets: Record<string, SecretSettingState>;
}

export type AdapterSettingPatchValue =
  | { action: "set"; value: string }
  | { action: "clear" }
  | { action: "unchanged" };

export type AdapterAuthKind =
  | "none"
  | "api_key"
  | "oauth_internal"
  | "external_process";

export type ConnectorRefreshStatus =
  | "pending"
  | "refreshing"
  | "ready"
  | "failed";

export type AdapterAuthStatusKind =
  | "not_required"
  | "missing"
  | "configured"
  | "authenticated"
  | "expired"
  | "error";

export interface AdapterAuthStatus {
  kind: AdapterAuthStatusKind;
  accountLabel?: string | null;
  expiresAt?: string | null;
  detail?: string | null;
}

export type ProviderRuntimeHealth = "healthy" | "degraded" | "cooling_down";

export interface ProviderRuntimeStatus {
  health: ProviderRuntimeHealth;
  active: number;
  completed: number;
  failed: number;
  cancelled: number;
  consecutiveFailures: number;
  retryAfterMs?: number | null;
  lastError?: string | null;
}

export interface ConnectorProviderSummary {
  id: string;
  label: string;
  /** The adapter's own icon as a data URI, if it ships one. */
  icon?: string | null;
  settingsSchema: ConnectorSettingsSchema;
  models: LlmModel[];
  modelError?: string | null;
  refreshStatus: ConnectorRefreshStatus;
  runtimeReady: boolean;
  runtimeStatus: ProviderRuntimeStatus;
  selectedModelId?: string | null;
  authKind: AdapterAuthKind;
  authStatus: AdapterAuthStatus;
  authenticated: boolean;
  adapterSettings?: AdapterSettingsView | null;
}

export interface ConnectorSettingsSnapshot {
  providers: ConnectorProviderSummary[];
  selectedModel: SelectedLlmModel;
}

export type ConnectorSettingsEventKind =
  | "refresh_started"
  | "provider_updated"
  | "selected_model_changed"
  | "adapter_settings_saved"
  | "authentication_finished"
  | "authentication_cancelled"
  | "logged_out";

export interface ConnectorSettingsEvent {
  kind: ConnectorSettingsEventKind;
  snapshot: ConnectorSettingsSnapshot;
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

export function listProjects(): Promise<ProjectSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(copyProjectSnapshot(getPreviewProjectSnapshot()));
  }

  return invoke<ProjectSnapshot>("list_projects");
}

export function openProject(path: string): Promise<ProjectSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(openPreviewProject(path));
  }

  return invoke<ProjectSnapshot>("open_project", { path });
}

export function pickProjectDirectory(): Promise<string | undefined> {
  if (!isTauriRuntime()) {
    return Promise.resolve(undefined);
  }

  return invoke<string | null>("pick_project_directory").then((path) => path ?? undefined);
}

export function setActiveProject(projectId: string): Promise<ProjectSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(selectPreviewProject(projectId));
  }

  return invoke<ProjectSnapshot>("set_active_project", { projectId });
}

export function listChats(
  limit = 100,
  projectId?: string | null,
): Promise<ChatThreadSummary[]> {
  if (!isTauriRuntime()) {
    return Promise.resolve(
      getPreviewChats(projectId).map(copyChat),
    );
  }

  return invoke<ChatThreadSummary[]>("list_chats", { limit, projectId });
}

export function createChat(projectId: string): Promise<ChatConversation> {
  if (!isTauriRuntime()) {
    const chat = createPreviewChat(projectId);
    return Promise.resolve({ chat: copyChat(chat), messages: [] });
  }

  return invoke<ChatConversation>("create_chat", { projectId });
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
  projectId?: string | null,
): Promise<SendChatMessageResult> {
  if (!isTauriRuntime()) {
    return Promise.resolve(sendPreviewChatMessage(chatId, content, projectId));
  }

  return invoke<SendChatMessageResult>("send_chat_message", {
    chatId,
    projectId,
    content,
  });
}

export function editChatUserMessage(
  chatId: string,
  messageId: string,
  content: string,
): Promise<SendChatMessageResult> {
  if (!isTauriRuntime()) {
    return Promise.resolve(editPreviewChatUserMessage(chatId, messageId, content));
  }

  return invoke<SendChatMessageResult>("edit_chat_user_message", {
    chatId,
    messageId,
    content,
  });
}

export function branchChatFromMessage(
  chatId: string,
  messageId: string,
): Promise<ChatConversation> {
  if (!isTauriRuntime()) {
    return Promise.resolve(branchPreviewChatFromMessage(chatId, messageId));
  }

  return invoke<ChatConversation>("branch_chat_from_message", {
    chatId,
    messageId,
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

export function continueChatMessage(
  chatId: string,
): Promise<SendChatMessageResult> {
  if (!isTauriRuntime()) {
    return Promise.reject(new Error("Continue requires the desktop app."));
  }

  return invoke<SendChatMessageResult>("continue_chat_message", { chatId });
}

export function cancelChatRun(
  runId: string,
): Promise<ChatRunCancellationResult> {
  if (!isTauriRuntime()) {
    return Promise.resolve({ runId, accepted: true });
  }

  return invoke<ChatRunCancellationResult>("cancel_chat_run", { runId });
}

export function runToolCommand(
  request: ToolExecutionRequest,
): Promise<ToolExecutionAccepted> {
  if (!isTauriRuntime()) {
    return Promise.resolve({ toolCallId: request.toolCallId });
  }

  return invoke<ToolExecutionAccepted>("run_tool_command", { request });
}

export function approveToolExecution(
  toolCallId: string,
  approved: boolean,
  reason?: string,
): Promise<ToolApprovalAnswer> {
  if (!isTauriRuntime()) {
    return Promise.resolve({ toolCallId, accepted: true });
  }

  return invoke<ToolApprovalAnswer>("approve_tool_execution", {
    toolCallId,
    approved,
    reason,
  });
}

export function cancelToolExecution(
  toolCallId: string,
): Promise<ToolExecutionCancellationResult> {
  if (!isTauriRuntime()) {
    return Promise.resolve({ toolCallId, accepted: true });
  }

  return invoke<ToolExecutionCancellationResult>("cancel_tool_execution", {
    toolCallId,
  });
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
  patch: Record<string, AdapterSettingPatchValue>,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
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
    return Promise.resolve(copyConnectorSettings(previewConnectorSettings));
  }

  return invoke<ConnectorSettingsSnapshot>("save_adapter_settings", {
    providerId,
    values: patch,
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
let previewProjectSnapshot: ProjectSnapshot | null = null;
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

function openPreviewProject(path: string) {
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

function selectPreviewProject(projectId: string) {
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

function getPreviewChats(projectId?: string | null) {
  previewChats ??= [];
  if (!projectId) {
    return previewChats;
  }
  return previewChats.filter((chat) => chat.projectId === projectId);
}

function createPreviewChat(projectId: string) {
  if (!getPreviewProjectSnapshot().projects.some((project) => project.id === projectId)) {
    throw new Error(`project not found: ${projectId}`);
  }
  const now = currentTimestamp();
  const chat: ChatThreadSummary = {
    id: `preview-chat-${++previewChatSequence}`,
    projectId,
    title: "New chat",
    preview: "",
    messageCount: 0,
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

function sendPreviewChatMessage(
  chatId: string | undefined,
  content: string,
  projectId?: string | null,
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
  };
}

function editPreviewChatUserMessage(
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
  };
}

function branchPreviewChatFromMessage(
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
      authStatus: { ...provider.authStatus },
      runtimeStatus: { ...provider.runtimeStatus },
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
            secrets: copySecretSettings(provider.adapterSettings.secrets),
          }
        : provider.adapterSettings,
    })),
  };
}

function copySecretSettings(
  secrets: Record<string, SecretSettingState> | undefined,
): Record<string, SecretSettingState> {
  return Object.fromEntries(
    Object.entries(secrets ?? {}).map(([key, state]) => [key, { ...state }]),
  );
}
