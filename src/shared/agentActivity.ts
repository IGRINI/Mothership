// App-wide agent activity: which chat runs are executing right now (across ALL
// projects, not just the one this client is viewing) and which finished runs
// the user hasn't reviewed yet — messenger-style "unread" results.
//
// Fed by the broadcast `chat-run-event` / `tool-execution-event` streams plus a
// `list_active_runs` seed at startup (and after a sidecar restart), so a client
// connecting mid-run still sees agents working. Only top-level chat runs are
// tracked — provider-internal subagents never surface here.
//
// "Seen" state is deliberately CLIENT-LOCAL (localStorage), like the active
// project: each client reviews results independently.

import { createSignal } from "solid-js";
import { listen } from "@tauri-apps/api/event";

import {
  getChat,
  isTauriRuntime,
  listActiveRuns,
  listProjects,
  onSidecarStatus,
} from "./api/mothership";
import type {
  ActiveRunSummary,
  ChatRunEvent,
  ToolExecutionEvent,
  ToolKind,
} from "./api/mothership";
import type { JsonValue } from "./api/generated/serde_json/JsonValue";
import { rememberChat } from "./session";
import { commandPresentation } from "./toolCommandPresentation";

export interface ActiveAgent {
  runId: string;
  chatId: string;
  chatTitle: string;
  projectId?: string;
  projectName?: string;
  providerId?: string;
  modelId?: string;
  startedAtMs: number;
  /** Semantic activity, e.g. "Reading file" / "Writing" / "Thinking". */
  statusLabel: string;
  /** Short context for the status: a path or a command line. */
  statusDetail?: string;
  /** Blocked on a tool-approval decision — needs the user. */
  awaitingApproval: boolean;
}

export type AgentOutcome = "completed" | "failed" | "cancelled" | "interrupted";

export interface FinishedAgent {
  runId: string;
  chatId: string;
  chatTitle: string;
  projectId?: string;
  projectName?: string;
  outcome: AgentOutcome;
  finishedAtMs: number;
}

/** A click on an agent row: open this project + chat. Consumed by Dashboard. */
export interface AgentFocusRequest {
  token: number;
  projectId?: string;
  chatId: string;
}

const FINISHED_STORAGE_KEY = "mothership.agentActivity.finished.v1";
const FINISHED_CAP = 20;
/** Never reconcile-away agents younger than this: a `list_active_runs` snapshot
 * can race a run that emitted `started` but hasn't registered yet. */
const RECONCILE_GRACE_MS = 15_000;
const RESEED_INTERVAL_MS = 60_000;

const THINKING = "Thinking";
const WRITING = "Writing";
const WORKING = "Working";
const NEEDS_APPROVAL = "Needs approval";

const TOOL_LABELS: Partial<Record<ToolKind, string>> = {
  read_file: "Reading file",
  write_file: "Writing file",
  edit_file: "Editing file",
  apply_patch: "Applying patch",
  search_text: "Searching code",
  image_generate: "Generating image",
};

interface ToolState {
  label: string;
  detail?: string;
  state: "pending" | "waiting" | "active";
}

/** Non-reactive per-run bookkeeping; the published `ActiveAgent` only changes
 * when something the popover displays changes (so streaming deltas don't churn
 * the UI). */
interface RunTracker {
  tools: Map<string, ToolState>;
  writing: boolean;
  lastEventAtMs: number;
}

const [agentsById, setAgentsById] = createSignal<Record<string, ActiveAgent>>(
  {},
);
const [finishedById, setFinishedById] = createSignal<
  Record<string, FinishedAgent>
>({});
const [focusRequest, setFocusRequest] = createSignal<AgentFocusRequest>();

const trackers = new Map<string, RunTracker>();
let projectNames: Record<string, string> = {};
let viewedChatId: string | undefined;
let focusToken = 0;
let initialized = false;
let projectRefreshInFlight = false;

// --- Public reactive accessors ----------------------------------------------

/** Running agents: approval-blocked first, then longest-running first. */
export function activeAgents(): ActiveAgent[] {
  return Object.values(agentsById()).sort((a, b) => {
    if (a.awaitingApproval !== b.awaitingApproval) {
      return a.awaitingApproval ? -1 : 1;
    }
    return a.startedAtMs - b.startedAtMs;
  });
}

