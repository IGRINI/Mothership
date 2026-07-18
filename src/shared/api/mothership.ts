import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---------------------------------------------------------------------------
// Wire types — GENERATED from the Rust serde structs by ts-rs.
//
// `src/shared/api/generated/` is produced by `npm run types:gen` (which runs the
// crates' `#[ts(export)]` export tests). DO NOT hand-edit those files or
// re-declare these shapes here — a renamed Rust field now propagates to TS
// automatically, and `npm run types:check` fails CI if the committed output
// drifts from the Rust source. The whole generated surface is re-exported below
// so existing `import { X } from "../shared/api/mothership"` call sites keep
// working unchanged.
// ---------------------------------------------------------------------------
export type * from "./generated";
// The recursive any-JSON type ts-rs emits for `serde_json::Value` fields (route
// options, tool payloads, …). Re-exported so call sites get it from this module.
export type { JsonValue } from "./generated/serde_json/JsonValue";

// Generated types this module also uses internally (preview-mode datasets +
// function signatures). Imported by value-less type import; they are already
// re-exported above.
import type {
  ActiveRunSummary,
  AdapterSettingPatchValue,
  ChangeFileDiff,
  ChangeFileSummary,
  ChangeSetSummary,
  ChatConversation,
  ChatRunCancellationResult,
  ChatThreadSummary,
  ConnectorSettingsSnapshot,
  DashboardSnapshot,
  PersonalizationSettings,
  ProjectSnapshot,
  PromptPreview,
  ReasoningConfig,
  RevertOutcome,
  SendChatMessageResult,
  SidecarStatus,
  ToolApprovalAnswer,
  ToolArtifactRange,
  ToolExecutionAccepted,
  ToolExecutionCancellationResult,
  ToolExecutionRequest,
  ToolPolicySettings,
} from "./generated";
import type { JsonValue } from "./generated/serde_json/JsonValue";
// The in-memory fake backend serving every API call outside the Tauri runtime
// (browser preview). One import per operation keeps the dispatch one-line.
import {
  appendPreviewActivityEvent,
  branchPreviewChatFromMessage,
  createPreviewChatConversation,
  deletePreviewChat,
  deletePreviewProject,
  editPreviewChatUserMessage,
  getPreviewChangeJournalRetention,
  getPreviewChat,
  getPreviewConnectorSettingsSnapshot,
  getPreviewDashboardSnapshot,
  getPreviewPersonalizationSnapshot,
  getPreviewPromptPreview,
  getPreviewToolPolicySnapshot,
  listPreviewChats,
  listPreviewProjects,
  openPreviewProject,
  previewSidecarStatus,
  renamePreviewChat,
  renamePreviewProject,
  savePreviewAdapterSettings,
  selectPreviewProject,
  sendPreviewChatMessage,
  setPreviewChangeJournalRetention,
  setPreviewChatModel,
  setPreviewChatState,
  setPreviewFeatureRoute,
  setPreviewPersonalization,
  setPreviewProjectAppearance,
  setPreviewProviderEnabled,
  setPreviewResponseLanguage,
  setPreviewSelectedModel,
  setPreviewToolPolicy,
} from "./preview-backend";

// --- Name aliases: keep the TS-facing names whose Rust counterparts are spelled
// differently (the Rust structs use `...View` / `Auth...`). Pure re-exports of
// the generated types so existing imports of these names resolve. ----------
export type { AuthStatus as AdapterAuthStatus } from "./generated";
export type { AuthStatusKind as AdapterAuthStatusKind } from "./generated";
export type { AdapterSettingsFieldView as AdapterSettingsField } from "./generated";
export type { AdapterSettingsFieldOptionView as AdapterSettingsFieldOption } from "./generated";

// --- Hand-synced types -----------------------------------------------------
// These intentionally do NOT have a 1:1 generated equivalent and are maintained
// by hand. Keep them in sync with the backend manually if the backend changes.

