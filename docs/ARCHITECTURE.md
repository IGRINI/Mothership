# Mothership Architecture

Mothership is a high-performance desktop workspace for local coding agents.

The architecture target is:

```text
Modular Monolith + Hexagonal Architecture + CQRS-lite + Event-driven runtime + Thin UI clients
```

In practical terms:

```text
one powerful Agent Core
multiple thin clients: Desktop / Phone / Web
all product logic lives in Core
UI sends commands, reads queries, and listens to events
```

The most important architectural idea:

```text
Agent Core is the product.
Desktop UI and Phone UI are clients.
```

## System Shape

```text
┌────────────────────┐
│ Desktop UI          │
│ Tauri / WebView     │
└─────────┬──────────┘
          │
┌─────────▼──────────┐
│ Agent API           │
│ commands / queries  │
│ event stream        │
└─────────┬──────────┘
          │
┌─────────▼──────────┐
│ Agent Core          │
│ application logic   │
└─────────┬──────────┘
          │
┌─────────▼──────────┐
│ Storage / Tools     │
│ SQLite / files      │
│ git / terminal / fs │
└────────────────────┘
```

Future phone and web clients must connect to the same Agent API contract. Core now runs **out of the Tauri process**, in a long-running sidecar: the desktop host is a thin client that speaks a newline-delimited JSON protocol to it (see [Provider Auth](PROVIDER_AUTH.md) and the sidecar protocol in `mothership-core::ipc`). This makes the "Core can become a separate local server" property real and enforced, not aspirational — a boundary you actually run as a process can't silently rot.

## Layers

```text
Presentation
  Desktop UI
  Mobile UI
  Web/PWA UI

API
  Commands
  Queries
  Events

Application
  Use cases:
    SendMessage
    LoadChat
    StartRun
    ApproveToolCall
    SearchHistory

Domain
  Chat
  Message
  Run
  ToolCall
  Project
  RemoteSession
  Permission

Infrastructure
  SQLite
  File storage
  LLM providers
  Terminal
  Git
  File system
  Relay
```

## Modular Monolith

Do not start with microservices. Mothership should be one Core with strong internal module boundaries.

Target Core modules:

```text
agent-core/
  chat/
  runs/
  tools/
  projects/
  storage/
  terminal/
  remote/
  auth/
  llm/
```

Module ownership:

- `chat`: chats, messages, conversation history.
- `runs`: active and historical agent executions.
- `tools`: tool calls, approvals, execution records.
- `projects`: project registry, files, indexing.
- `terminal`: terminal sessions and command execution.
- `remote`: phone/web sessions, synchronization, event fanout.
- `auth`: local/remote access control and provider authorization metadata.
- `storage`: SQLite repositories and file/blob storage.
- `llm`: model providers, provider gateway access, and streaming adapters.

Each module should expose a small application-facing API. Avoid one giant `AgentService`.

Provider authorization has its own architectural decision record:
[Provider Auth](PROVIDER_AUTH.md). The key rule is that Core stays provider-agnostic:
OpenAI Codex OAuth, Anthropic OAuth, future API keys, and future providers are adapters
behind one ProviderAuth Core contract.

## Hexagonal Core

Core logic must not depend directly on Tauri, SQLite, OpenAI, Anthropic, terminal processes, git, file system APIs, or relay servers.

Core talks through ports:

```text
LlmProvider
ProviderAuthAdapter
CredentialVault
ProviderGateway
ChatRepository
RunRepository
MessageRepository
EventLog
FileSystem
Terminal
GitClient
RemoteTransport
Notifier
Clock
IdGenerator
```

Infrastructure implements those ports:

```text
SqliteChatRepository
SqliteRunRepository
LocalFileSystem
ProcessTerminal
OpenAiProvider
AnthropicProvider
OpenAiCodexOAuthAdapter
AnthropicOAuthTokenPasteAdapter
OsCredentialVault
LocalRemoteTransport
```

This keeps Core testable and makes it possible to:

- change LLM providers;
- add OAuth/API-key auth methods without changing chat or run logic;
- move Core into a sidecar/server;
- add phone remote control;
- test use cases without Tauri or real file system access;
- keep UI clients thin.

## CQRS-lite

Separate state-changing operations from read-only operations.

Commands mutate state:

```text
CreateChat
SendMessage
CancelRun
ApproveToolCall
RejectToolCall
RenameChat
ArchiveChat
StartProjectIndexing
StopTerminalSession
```

Queries read state:

```text
ListChats
GetChatMessages
GetRunStatus
SearchChats
ListProjects
GetToolCallOutput
```

Clients should use the same contract:

```text
command: SendMessage
query: GetChatMessages
subscribe: ChatEvents
```

This is required for desktop and phone to behave as peers.

## Event-driven Runtime

Every important runtime change should become an event.

Core event examples:

```text
chat.created
message.created
message.updated
run.started
run.delta_received
tool.call_requested
tool.call_approved
tool.call_started
tool.call_finished
run.finished
run.failed
```

UI clients should not poll Core for progress. They subscribe to event streams and update only the visible state.

```text
Desktop UI ─┐
            ├─ Agent Core events
Phone UI ───┘
```

This enables remote control, reconnects, shared live state, and crash recovery.

## Append-only Event Log

Core should store an append-only history of important actions, not only final chat messages.

Useful persisted event categories:

```text
message created
assistant started
assistant streamed chunk
tool call requested
tool approved
file edited
run completed
run failed
```