/** Finished-but-not-yet-reviewed agents, newest first. */
export function unseenFinishedAgents(): FinishedAgent[] {
  return Object.values(finishedById()).sort(
    (a, b) => b.finishedAtMs - a.finishedAtMs,
  );
}

export function activeAgentCount(): number {
  return Object.keys(agentsById()).length;
}

export function unseenFinishedCount(): number {
  return Object.keys(finishedById()).length;
}

export function anyAgentAwaitingApproval(): boolean {
  return Object.values(agentsById()).some((agent) => agent.awaitingApproval);
}

export function agentFocusRequest(): AgentFocusRequest | undefined {
  return focusRequest();
}

/** True while a run is executing in this chat (sidebar row spinner). */
export function chatHasRunningAgent(chatId: string): boolean {
  return Object.values(agentsById()).some((agent) => agent.chatId === chatId);
}

/** True when this chat's run is blocked on a tool approval (sidebar amber). */
export function chatAwaitingApproval(chatId: string): boolean {
  return Object.values(agentsById()).some(
    (agent) => agent.chatId === chatId && agent.awaitingApproval,
  );
}

/** The newest not-yet-reviewed outcome for a chat (sidebar unread dot). */
export function chatUnseenOutcome(chatId: string): AgentOutcome | undefined {
  let newest: FinishedAgent | undefined;
  for (const agent of Object.values(finishedById())) {
    if (
      agent.chatId === chatId &&
      (!newest || agent.finishedAtMs > newest.finishedAtMs)
    ) {
      newest = agent;
    }
  }
  return newest?.outcome;
}

/** Running / approval-blocked / awaiting-review agent counts for a project
 * (sidebar badges). `attention` is the subset of `running` that is blocked on
 * a tool approval. Agents whose project is unknown aren't attributed. */
export function projectAgentCounts(projectId: string): {
  running: number;
  attention: number;
  unseen: number;
} {
  let running = 0;
  let attention = 0;
  for (const agent of Object.values(agentsById())) {
    if (agent.projectId === projectId) {
      running += 1;
      if (agent.awaitingApproval) {
        attention += 1;
      }
    }
  }
  let unseen = 0;
  for (const agent of Object.values(finishedById())) {
    if (agent.projectId === projectId) {
      unseen += 1;
    }
  }
  return { running, attention, unseen };
}

// --- Public actions ----------------------------------------------------------

/** Ask the workspace to open this agent's project + chat. Writes the session
 * restore keys FIRST so even a cold Dashboard mount (e.g. settings view is
 * open) lands on the right chat. */
export function requestAgentFocus(agent: {
  projectId?: string;
  chatId: string;
}): void {
  if (agent.projectId) {
    rememberChat(agent.projectId, agent.chatId);
  }
  focusToken += 1;
  setFocusRequest({
    token: focusToken,
    projectId: agent.projectId,
    chatId: agent.chatId,
  });
}

/** Dashboard consumed (or superseded) the focus request. */
export function consumeAgentFocusRequest(token: number): void {
  setFocusRequest((current) =>
    current?.token === token ? undefined : current,
  );
}

/** Dashboard reports which chat is on screen; reviewing a chat marks its
 * finished agents seen (like opening a conversation clears unread). */
export function reportViewedChat(chatId: string | undefined): void {
  viewedChatId = chatId;
  markViewedChatSeen();
}

/** Drop one finished entry (e.g. dismissed from the popover). */
export function dismissFinishedAgent(runId: string): void {
  setFinishedById((current) => {
    if (!(runId in current)) {
      return current;
    }
    const next = { ...current };
    delete next[runId];
    return next;
  });
  persistFinished();
}

/** Mark every finished agent reviewed. */
export function clearFinishedAgents(): void {
  if (unseenFinishedCount() === 0) {
    return;
  }
  setFinishedById({});
  persistFinished();
}

/** The chat was deleted: drop its activity immediately (running agents are
 * cancelled by the backend; their terminal events for a gone chat are noise). */
export function forgetChatActivity(chatId: string): void {
  for (const agent of Object.values(agentsById())) {
    if (agent.chatId === chatId) {
      removeAgent(agent.runId);
    }
  }
  dropFinishedForChat(chatId);
}