// hand-synced: ts-rs — the Rust `ConnectorProviderSummary.auth_kind` field is a
// plain `String` (the adapter's declared scheme), so the generated type widens
// it to `string`. This narrowed union is the known set of values, kept for the
// UI/preview; it is a subset of `string` and assignable to `authKind`.
export type AdapterAuthKind =
  | "none"
  | "api_key"
  | "oauth_internal"
  | "external_process";

// hand-synced: ts-rs — likewise `AdapterSettingsFieldView.kind` is a Rust
// `String`, so the generated field type is `string`. This narrowed union mirrors
// the adapter-protocol `SettingsFieldKind` values for the settings-form UI.
export type AdapterSettingsFieldKind =
  | "text"
  | "secret"
  | "bool"
  | "string_list"
  | "model_visibility_list";

/**
 * Sidecar (agent core) health. The host emits `sidecar-status` transitions:
 * transient outages are auto-restarted with backoff; a `permanent` failure
 * means the restart budget is exhausted and a manual restart is required.
 * Payloads are normalized defensively (string or object forms accepted).
 *
 * hand-synced: ts-rs — there is no Rust struct for this. The shape is
 * synthesized host-side in {@link normalizeSidecarStatus} from a string or an
 * arbitrary JSON payload, so it has no serde source to derive from.
 */
export interface SidecarStatusEvent {
  state: string;
  permanent?: boolean;
  attempt?: number;
  maxAttempts?: number;
  detail?: string | null;
}

export function getDashboardSnapshot(): Promise<DashboardSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewDashboardSnapshot());
  }

  return invoke<DashboardSnapshot>("get_dashboard_snapshot");
}

export function appendActivityEvent(
  message: string,
): Promise<DashboardSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(appendPreviewActivityEvent(message));
  }

  return invoke<DashboardSnapshot>("append_activity_event", { message });
}

export function runSidecarStatus(): Promise<SidecarStatus> {
  if (!isTauriRuntime()) {
    return Promise.resolve(previewSidecarStatus());
  }

  return invoke<SidecarStatus>("run_sidecar_status");
}

export function listProjects(): Promise<ProjectSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(listPreviewProjects());
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

/** Sets the execution model for a specific chat (persisted + synced across
 * clients via the `chat-updated` event). Returns the updated chat summary. */
export function setChatModel(
  chatId: string,
  providerId: string,
  modelId: string,
): Promise<ChatThreadSummary> {
  if (!isTauriRuntime()) {
    return Promise.resolve(setPreviewChatModel(chatId, providerId, modelId));
  }

  return invoke<ChatThreadSummary>("set_chat_model", {
    chatId,
    providerId,
    modelId,
  });
}

/**
 * Persists a chat's per-chat session state — approval mode, reasoning option,
 * fast mode, and the unsent composer draft. Blank values clear that field. Returns the
 * refreshed summary.
 */
export function setChatState(
  chatId: string,
  approvalMode: string | null,
  reasoning: string | null,
  fastMode: boolean | null,
  draft: string | null,
): Promise<ChatThreadSummary> {
  if (!isTauriRuntime()) {
    return Promise.resolve(
      setPreviewChatState(chatId, approvalMode, reasoning, fastMode, draft),
    );
  }

  return invoke<ChatThreadSummary>("set_chat_state", {
    chatId,
    approvalMode,
    reasoning,
    fastMode,
    draft,
  });
}

export function listChats(
  limit = 100,
  projectId?: string | null,
): Promise<ChatThreadSummary[]> {
  if (!isTauriRuntime()) {
    return Promise.resolve(listPreviewChats(projectId));
  }

  return invoke<ChatThreadSummary[]>("list_chats", { limit, projectId });
}

export function createChat(
  projectId: string,
  copyFromChatId?: string | null,
): Promise<ChatConversation> {
  if (!isTauriRuntime()) {
    return Promise.resolve(
      createPreviewChatConversation(projectId, copyFromChatId),
    );
  }

  return invoke<ChatConversation>("create_chat", { projectId, copyFromChatId });
}

