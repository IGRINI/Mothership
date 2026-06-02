# Runtime Migration Plan

Step-by-step path from today's code to the agent runtime described in the
[`runtime/`](README.md) docs — **without breaking the working chat**.

- **Created:** 2026-05-26
- **Status:** Phase 1 done (`22a3b6d`) · Phase 3 done (`b2e9615`; T3.5 Unix/macOS open) · **Sidecar split done (2026-05-27)** — Core now runs out-of-process ahead of Phase 5, see note below · next: Phase 2 (tokio + RunActor) or T3.5
- **How to use:** tick `- [ ]` → `- [x]` as tasks land. Update **Status** above with the
  current phase. Each phase should leave the streaming chat working — that is the
  regression gate.

## Context (where we are)

A working `chat + LLM + auth` vertical slice exists, but:

- a "run" is a `std::thread::spawn`, not an actor (no cancel / mailbox / reconnect);
- Core is synchronous (no tokio);
- there is no tool execution, no `ProcessSandbox`, no resource control.

Resolved since this plan was written:
- run orchestration moved into Core (`ChatRunService`, `ConnectorManager`) — "move behavior to Core";
- **the sidecar is now a long-running daemon, not a one-shot CLI.** Core runs *in it*; the desktop host is a thin client over a newline-JSON protocol (`mothership-core::ipc`). See the note at the end of Phase 5 for why this was pulled forward.

Guiding order: **relocate → change execution model → add data-plane → wire tools →
scale limits.** Each step keeps chat green.

## Phase 1 — Move run orchestration into Core (synchronous, no tokio)

Implements the "Core owns behavior" rule. No new dependencies. Fixes divergence #1.

- [x] **T1.1 — `ChatRunService` in Core.** Done — `crates/mothership-core/src/run.rs`.
  Took `&SendChatMessageResult` instead of `ChatRunContext` (the host already has it
  post-`begin_chat_run`, and the `Started` event needs the full chat/message objects).
  Behavior byte-identical.
- [x] **T1.2 — Thin the host.** Done — `TauriChatRunSink` reduced to `{ app }` +
  `emit("chat-run-event")`; `send_chat_message` keeps `std::thread::spawn` and calls
  `ChatRunService::new(&db).run(...)`. Old `run_chat_completion`/`complete_chat_run` removed.
- [x] **T1.3 — Core unit test.** Done — 3 offline error-path tests (no model / no active
  connection / empty context) with a capturing sink asserting `Started → Failed`.
- [ ] **Regression gate — partial.** `cargo test -p mothership-core` 30/0 ✓,
  `cargo check -p mothership-app` ✓. **UI smoke test still pending** (live streaming not
  auto-verifiable — run the app and send a message before calling this fully done).

**Review notes (commit `22a3b6d`, reviewed against `git diff`):**
- DB↔emit untangling is clean: DB writes live in a private `DbForwardingSink` (Core),
  host sink is emit-only. Scope was exactly the chat-run path; `core/Cargo.toml` untouched.
- _Follow-up (low):_ `cargo tree -p mothership-core` still shows `tokio` — transitive via
  `reqwest` default features (pre-existing, not from this change). To make Core truly
  tokio-free: `reqwest { default-features = false }` + a blocking feature set. Platform GUI
  crates (tauri/wry/winit/gtk/objc) are absent, so the leak-test holds for *platform* crates.
- _Follow-up (low):_ `auth_store_path` helper now duplicated in `run.rs` / `commands.rs` /
  `src-sidecar`. Consider centralizing in Core.
- _Deferred:_ happy-path unit test needs a `&dyn LlmChatCompletionGateway` seam to fake the
  gateway — do it when Phase 2/4 touches the gateway anyway.

## Phase 2 — tokio + `RunActor` + cancellation

Execution-model change. Do **after** Phase 1, not at the same time.

- [ ] **T2.1 — Introduce tokio in Core** (`multi_thread`). Do **not** rewrite the sync
  LLM gateway — bridge its call with `spawn_blocking`.
- [ ] **T2.2 — `RunManager` + `RunActor`** as a tokio task with a cancel token,
  replacing `std::thread::spawn`. State machine: Started → Delta → Completed/Failed.