/** The project was deleted: drop the activity of all its chats. */
export function forgetProjectActivity(projectId: string): void {
  for (const agent of Object.values(agentsById())) {
    if (agent.projectId === projectId) {
      removeAgent(agent.runId);
    }
  }
  setFinishedById((current) => {
    const entries = Object.values(current).filter(
      (entry) => entry.projectId !== projectId,
    );
    if (entries.length === Object.keys(current).length) {
      return current;
    }
    return Object.fromEntries(entries.map((entry) => [entry.runId, entry]));
  });
  persistFinished();
}

// --- Wiring ------------------------------------------------------------------

/** Idempotent app-start hook: attaches event listeners, restores the unread
 * list, and seeds in-flight runs. Outside the Tauri runtime (browser preview)
 * it seeds demo agents instead so the status pill is exercisable. */
export function initAgentActivity(): void {
  if (initialized) {
    return;
  }
  initialized = true;

  loadPersistedFinished();

  if (!isTauriRuntime()) {
    seedPreviewAgents();
    return;
  }

  void listen<ChatRunEvent>("chat-run-event", (event) => {
    applyRunEvent(event.payload);
  });
  void listen<ToolExecutionEvent>("tool-execution-event", (event) => {
    applyToolEvent(event.payload);
  });
  // After a sidecar restart, in-flight runs died with it and their terminal
  // events may never arrive — reconcile against the fresh registry.
  void onSidecarStatus((status) => {
    if (status.state === "ready") {
      void reconcileWithBackend();
    }
  });

  if (typeof document !== "undefined") {
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") {
        markViewedChatSeen();
      }
    });
  }

  void reconcileWithBackend();
  // Periodic self-heal: a missed terminal event must never leave a phantom
  // agent in the status bar forever.
  window.setInterval(() => {
    void reconcileWithBackend();
  }, RESEED_INTERVAL_MS);
}

// --- Event application --------------------------------------------------------

function applyRunEvent(event: ChatRunEvent): void {
  switch (event.kind) {
    case "started": {
      // A new run supersedes any tracked run on the same chat (retry/edit) and
      // any unread result for it — the chat is live again.
      const stale = Object.values(agentsById()).filter(
        (agent) => agent.chatId === event.chatId && agent.runId !== event.runId,
      );
      for (const agent of stale) {
        removeAgent(agent.runId);
      }
      dropFinishedForChat(event.chatId);
      upsertAgentFromEvent(event);
      break;
    }
    case "transport_selected":
      touchTracker(event.runId);
      break;
    case "delta": {
      ensureAgentTracked(event);
      const tracker = trackers.get(event.runId);
      if (tracker && !tracker.writing) {
        tracker.writing = true;
        publishStatus(event.runId);
      }
      touchTracker(event.runId);
      break;
    }
    case "tool_call":
      ensureAgentTracked(event);
      touchTracker(event.runId);
      break;
    case "completed":
    case "failed":
    case "cancelled":
      finishAgent(event.runId, event.chatId, event.kind);
      break;
  }
}

function applyToolEvent(event: ToolExecutionEvent): void {
  const runId = event.runId ?? undefined;
  if (!runId) {
    return;
  }
  const tracker = trackers.get(runId);
  if (!tracker) {
    // Tool event for a run we don't know (e.g. manual tool runs) — ignore.
    return;
  }

  switch (event.kind) {
    case "queued":
    case "waiting_for_resource":
      tracker.tools.set(event.toolCallId, {
        ...toolPresentation(event),
        state: "pending",
      });
      break;
    case "permission_requested":
      tracker.tools.set(event.toolCallId, {
        ...toolPresentation(event),
        state: "waiting",
      });
      break;
    case "started":
    case "output": {
      const existing = tracker.tools.get(event.toolCallId);
      // Re-insert on start so Map insertion order tracks recency.
      if (existing?.state !== "active") {
        tracker.tools.delete(event.toolCallId);
      }
      tracker.tools.set(event.toolCallId, {
        ...toolPresentation(event, tracker.tools.get(event.toolCallId)),
        state: "active",
      });
      tracker.writing = false;
      break;
    }
    case "permission_denied":
    case "completed":
    case "failed":
    case "cancelled":
    case "timed_out":
    case "loop_blocked":
      tracker.tools.delete(event.toolCallId);
      tracker.writing = false;
      break;
  }

  touchTracker(runId);
  publishStatus(runId);
}