export function getChat(
  chatId: string,
  limit = 200,
): Promise<ChatConversation> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewChat(chatId, limit));
  }

  return invoke<ChatConversation>("get_chat", { chatId, limit });
}

export function getPromptPreview(chatId: string): Promise<PromptPreview> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewPromptPreview(chatId));
  }

  return invoke<PromptPreview>("get_prompt_preview", { chatId });
}

export function sendChatMessage(
  chatId: string | undefined,
  content: string,
  projectId?: string | null,
  reasoning?: ReasoningConfig | null,
  fastMode = false,
): Promise<SendChatMessageResult> {
  if (!isTauriRuntime()) {
    return Promise.resolve(sendPreviewChatMessage(chatId, content, projectId, fastMode));
  }

  return invoke<SendChatMessageResult>("send_chat_message", {
    chatId,
    projectId,
    content,
    reasoning,
    fastMode,
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

export function steerChatRun(runId: string, content: string): Promise<void> {
  if (!isTauriRuntime()) {
    return Promise.resolve();
  }

  return invoke<void>("steer_chat_run", { runId, content });
}

/** Every chat run currently executing, across all projects. Seeds the
 * agent-activity status pill; live updates then arrive as `chat-run-event`s. */
export function listActiveRuns(): Promise<ActiveRunSummary[]> {
  if (!isTauriRuntime()) {
    return Promise.resolve([]);
  }

  return invoke<ActiveRunSummary[]>("list_active_runs");
}

/** Renames a chat (synced to other clients via `chat-updated`). */
export function renameChat(
  chatId: string,
  title: string,
): Promise<ChatThreadSummary> {
  if (!isTauriRuntime()) {
    return Promise.resolve(renamePreviewChat(chatId, title));
  }

  return invoke<ChatThreadSummary>("rename_chat", { chatId, title });
}

/** Permanently deletes a chat and everything recorded under it. */
export function deleteChat(chatId: string): Promise<void> {
  if (!isTauriRuntime()) {
    deletePreviewChat(chatId);
    return Promise.resolve();
  }

  return invoke<void>("delete_chat", { chatId });
}

/** Renames a project's display name (the folder on disk is untouched). */
export function renameProject(
  projectId: string,
  name: string,
): Promise<ProjectSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(renamePreviewProject(projectId, name));
  }

  return invoke<ProjectSnapshot>("rename_project", { projectId, name });
}

/** Removes a project from Mothership with ALL its chats (disk untouched). */
export function deleteProject(projectId: string): Promise<ProjectSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(deletePreviewProject(projectId));
  }

  return invoke<ProjectSnapshot>("delete_project", { projectId });
}

/** Persists a project's sidebar icon (`emoji:…`/`lucide:…`) and accent color
 * (`#rrggbb`). `null` clears back to the defaults. */
export function setProjectAppearance(
  projectId: string,
  icon: string | null,
  iconColor: string | null,
): Promise<ProjectSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(
      setPreviewProjectAppearance(projectId, icon, iconColor),
    );
  }

  return invoke<ProjectSnapshot>("set_project_appearance", {
    projectId,
    icon,
    iconColor,
  });
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

/**
 * Lazily fetch a byte range of a tool call's durable output artifact. The Core
 * resolves `logRef` against its own tool-output store and refuses anything that
 * escapes it — the UI can never read an arbitrary path, nor the (possibly
 * changed) live file.
 */
export function getToolArtifactRange(
  toolCallId: string,
  logRef: string,
  offset: number,
  limit: number,
): Promise<ToolArtifactRange> {
  if (!isTauriRuntime()) {
    return Promise.resolve({
      content: "",
      offset,
      nextOffset: null,
      totalBytes: 0,
      eof: true,
    });
  }

  return invoke<ToolArtifactRange>("get_tool_artifact_range", {
    toolCallId,
    logRef,
    offset,
    limit,
  });
}