- [ ] **T2.3 — `cancel_run` command** wired to cancel the actor (the "stop" button that
  doesn't exist today).
- [ ] **Regression gate:** chat still streams; cancel actually stops a run.

## Phase 3 — `ProcessSandbox` (parallel track — can start now)

Net-new, touches nothing existing. Implements [`PROCESS_SANDBOX.md`](PROCESS_SANDBOX.md).

- [x] **T3.1 — port + crate.** Done — crate `crates/process-sandbox` (trait + adapter live
  together for now; can split into a `ports` crate later). `ProcessSandbox`/`SpawnedProcess`
  + `ToolSpec`/`ToolExit` + `MockSandbox`; `platform_sandbox()` is the single `#[cfg]` root.
- [x] **T3.2 — Windows adapter.** Done — `JobObjectSandbox` on `process-wrap` 9.1 (`tokio1`).
  Tree-kill via `TerminateJobObject`; relies on process-wrap's internal `CREATE_SUSPENDED`
  (race-free job assignment) — no hand-rolled Win32.
- [ ] **T3.3 — drain + `OutputPolicy` — partial.** Parallel two-task drain done (invariant
  enforced) + `BoundedCapture` (head+tail) honoring `memory_preview_bytes`. The rest of
  `OutputPolicy` (`ui_stream_bytes_per_sec`, `agent_tail_bytes`, `spill_to_file`) are present
  but no-op `TODO`s — finish in a follow-up.
- [x] **T3.4 — `kill_tree` + test.** Done — integration test spawns `cmd /c ping`, finds the
  `ping.exe` **grandchild** by ParentProcessId (CIM), kills the tree, asserts that exact PID is
  gone. 9/9 tests pass on Windows.
- [ ] **T3.5 — Unix adapter + macOS reaper.** Open. Currently a loud `unix.rs` stub
  (`UnimplementedSandbox`) so the crate compiles on Linux/macOS; real impls + CI matrix next.

**Review notes (commit `b2e9615`, verified `cargo test -p process-sandbox` 9/9 + leak test):**
- Leak test green: `cargo tree -p mothership-core` has no `process-sandbox` / `process-wrap` / `async-trait`.
- _Follow-up (verify):_ adapter sets `kill_on_drop(false)` and documents "no kill on drop", but
  process-wrap's Job Object may use `KILL_ON_JOB_CLOSE` — confirm whether dropping a
  `SpawnedProcess` without `kill_tree()` leaks the tree or kills it, then document the drop policy.
- _Follow-up (minor):_ `SpawnedProcess::wait()` is documented idempotent-after-exit, but the
  Windows impl calls the underlying `wait()` directly — harden the impl or relax the contract.

## Phase 4 — `ToolSupervisor` + first tool

- [ ] **T4.1 — `ToolSupervisor`** in Core over `Arc<dyn ProcessSandbox>`, returning
  `exit / head / tail / log_ref`.
- [ ] **T4.2 — First tool `run_command`** behind the approve/confirm capability model
  from [`../ARCHITECTURE.md`](../ARCHITECTURE.md).
- [ ] **T4.3 — Tool-calling in `RunActor`** — depends on gateway function-calling
  support; likely its own phase.

## Phase 5 — Resource limits & event backpressure (deferred, Stage 2+)

Only needed once concurrent runs/tools exist. Premature before that.

- [ ] **T5.1 — Local token buckets** + per-repo git serialization.
- [ ] **T5.2 — Event batching / coalescing** (today: one DB write + emit per delta — the
  un-batched hot path; batch when it bites).

### Sidecar split — pulled forward (done 2026-05-27)

This plan originally deferred the long-running sidecar to "after the actor model."
That sequencing was reversed deliberately, and the split was done first:

- **Why now:** coupling accretes. Every module built against the in-host assumption
  makes the boundary more expensive to extract later. The surface was minimal at this
  point (13 ops), and the transport mechanism already existed (the `mothership-adapter-host`
  NDJSON-over-stdio pattern). Doing it while small was cheaper than doing it after piling
  modules on top — and a boundary you don't *run* as a process isn't enforced, so the
  discipline rots. (A `veche` committee review concurred: extraction now, not a runtime
  rewrite; keep actor/agent semantics off the wire as additive enum variants.)
- **What landed:** Core (DB, vault, adapters, chat runs) runs in `src-sidecar` as a daemon;
  the host is a thin client (`src-tauri/src/sidecar.rs` supervisor + `commands.rs` pass-through).
  Protocol in `mothership-core::ipc`: Hello/Initialize/Ready handshake, correlated
  request/response, operation-centric streamed events, typed errors, version handshake.
  Windows Job Object (`src-tauri/src/job.rs`, `KILL_ON_JOB_CLOSE`) ties the sidecar +
  its adapter subtree to the host's lifetime. Verified by `src-sidecar/tests/protocol_handshake.rs`.
- **What this does NOT include (still future, additively):** the tokio `RunActor` model
  (Phase 2) and `ProcessSandbox`-backed tool execution (Phase 4) move *inside* the sidecar
  behind the same protocol — `CoreRequest`/`CoreEvent` gain `Run`/`ToolCall` variants without
  a redesign. Per-adapter sub-jobs (vs. the current host-level job) are deferred; a sidecar
  crash can briefly orphan an in-flight adapter until host exit closes the job.

## Guardrail (cheap, any time)

- [ ] **CI matrix** `windows-latest / ubuntu-latest / macos-latest` → `cargo build && cargo test`,
  so `#[cfg]` code never silently rots.
- [ ] **Leak test in CI:** assert `cargo tree -p mothership-core` has no platform crates.

## Start here

1. **T1.1** — extract `ChatRunService` into Core (critical path; smallest real fix;
   chat is its own test; zero new deps).
2. **In parallel, optional:** **T3.1 + T3.2** — `ProcessSandbox` + Windows adapter
   (independent, locally verifiable).

Do **not** start with Phase 2 (bigger; follows the relocation) or Phase 5 (premature).