/** Label + detail for a tool event, falling back to whatever we knew before
 * (later events — e.g. `output` — often omit the command/path context). */
function toolPresentation(
  event: ToolExecutionEvent,
  previous?: ToolState,
): { label: string; detail?: string } {
  const kind = event.toolKind ?? undefined;
  if (kind === "run_command" || event.command) {
    const command = commandPresentation({
      command: event.command,
      payload: payloadRecord(event.payload),
      result: event.result,
      output: event.chunk,
    });
    const detail = command.target ?? command.commandLine;
    return {
      label: command.activityLabel,
      detail: detail ? truncate(detail, 72) : previous?.detail,
    };
  }

  const label = kind
    ? TOOL_LABELS[kind] ?? "Running tool"
    : previous?.label ?? "Running tool";

  let detail: string | undefined;
  if (event.command) {
    detail = truncate(
      [event.command.program, ...event.command.args].join(" ").trim(),
      72,
    );
  }
  detail ??= event.touchedPaths?.[0] ?? undefined;
  detail ??= payloadString(event.payload, "path");
  detail ??= payloadString(event.payload, "query");
  detail ??= previous?.detail;
  return { label, detail };
}

function truncate(value: string, max: number): string {
  return value.length > max ? `${value.slice(0, max - 1)}…` : value;
}

function payloadString(
  payload: JsonValue | null | undefined,
  key: string,
): string | undefined {
  if (payload && typeof payload === "object" && !Array.isArray(payload)) {
    const value = (payload as Record<string, JsonValue>)[key];
    if (typeof value === "string" && value.length > 0) {
      return value;
    }
  }
  return undefined;
}

function payloadRecord(
  payload: JsonValue | null | undefined,
): Record<string, unknown> | null {
  return payload && typeof payload === "object" && !Array.isArray(payload)
    ? (payload as Record<string, unknown>)
    : null;
}

// --- Agent lifecycle ----------------------------------------------------------

function upsertAgentFromEvent(event: ChatRunEvent): void {
  const chat = event.chat ?? undefined;
  ensureTracker(event.runId);
  const existing = agentsById()[event.runId];
  const projectId = chat?.projectId ?? existing?.projectId ?? undefined;
  upsertAgent({
    runId: event.runId,
    chatId: event.chatId,
    chatTitle: chat?.title ?? existing?.chatTitle ?? "Chat",
    projectId,
    projectName: projectId ? projectNames[projectId] : undefined,
    providerId: event.message?.providerId ?? existing?.providerId ?? undefined,
    modelId: event.message?.modelId ?? existing?.modelId ?? undefined,
    startedAtMs: existing?.startedAtMs ?? Date.now(),
    statusLabel: existing?.statusLabel ?? THINKING,
    statusDetail: existing?.statusDetail,
    awaitingApproval: existing?.awaitingApproval ?? false,
  });
}

/** A delta/tool event for an unknown run (this client attached mid-run):
 * track a stub now, resolve its chat/project names asynchronously. */
function ensureAgentTracked(event: ChatRunEvent): void {
  if (agentsById()[event.runId]) {
    return;
  }
  ensureTracker(event.runId);
  upsertAgent({
    runId: event.runId,
    chatId: event.chatId,
    chatTitle: "Chat",
    startedAtMs: Date.now(),
    statusLabel: WORKING,
    awaitingApproval: false,
  });
  void resolveAgentChat(event.runId, event.chatId);
}

async function resolveAgentChat(runId: string, chatId: string): Promise<void> {
  try {
    const conversation = await getChat(chatId, 1);
    const current = agentsById()[runId];
    if (!current) {
      return;
    }
    const projectId = conversation.chat.projectId ?? undefined;
    upsertAgent({
      ...current,
      chatTitle: conversation.chat.title,
      projectId,
      projectName: projectId ? projectNames[projectId] : undefined,
    });
    if (projectId && !projectNames[projectId]) {
      void refreshProjectNames();
    }
  } catch {
    // Keep the stub — the row still links to the chat by id.
  }
}