// --- Workspace Change Journal ----------------------------------------------

// ChangeOp, ChangeSetStatus, ConflictReason, ChangeSetEventKind,
// ChangeFileSummary, ChangeSetSummary, ChangeFileDiff, ChangeConflict,
// RevertOutcome, and ChangeSetEvent are generated from
// `mothership-core::changes` and re-exported via `./generated` above.

/** All change sets recorded in a chat (to hydrate a reopened conversation). */
export function getChatChangeSets(chatId: string): Promise<ChangeSetSummary[]> {
  if (!isTauriRuntime()) {
    return Promise.resolve([]);
  }
  return invoke<ChangeSetSummary[]>("get_chat_change_sets", { chatId });
}

/** Change sets attributed to one assistant message. */
export function getMessageChangeSummary(
  messageId: string,
): Promise<ChangeSetSummary[]> {
  if (!isTauriRuntime()) {
    return Promise.resolve([]);
  }
  return invoke<ChangeSetSummary[]>("get_message_change_summary", { messageId });
}

/**
 * Lazily fetch a window of a single change file's unified diff. `limit = 0`
 * returns the whole diff from `offset`. With `full`, the diff is rendered with
 * whole-file context (the entire file with edits marked in place) instead of
 * just the changed hunks.
 */
export function getChangeFileDiff(
  changeFileId: string,
  offset = 0,
  limit = 0,
  full = false,
): Promise<ChangeFileDiff> {
  if (!isTauriRuntime()) {
    return Promise.resolve({
      changeFileId,
      path: "",
      op: "M",
      isBinary: false,
      isLarge: false,
      lines: [],
      offset: 0,
      totalLines: 0,
      additions: 0,
      deletions: 0,
      unavailable: true,
    });
  }
  return invoke<ChangeFileDiff>("get_change_file_diff", {
    changeFileId,
    offset,
    limit,
    full,
  });
}

/**
 * A page of a change set's files, beyond the inline preview its summary carries.
 * `limit = 0` returns all remaining files from `offset`.
 */
export function listChangeSetFiles(
  changeSetId: string,
  offset = 0,
  limit = 0,
): Promise<ChangeFileSummary[]> {
  if (!isTauriRuntime()) {
    return Promise.resolve([]);
  }
  return invoke<ChangeFileSummary[]>("list_change_set_files", {
    changeSetId,
    offset,
    limit,
  });
}

/** Revert a change set (conflict-aware; Core refuses to clobber user edits). */
export function revertChangeSet(changeSetId: string): Promise<RevertOutcome> {
  if (!isTauriRuntime()) {
    return Promise.resolve({
      changeSet: {
        id: changeSetId,
        status: "reverted",
        toolFailed: false,
        fileCount: 0,
        additions: 0,
        deletions: 0,
        files: [],
        createdAt: "",
        updatedAt: "",
      },
      conflicts: [],
      reverted: true,
    });
  }
  return invoke<RevertOutcome>("revert_change_set", { changeSetId });
}

export function onSidecarStatus(
  handler: (status: SidecarStatusEvent) => void,
): Promise<() => void> {
  if (!isTauriRuntime()) {
    return Promise.resolve(() => {});
  }

  return listen<unknown>("sidecar-status", (event) =>
    handler(normalizeSidecarStatus(event.payload)),
  );
}

/** Current health, for seeding the UI on mount (events race `listen`). */
export function getSidecarHealth(): Promise<SidecarStatusEvent> {
  if (!isTauriRuntime()) {
    return Promise.resolve({ state: "ready" });
  }

  return invoke<unknown>("get_sidecar_health").then(normalizeSidecarStatus);
}

