# AGENTS.md

This file defines how coding agents should work in this repository.

## Product Context

Mothership is a Tauri 2 + SolidJS desktop application for controlling a local AI coding agent. The user's PC is the main station where the agent runs; the future mobile client is a remote control for the same chats, tasks, and history.

Read [docs/PROJECT_DESCRIPTION.md](docs/PROJECT_DESCRIPTION.md) before making product or UX decisions.
Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) before making architecture, Core, runtime, storage, API, or UI-state decisions.

Primary product requirement: the app must stay responsive with large histories, long answers, many chats, and background tasks.

## Architecture Doctrine

The target architecture is:

```text
Modular Monolith + Hexagonal Architecture + CQRS-lite + Event-driven runtime + Thin UI clients
```

Core rule:

```text
Agent Core is the product.
Desktop UI and Phone UI are clients.
```

Do not treat this as a "Tauri app with some logic inside." Treat it as Agent Core with a Tauri desktop adapter.

Architectural requirements:

- Keep application behavior in Core modules, not frontend components.
- Design APIs as remote-first commands, queries, and event streams.
- Keep UI clients thin: render state, send commands, run queries, subscribe to events.
- Keep Tauri commands as adapters; they should route into Core rather than contain business logic.
- Use ports/adapters for LLM providers, storage, file system, terminal, git, remote transport, and notifications.
- Prefer vertical slices that cross UI -> API -> Core -> Storage -> Events -> UI.
- Preserve event-driven runtime semantics for runs, tools, terminal sessions, indexing, and remote sessions.
- Use actor/job style execution for long-running or cancellable work.
- Store structured state in SQLite and large payloads as files/blobs.
- Use capability-based permissions for tool access.

## Hard Rules

- Do not use `rg`; it is considered unavailable in this workspace. Use PowerShell `Get-ChildItem`, `Select-String`, or another available tool.
- Write production-ready code. Keep changes simple, maintainable, and testable.
- Follow KISS, DRY, and SOLID where they apply.
- Preserve clear separation of responsibility and correct Rust/TypeScript module boundaries.
- Do not use the official Figma connector or `mcp__codex_apps__figma`. If Figma work is needed, use only Figma UI MCP Bridge (`mcp__figma_ui_mcp__`) or explicit user-provided design data.
- Do not commit or modify `projects-to-research/`; it is ignored local research material.
- Do not revert user changes unless the user explicitly asks for it.

## Architecture Boundaries

- `src/` contains SolidJS frontend code.
- `src/shared/` contains frontend shared API/types/helpers.
- `src/features/` contains frontend feature slices.
- `src-tauri/` contains the Tauri shell, app commands, capabilities, config, and platform resources.
- `src-tauri/src/commands.rs` exposes Tauri commands.
- `src-tauri/src/sidecar.rs` owns sidecar invocation from the Tauri app.
- `src-sidecar/` contains the standalone Rust sidecar binary.
- `crates/mothership-core/` contains shared Rust domain, storage, models, and SQLite logic.
- `scripts/` contains development/build automation.

Keep business/domain logic out of UI components when it belongs in shared modules or Rust core. Keep Tauri command handlers thin: validate/route, then delegate.

Future Core module boundaries should follow:

- `chat`: chats, messages, history.
- `runs`: agent task execution and run state.
- `tools`: tool calls, approvals, execution records.
- `projects`: project registry, file metadata, indexing.
- `terminal`: terminal sessions and command execution.
- `remote`: phone/web sessions and event fanout.
- `auth`: local and remote access control.
- `storage`: repositories, migrations, file/blob storage.
- `llm`: provider adapters and streaming.

## Sidecar Contract

The sidecar binary package is `mothership-sidecar`.

Build scripts copy compiled sidecars into `src-tauri/binaries/` for Tauri bundling. Runtime Rust code should invoke the sidecar by logical name:

```rust
app.shell().sidecar("mothership-sidecar")
```

Do not use `binaries/mothership-sidecar` in runtime Rust code. Tauri resolves sidecar paths relative to the running executable.

## UI Expectations

- Build the actual workspace experience, not a marketing landing page.
- Keep the interface calm, dense enough for developer work, and suitable for long sessions.
- Dark theme is the primary direction.
- Use virtualization for large chats, logs, histories, project lists, and event streams.
- Use paginated queries for histories, messages, logs, tool output, and search.
- Use optimistic UI for actions like sending messages, renaming chats, approvals, and cancellation.
- Long operations must not block typing, navigation, or switching views.
- Prefer progressive loading and visible status over freezing the UI.
- Avoid decorative UI that does not help the developer understand chats, tasks, agent progress, or history.

## Verification

Use the narrowest verification that proves the change, then broaden when touching shared behavior.

Useful commands:

```powershell
npm run build
cargo check --workspace
npm run tauri:dev
npm run tauri:build
```

For sidecar or Tauri runtime changes, `cargo check` is not enough. Run `npm run tauri:dev` and exercise the affected UI path. For release/bundling changes, run `npm run tauri:build`.

## Current Local Notes

- Research repositories are stored under `projects-to-research/`.
- `projects-to-research/`, `node_modules/`, `dist/`, `target/`, and generated sidecar binaries are ignored.
- Windows GNU builds use project-local build script handling for resource compilation and app manifest behavior. Do not remove that without re-testing `npm run tauri:dev` and `npm run tauri:build`.