function upsertAgent(agent: ActiveAgent): void {
  setAgentsById((current) => ({ ...current, [agent.runId]: agent }));
  if (agent.projectId && !projectNames[agent.projectId]) {
    void refreshProjectNames();
  }
}

function removeAgent(runId: string): void {
  trackers.delete(runId);
  setAgentsById((current) => {
    if (!(runId in current)) {
      return current;
    }
    const next = { ...current };
    delete next[runId];
    return next;
  });
}

function finishAgent(
  runId: string,
  chatId: string,
  kind: "completed" | "failed" | "cancelled",
  knownAgent?: ActiveAgent,
): void {
  const agent = agentsById()[runId] ?? knownAgent;
  removeAgent(runId);
  if (!agent) {
    // Never tracked (e.g. run finished before this client learned of it) —
    // nothing meaningful to list.
    return;
  }

  // Finishing in the chat the user is actively looking at is "seen" instantly,
  // like a message arriving in an open conversation.
  const viewing =
    chatId === viewedChatId &&
    typeof document !== "undefined" &&
    document.visibilityState === "visible";
  if (viewing) {
    return;
  }

  addFinished({
    runId,
    chatId,
    chatTitle: agent.chatTitle,
    projectId: agent.projectId,
    projectName: agent.projectName,
    outcome: kind,
    finishedAtMs: Date.now(),
  });
}

function addFinished(finished: FinishedAgent): void {
  setFinishedById((current) => {
    const entries = Object.values({ ...current, [finished.runId]: finished })
      .sort((a, b) => b.finishedAtMs - a.finishedAtMs)
      .slice(0, FINISHED_CAP);
    return Object.fromEntries(entries.map((entry) => [entry.runId, entry]));
  });
  persistFinished();
}

function dropFinishedForChat(chatId: string): void {
  setFinishedById((current) => {
    const entries = Object.values(current).filter(
      (entry) => entry.chatId !== chatId,
    );
    if (entries.length === Object.keys(current).length) {
      return current;
    }
    return Object.fromEntries(entries.map((entry) => [entry.runId, entry]));
  });
  persistFinished();
}

function markViewedChatSeen(): void {
  if (!viewedChatId) {
    return;
  }
  if (typeof document !== "undefined" && document.visibilityState !== "visible") {
    return;
  }
  dropFinishedForChat(viewedChatId);
}

// --- Status derivation ---------------------------------------------------------

function ensureTracker(runId: string): RunTracker {
  let tracker = trackers.get(runId);
  if (!tracker) {
    tracker = { tools: new Map(), writing: false, lastEventAtMs: Date.now() };
    trackers.set(runId, tracker);
  }
  return tracker;
}

function touchTracker(runId: string): void {
  const tracker = trackers.get(runId);
  if (tracker) {
    tracker.lastEventAtMs = Date.now();
  }
}

/** Recompute the displayed status for a run and publish it only when it
 * actually changed (precedence: approval > active tool > writing > thinking). */
function publishStatus(runId: string): void {
  const agent = agentsById()[runId];
  const tracker = trackers.get(runId);
  if (!agent || !tracker) {
    return;
  }

  let label = tracker.writing ? WRITING : THINKING;
  let detail: string | undefined;
  let awaiting = false;

  const states = [...tracker.tools.values()];
  const waitingTool = states.find((tool) => tool.state === "waiting");
  const activeTool = [...states]
    .reverse()
    .find((tool) => tool.state === "active");
  if (waitingTool) {
    label = NEEDS_APPROVAL;
    detail = waitingTool.detail ?? waitingTool.label;
    awaiting = true;
  } else if (activeTool) {
    label = activeTool.label;
    detail = activeTool.detail;
  }

  if (
    agent.statusLabel !== label ||
    agent.statusDetail !== detail ||
    agent.awaitingApproval !== awaiting
  ) {
    upsertAgent({
      ...agent,
      statusLabel: label,
      statusDetail: detail,
      awaitingApproval: awaiting,
    });
  }
}

// --- Seeding / reconciliation ---------------------------------------------------