function normalizeSidecarStatus(payload: unknown): SidecarStatusEvent {
  if (typeof payload === "string") {
    return { state: payload };
  }
  if (payload && typeof payload === "object") {
    const record = payload as Record<string, unknown>;
    const state =
      typeof record.state === "string"
        ? record.state
        : typeof record.status === "string"
          ? record.status
          : "unknown";
    return {
      state,
      permanent: record.permanent === true,
      attempt: typeof record.attempt === "number" ? record.attempt : undefined,
      maxAttempts:
        typeof record.maxAttempts === "number" ? record.maxAttempts : undefined,
      detail: typeof record.detail === "string" ? record.detail : null,
    };
  }
  return { state: "unknown" };
}

/** Manually restart a dead sidecar; resolves when it is Ready again. */
export function restartSidecar(): Promise<void> {
  if (!isTauriRuntime()) {
    return Promise.resolve();
  }

  return invoke("restart_sidecar").then(() => undefined);
}

/**
 * Open an external URL in the system browser (http/https/mailto/tel). Keeps
 * the WebView on the app — assistant-rendered links must never navigate it.
 */
export function openExternalUrl(url: string): Promise<void> {
  if (!isTauriRuntime()) {
    window.open(url, "_blank", "noopener,noreferrer");
    return Promise.resolve();
  }

  return import("@tauri-apps/plugin-opener").then(({ openUrl }) =>
    openUrl(url),
  );
}

/**
 * Open a workspace path in the OS default application. The Core resolves the
 * path against the owning project's workspace root and refuses anything outside
 * it (capability + containment) — never a raw shell open of UI-supplied data.
 */
export function openToolPath(
  projectId: string | null | undefined,
  path: string,
): Promise<void> {
  if (!isTauriRuntime()) {
    return Promise.resolve();
  }

  return invoke<void>("open_tool_path", { projectId, path });
}

/**
 * Resolve a workspace path to a canonical absolute path after Core containment.
 * Use this before rendering local media in the UI; never convert raw model text
 * directly to a file URL.
 */
export function resolveToolPath(
  projectId: string | null | undefined,
  path: string,
): Promise<string> {
  if (!isTauriRuntime()) {
    return Promise.resolve(path);
  }

  return invoke<string>("resolve_tool_path", { projectId, path });
}

export function readImageDataUrl(
  projectId: string | null | undefined,
  path: string,
): Promise<string> {
  if (!isTauriRuntime()) {
    return Promise.resolve(path);
  }

  return invoke<string>("read_image_data_url", { projectId, path });
}

export function readImageDataUrlWithTimeout(
  projectId: string | null | undefined,
  path: string,
  timeoutMs = 8000,
): Promise<string> {
  if (!isTauriRuntime()) {
    return readImageDataUrl(projectId, path);
  }

  let timeoutId: number | undefined;
  return Promise.race([
    readImageDataUrl(projectId, path),
    new Promise<string>((_, reject) => {
      timeoutId = window.setTimeout(
        () => reject(new Error("image preview timed out")),
        timeoutMs,
      );
    }),
  ]).finally(() => {
    if (timeoutId !== undefined) {
      window.clearTimeout(timeoutId);
    }
  });
}

export function openArtifactPath(path: string): Promise<void> {
  if (!isTauriRuntime()) {
    return Promise.resolve();
  }

  return invoke<void>("open_artifact_path", { path });
}

export function revealArtifactPath(path: string): Promise<void> {
  if (!isTauriRuntime()) {
    return Promise.resolve();
  }

  return invoke<void>("reveal_artifact_path", { path });
}

/**
 * Reveal a workspace path in the OS file manager (Explorer / Finder). Same
 * Core-side resolution + containment as {@link openToolPath}.
 */
export function revealToolPath(
  projectId: string | null | undefined,
  path: string,
): Promise<void> {
  if (!isTauriRuntime()) {
    return Promise.resolve();
  }

  return invoke<void>("reveal_tool_path", { projectId, path });
}

export function getConnectorSettings(): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewConnectorSettingsSnapshot());
  }

  return invoke<ConnectorSettingsSnapshot>("get_connector_settings");
}

