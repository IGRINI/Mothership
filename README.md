# Mothership

Mothership is a desktop control plane for a local AI coding agent.

The user's computer is the main station where the agent runs, reads files, edits projects, starts long-running tasks, and keeps the full work history. The desktop app is the primary workspace, while a future mobile client will act as a remote control for the same chats, tasks, and agent state.

The product target is simple:

> My agent runs on my PC, and I can control it from anywhere.

## Product Direction

Mothership is not meant to feel like a generic chat window or a full IDE replacement. It is an AI workspace for developers:

- chat with a local AI agent;
- continue old project discussions;
- inspect task progress and history;
- run long work sessions without blocking the UI;
- control the same local agent from a phone while the computer keeps doing the work.

The most important product quality is responsiveness. Large histories, long answers, many chats, and background tasks must not make the interface feel heavy. Prefer progressive loading, virtualization, skeletons, and clear task states over blocking the app while everything loads.

The full product description lives in [docs/PROJECT_DESCRIPTION.md](docs/PROJECT_DESCRIPTION.md).

## Architecture

Mothership is designed as:

```text
Modular Monolith + Hexagonal Architecture + CQRS-lite + Event-driven runtime + Thin UI clients
```

The desktop app is not the product boundary. The product boundary is Agent Core:

```text
Agent Core = product logic and runtime
Desktop UI = local thin client
Phone UI = future remote thin client
Web UI = optional thin client
```

Even when Core runs inside the Tauri process in v1, code should be shaped as if Core can move into a local sidecar/server. UI clients should send commands, run queries, subscribe to events, and keep only screen-local state.

The architecture source of truth lives in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Current Stack

- **Desktop shell:** Tauri 2
- **Frontend:** SolidJS, TypeScript, Vite
- **Native app layer:** Rust
- **Local data:** SQLite through `rusqlite`
- **Sidecar:** Rust binary launched by the Tauri shell
- **Large lists:** virtualized rendering through TanStack Virtual
- **Research sources:** cloned into `projects-to-research/` and ignored by git

## Repository Layout

```text
.
├── crates/mothership-core/     # Shared Rust domain and SQLite logic
├── docs/                       # Product and technical documentation
├── projects-to-research/       # Ignored local research repositories
├── scripts/                    # Build helpers, including sidecar copy script
├── src/                        # SolidJS frontend
├── src-sidecar/                # Rust sidecar binary
├── src-tauri/                  # Tauri 2 app shell, commands, config, resources
├── Cargo.toml                  # Rust workspace
└── package.json                # Frontend and Tauri scripts
```

## Development

Install JavaScript dependencies:

```powershell
npm install
```

Run the desktop app in development mode:

```powershell
npm run tauri:dev
```

This command builds the Rust sidecar, starts Vite, builds the Tauri shell, and launches the app.

Build frontend assets only:

```powershell
npm run build
```

Check the Rust workspace:

```powershell
cargo check --workspace
```

Build distributable desktop bundles:

```powershell
npm run tauri:build
```

The Windows bundles are written under:

```text
target/release/bundle/
```

## Sidecar Notes

The sidecar package is `mothership-sidecar`. During dev/build, `scripts/build-sidecar.mjs` compiles it and copies target-specific binaries into `src-tauri/binaries/` for Tauri bundling.

At runtime the Tauri shell launches the sidecar by logical name:

```rust
mothership-sidecar
```

Do not call it through `binaries/mothership-sidecar` from Rust app code. Tauri resolves sidecars relative to the running app executable.

## UX Principles

- Keep the app fast, calm, and predictable.
- Do not block typing, navigation, or chat switching during long operations.
- Virtualize large chat histories, logs, and lists.
- Load visible data first; fetch or compute deeper history progressively.
- Use dark mode as the primary visual direction.
- Prefer practical developer workflows over decorative landing-page UI.
- Keep the interface focused on chats, task state, history, and agent output.

## Research Repositories

External repositories used for future research are stored in:

```text
projects-to-research/
```

That directory is intentionally ignored by git. Treat it as local reference material, not part of the product source tree.