async function reconcileWithBackend(): Promise<void> {
  let runs: ActiveRunSummary[];
  try {
    runs = await listActiveRuns();
  } catch {
    // Sidecar still booting/restarting; the next ready event or interval tick
    // will retry.
    return;
  }
  await refreshProjectNames();

  const seen = new Set<string>();
  for (const run of runs) {
    seen.add(run.runId);
    const existing = agentsById()[run.runId];
    upsertAgent({
      runId: run.runId,
      chatId: run.chatId,
      chatTitle: run.chatTitle,
      projectId: run.projectId ?? undefined,
      projectName:
        (run.projectId ? projectNames[run.projectId] : undefined) ??
        run.projectName ??
        undefined,
      providerId: run.providerId,
      modelId: run.modelId,
      startedAtMs: run.startedAtMs,
      statusLabel: existing?.statusLabel ?? WORKING,
      statusDetail: existing?.statusDetail,
      awaitingApproval: existing?.awaitingApproval ?? false,
    });
    ensureTracker(run.runId);
  }

  // Anything we track that the registry no longer knows either finished while
  // we weren't listening or died with the sidecar — resolve its real outcome.
  for (const agent of Object.values(agentsById())) {
    if (seen.has(agent.runId)) {
      continue;
    }
    const tracker = trackers.get(agent.runId);
    if (
      tracker &&
      Date.now() - tracker.lastEventAtMs < RECONCILE_GRACE_MS
    ) {
      continue;
    }
    void resolveDroppedAgent(agent);
  }
}

/** The registry dropped this run without us seeing a terminal event. Read the
 * chat to learn how (or whether) its assistant message actually ended. */
async function resolveDroppedAgent(agent: ActiveAgent): Promise<void> {
  let outcome: AgentOutcome = "interrupted";
  try {
    const conversation = await getChat(agent.chatId, 30);
    const lastAssistant = [...conversation.messages]
      .reverse()
      .find((message) => message.role === "assistant");
    if (lastAssistant) {
      if (lastAssistant.status === "sending") {
        // Still marked running in the DB — sidecar death mid-run. Interrupted.
        outcome = "interrupted";
      } else if (lastAssistant.status === "complete") {
        outcome = "completed";
      } else if (lastAssistant.status === "cancelled") {
        outcome = "cancelled";
      } else {
        outcome = "failed";
      }
    }
  } catch {
    // Unreachable chat — report the conservative outcome.
  }
  if (!agentsById()[agent.runId]) {
    return;
  }
  removeAgent(agent.runId);
  if (outcome === "completed" || outcome === "failed") {
    finishAgent(agent.runId, agent.chatId, outcome, agent);
  } else {
    addFinishedInterruptedOrCancelled(agent, outcome);
  }
}

function addFinishedInterruptedOrCancelled(
  agent: ActiveAgent,
  outcome: AgentOutcome,
): void {
  const viewing =
    agent.chatId === viewedChatId &&
    typeof document !== "undefined" &&
    document.visibilityState === "visible";
  if (viewing) {
    return;
  }
  addFinished({
    runId: agent.runId,
    chatId: agent.chatId,
    chatTitle: agent.chatTitle,
    projectId: agent.projectId,
    projectName: agent.projectName,
    outcome,
    finishedAtMs: Date.now(),
  });
}

async function refreshProjectNames(): Promise<void> {
  if (projectRefreshInFlight) {
    return;
  }
  projectRefreshInFlight = true;
  try {
    const snapshot = await listProjects();
    projectNames = Object.fromEntries(
      snapshot.projects.map((project) => [project.id, project.name]),
    );
    // Backfill names onto agents created before the lookup landed.
    setAgentsById((current) => {
      let changed = false;
      const next: Record<string, ActiveAgent> = {};
      for (const [runId, agent] of Object.entries(current)) {
        const name = agent.projectId
          ? projectNames[agent.projectId]
          : undefined;
        if (name && agent.projectName !== name) {
          next[runId] = { ...agent, projectName: name };
          changed = true;
        } else {
          next[runId] = agent;
        }
      }
      return changed ? next : current;
    });
  } catch {
    // Names stay unresolved; rows fall back to chat titles only.
  } finally {
    projectRefreshInFlight = false;
  }
}