export function setSelectedModel(
  providerId: string,
  modelId: string,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(setPreviewSelectedModel(providerId, modelId));
  }

  return invoke<ConnectorSettingsSnapshot>("set_selected_model", {
    providerId,
    modelId,
  });
}

export function setFeatureRoute(
  feature: string,
  providerId: string,
  modelId: string,
  options: JsonValue = {},
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(
      setPreviewFeatureRoute(feature, providerId, modelId, options),
    );
  }

  return invoke<ConnectorSettingsSnapshot>("set_feature_route", {
    feature,
    providerId,
    modelId,
    options,
  });
}

export function setProviderEnabled(
  providerId: string,
  enabled: boolean,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(setPreviewProviderEnabled(providerId, enabled));
  }

  return invoke<ConnectorSettingsSnapshot>("set_provider_enabled", {
    providerId,
    enabled,
  });
}

export function saveAdapterSettings(
  providerId: string,
  patch: Record<string, AdapterSettingPatchValue>,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(savePreviewAdapterSettings(providerId, patch));
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
    return Promise.resolve(getPreviewConnectorSettingsSnapshot());
  }

  return invoke<ConnectorSettingsSnapshot>("authenticate_adapter", {
    providerId,
  });
}

export function cancelAuthenticateAdapter(
  providerId: string,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewConnectorSettingsSnapshot());
  }

  return invoke<ConnectorSettingsSnapshot>("cancel_authenticate_adapter", {
    providerId,
  });
}

export function logoutAdapter(
  providerId: string,
): Promise<ConnectorSettingsSnapshot> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewConnectorSettingsSnapshot());
  }

  return invoke<ConnectorSettingsSnapshot>("logout_adapter", { providerId });
}

export function getPersonalization(): Promise<PersonalizationSettings> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewPersonalizationSnapshot());
  }

  return invoke<PersonalizationSettings>("get_personalization");
}

/**
 * Save (or clear, when `content` is blank) the custom instruction for one scope:
 * global (`providerId`/`modelId` both null), a provider (`providerId` only), or
 * a provider+model (both). Returns the refreshed full view.
 */
export function setPersonalization(
  providerId: string | null,
  modelId: string | null,
  content: string,
): Promise<PersonalizationSettings> {
  if (!isTauriRuntime()) {
    return Promise.resolve(
      setPreviewPersonalization(providerId, modelId, content),
    );
  }

  return invoke<PersonalizationSettings>("set_personalization", {
    providerId,
    modelId,
    content,
  });
}

export function setResponseLanguage(
  languageId: string,
  customLanguage: string,
): Promise<PersonalizationSettings> {
  if (!isTauriRuntime()) {
    return Promise.resolve(setPreviewResponseLanguage(languageId, customLanguage));
  }

  return invoke<PersonalizationSettings>("set_response_language", {
    languageId,
    customLanguage,
  });
}

export function getToolPolicy(): Promise<ToolPolicySettings> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewToolPolicySnapshot());
  }

  return invoke<ToolPolicySettings>("get_tool_policy");
}

export function setToolPolicy(
  settings: ToolPolicySettings,
): Promise<ToolPolicySettings> {
  if (!isTauriRuntime()) {
    return Promise.resolve(setPreviewToolPolicy(settings));
  }

  return invoke<ToolPolicySettings>("set_tool_policy", { settings });
}

/**
 * Workspace change-journal retention: changes from the last N agent MESSAGES
 * are kept per project (one message may record hundreds of change sets — they
 * survive together). 0 = unlimited; pruning runs when a new set is recorded.
 */
export function getChangeJournalRetention(): Promise<number> {
  if (!isTauriRuntime()) {
    return Promise.resolve(getPreviewChangeJournalRetention());
  }

  return invoke<number>("get_change_journal_retention");
}

export function setChangeJournalRetention(value: number): Promise<number> {
  if (!isTauriRuntime()) {
    return Promise.resolve(setPreviewChangeJournalRetention(value));
  }

  return invoke<number>("set_change_journal_retention", { value });
}

export function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}