The UI does not need to be pure event-sourced from day one. The event log exists to support:

- chat reconstruction;
- timelines;
- tool call audit history;
- debug replay;
- crash recovery;
- remote clients catching up after reconnect.

## Actor and Job Model

Agent work cannot be a single blocking function call.

Runtime actors/jobs:

```text
RunActor
TerminalActor
IndexerActor
RemoteSessionActor
ToolExecutionJob
```

Rules:

- each active run owns its event queue;
- tool calls run as jobs;
- terminal sessions are long-lived actors;
- project indexing never blocks chat;
- cancellation and reconnect must be first-class;
- multiple runs/tasks may exist concurrently.

The concurrency, process isolation, and cross-platform execution behind this model have
their own design notes: [Runtime & Cross-Platform Execution](runtime/README.md). The key
rules are that tool execution (not agent loops) is what gets isolated in child processes,
physical resources are bounded via leases rather than by capping chats, and Core stays
OS-agnostic behind a `ProcessSandbox` port.

## Thin Clients

UI clients should:

- show data;
- send commands;
- run queries;
- subscribe to events;
- keep only screen-local state.

UI clients should not:

- own agent logic;
- keep full history in memory;
- build LLM context;
- decide tool permissions;
- directly orchestrate terminal/file/git workflows.

Client-local state examples:

```text
selected chat id
open panel
column widths
theme
scroll position
draft text
optimistic item state
```

Core state examples:

```text
chats
messages
runs
tool calls
projects
terminal sessions
permissions
remote sessions
```

## Optimistic UI

The UI must react immediately, then reconcile with Core.

Send message flow:

```text
1. UI immediately shows user message with status "sending".
2. UI sends SendMessage(chatId, text, clientMessageId).
3. Core persists the message.
4. Core emits message.created.
5. UI marks the message as confirmed.
6. Core starts a run and streams events.
7. UI updates only the visible chat.
```

Do not wait for database, LLM, or tool work before showing the user's action.

## Virtualized and Paginated Data

The UI must never load or render all history.

Required query style:

```text
ListChats(limit=50)
GetMessages(chatId, limit=50, before=...)
GetToolOutput(id, offset, limit)
SearchChats(query, limit=100)
```

Target behavior:

```text
10,000 messages in storage
20-40 visible message rows in the DOM
```

Use virtualization for chats, logs, history, project files, terminal output, tool outputs, and timelines.

## Storage

Use SQLite for structured state and files for large blobs.

SQLite:

```text
chats
messages
runs
tool_calls
events
projects
settings
indexes
remote_sessions
permissions
```

Files:

```text
attachments
screenshots
large logs
terminal dumps
tool outputs
snapshots
artifacts
```

SQLite owns relationships and queryable metadata. Files own heavy payloads.

## Remote-first API

Do not design APIs only for desktop. Design APIs for any client.

Minimum API shape:

```text
commands:
  send_message
  cancel_run
  approve_tool_call
  reject_tool_call
  create_chat
  archive_chat

queries:
  list_chats
  get_messages
  get_run
  search
  get_project_files

events:
  subscribe_chat
  subscribe_run
  subscribe_terminal
```

The desktop client and phone client should use the same command/query/event model.

## Capability-based Tools

The agent must not have uncontrolled access to the machine.

Tool capabilities:

```text
read_file
write_file
run_command
edit_project
access_terminal
use_network
delete_file
```

Approval model:

```text
safe action      -> can be automatic
dangerous action -> ask for confirmation
critical action  -> always require confirmation or block
```

Examples:

```text
read package.json   -> auto
edit source file    -> configurable auto/confirm
rm -rf              -> always block/confirm
unknown command     -> confirm
```

This is especially important for remote control from a phone.

## Vertical Slices

Build by vertical slices, not by isolated layers.

Avoid:

```text
month 1: storage only
month 2: API only
month 3: UI only
then nothing works together
```

Prefer:

```text
Slice 1: list chats
Slice 2: open chat
Slice 3: send message
Slice 4: stream answer
Slice 5: save history
Slice 6: tool call
Slice 7: remote read-only
Slice 8: remote send message
```

Each slice should go through:

```text
UI -> API -> Core -> Storage -> Events -> UI
```

## What Not To Do

Do not build:

- one huge global store;
- all history in UI memory;
- agent logic in frontend components;
- Tauri commands called directly from many components;
- provider-specific auth logic in UI, chat, runs, or tools;
- raw provider tokens in UI state, logs, event payloads, or sidecar env vars;
- file-per-chat JSON as primary storage;
- a 5,000-line `AgentService`;
- synchronous operations on the UI thread;
- non-virtualized rendering of long histories.

## Current Repo Mapping

The current codebase is an early scaffold. The target mapping is:

- `crates/mothership-core/`: Agent Core — domain, application services (`ChatRunService`, `ConnectorManager`), repositories, and the host↔sidecar wire protocol (`ipc`). Runs in the sidecar process.
- `src-sidecar/`: the long-running Core host process. Owns the database, the credential vault, and provider-adapter subprocesses; serves the host over stdio.
- `src-tauri/`: thin desktop host — window/webview, the sidecar supervisor (spawn, handshake, request/response correlation, crash-restart), and Tauri commands that forward to the sidecar. Holds no database or adapters.
- `src/`: thin SolidJS desktop client.
- `docs/`: product and architecture source of truth.

When in doubt, move product behavior toward Core and keep clients thin.