// --- Unread persistence ----------------------------------------------------------

function loadPersistedFinished(): void {
  try {
    const raw = localStorage.getItem(FINISHED_STORAGE_KEY);
    if (!raw) {
      return;
    }
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed)) {
      return;
    }
    const entries = parsed
      .filter(
        (entry): entry is FinishedAgent =>
          typeof entry === "object" &&
          entry !== null &&
          typeof (entry as FinishedAgent).runId === "string" &&
          typeof (entry as FinishedAgent).chatId === "string" &&
          typeof (entry as FinishedAgent).finishedAtMs === "number",
      )
      .slice(0, FINISHED_CAP);
    if (entries.length > 0) {
      setFinishedById(
        Object.fromEntries(entries.map((entry) => [entry.runId, entry])),
      );
    }
  } catch {
    // Corrupt cache — start clean.
  }
}

function persistFinished(): void {
  try {
    localStorage.setItem(
      FINISHED_STORAGE_KEY,
      JSON.stringify(unseenFinishedAgents()),
    );
  } catch {
    // Best effort; unread state is a convenience.
  }
}

// --- Formatting helpers (shared by the status bar UI) -----------------------------

export function formatElapsedMs(sinceMs: number, nowMs: number): string {
  const seconds = Math.max(0, Math.floor((nowMs - sinceMs) / 1000));
  if (seconds < 60) {
    return `${seconds}s`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m ${seconds % 60}s`;
  }
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

export function formatAgoMs(sinceMs: number, nowMs: number): string {
  const seconds = Math.max(0, Math.floor((nowMs - sinceMs) / 1000));
  if (seconds < 60) {
    return "just now";
  }
  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m ago`;
  }
  if (seconds < 86_400) {
    return `${Math.floor(seconds / 3600)}h ago`;
  }
  return `${Math.floor(seconds / 86_400)}d ago`;
}

// --- Browser-preview demo data ------------------------------------------------------

/** Outside Tauri there is no agent core; seed believable demo activity so the
 * pill + popover render (mirrors the preview datasets in `mothership.ts`). */
function seedPreviewAgents(): void {
  const now = Date.now();
  const demo: ActiveAgent[] = [
    {
      runId: "preview-run-1",
      chatId: "preview-chat-1",
      chatTitle: "Fix virtualized scroll jank",
      projectId: "preview-project-mothership",
      projectName: "Mothership",
      providerId: "anthropic",
      modelId: "claude-opus-4-8",
      startedAtMs: now - 4 * 60_000,
      statusLabel: "Editing file",
      statusDetail: "src/features/dashboard/Dashboard.tsx",
      awaitingApproval: false,
    },
    {
      runId: "preview-run-2",
      chatId: "preview-chat-2",
      chatTitle: "Migrate settings storage",
      projectId: "preview-project-mothership",
      projectName: "Mothership",
      providerId: "anthropic",
      modelId: "claude-sonnet-4-6",
      startedAtMs: now - 11 * 60_000,
      statusLabel: NEEDS_APPROVAL,
      statusDetail: "cargo install sqlx-cli",
      awaitingApproval: true,
    },
    {
      runId: "preview-run-3",
      chatId: "preview-chat-3",
      chatTitle: "Write release notes",
      projectId: "preview-project-mothership",
      projectName: "Mothership",
      providerId: "openrouter",
      modelId: "deepseek-v3",
      startedAtMs: now - 35_000,
      statusLabel: WRITING,
      awaitingApproval: false,
    },
  ];
  setAgentsById(
    Object.fromEntries(demo.map((agent) => [agent.runId, agent])),
  );
  setFinishedById({
    "preview-run-4": {
      runId: "preview-run-4",
      chatId: "preview-chat-4",
      chatTitle: "Audit credential vault",
      projectId: "preview-project-mothership",
      projectName: "Mothership",
      outcome: "completed",
      finishedAtMs: now - 9 * 60_000,
    },
    "preview-run-5": {
      runId: "preview-run-5",
      chatId: "preview-chat-5",
      chatTitle: "Refactor adapter pool",
      projectId: "preview-project-2",
      projectName: "CodeWhale",
      outcome: "failed",
      finishedAtMs: now - 26 * 60_000,
    },
  });
}
