# Runtime & Cross-Platform Execution

Design notes for Mothership's agent runtime: how many agents run concurrently, how
their tool execution is isolated, and how the whole thing stays correct on Linux,
macOS, and Windows.

This is a **deep-dive that extends** [`../ARCHITECTURE.md`](../ARCHITECTURE.md) —
specifically its *Actor and Job Model* and *Hexagonal Core* sections. It does not
replace anything there; it fills in the parts that doc only sketches.

> **Status: design under evaluation, not yet implemented.** Verify against the actual
> `crates/` before treating any of this as as-built. These notes are the sum of a
> design exploration plus corrections made while stress-testing it.

## The two theses (the whole spine)

Everything below follows from two ideas. If you remember nothing else, remember these.

**1. Control-plane vs data-plane.**

```text
agent loop      = cheap control-plane   (mostly awaiting LLM / IPC / tool results)
tool execution  = dangerous data-plane  (shell/git/cargo, CPU-heavy, unbounded stdout)
```

Isolate the **data-plane**, not the control-plane. The thing that hangs, crashes,
leaks, or floods you is almost always a tool process — not the agent's await loop.
Spending your first isolation budget on process-per-agent isolates the cheap, safe
part and pays the most for the least.

**2. Logical concurrency ≠ physical concurrency.**

```text
logical (fine):     100 agents alive, 500 subagents, 1000 pending tool calls
physical (bounded): CPU in use, disk in use, concurrent git ops, db writes,
                    live tool processes, event throughput
```

You do **not** cap the number of chats. You bound the *physical* resources via
leases/budgets, and let agents queue on them. "100 agents" then means 100 isolated
execution trees competing only for explicitly managed resources — not an unbounded
fork bomb.

## Documents

- [`EXECUTION_MODEL.md`](EXECUTION_MODEL.md) — runtime topology, agent-loop placement,
  tool isolation, the resource broker, the staged build order.
- [`PROCESS_SANDBOX.md`](PROCESS_SANDBOX.md) — the cross-platform process-isolation
  port and its per-OS adapters; reliable process-tree kill.
- [`OUTPUT_AND_IPC.md`](OUTPUT_AND_IPC.md) — stdout/stderr handling, output policy,
  IPC transport, event backpressure.
- [`CROSS_PLATFORM_PATTERN.md`](CROSS_PLATFORM_PATTERN.md) — how to keep Core
  platform-agnostic (ports & adapters, dependency direction, the leak test).
- [`MIGRATION_PLAN.md`](MIGRATION_PLAN.md) — **live checklist**: the phased path from
  today's code to this runtime without breaking chat. Tick boxes as work lands.

## Mapping to the existing actor model

These notes use the vocabulary already in `ARCHITECTURE.md`:

```text
RunActor          = an agent loop, runs as a tokio task inside Core
ToolExecutionJob  = a single tool run, executed in an isolated child process
TerminalActor     = long-lived terminal session (also a sandboxed process)
```

New concept introduced here: **`ProcessSandbox`** — a low-level port that spawns and
reliably kills process trees per-OS. The existing `Terminal`, `GitClient`, and the
`run_command` tool adapter are all expected to sit *on top of* `ProcessSandbox` rather
than each re-solving process spawning and kill-on-cancel.

## Decision log (research + corrections)

The non-obvious decisions, and why we landed where we did. The "corrections" are the
alternatives we considered and rejected — recorded here so they don't get re-litigated.

| Considered | Decision | Why |
| --- | --- | --- |
| `max_active_chats = 3` (flat cap) | **Rejected** | Wrong model for an app meant to run many agents; bounds the wrong thing. Bound physical resources via leases instead. |
| Process-per-agent-tree as the default | **Rejected as default; kept as escalation** | The danger is tools, not agent loops. Process spawn is expensive (esp. Windows), and 100 processes is heavy. Escalate to a dedicated process only when metrics prove an agent loop threatens Core. |
| Central `ResourceBroker` on the hot path | **Rejected** | A central lease per git/shell/cpu call becomes an `O(agents)` chokepoint. Use local token buckets + sharding by resource class. |
| `current_thread` runtime per worker (early take) | **Refined** | Correct for an *isolated* per-agent process. Wrong for the *shared* Core, where one bad task would starve all 100 — the shared Core must be `multi_thread`. |
| Kill a tool when output exceeds N bytes | **Rejected** | Verbosity ≠ hang. A large `cargo build` is legitimate. Bound only what's held in memory / streamed; spill the rest to a file. Kill only on timeout / hang / cancel. |
| `command-group` as the spawn/kill base | **Replaced with `process-wrap`** | `command-group` is superseded; `process-wrap` is its successor and its Windows `JobObject` wrapper sets `CREATE_SUSPENDED` internally, closing the assign-before-grandchild race. |
| Graceful shutdown via Unix signals | **Rejected for portability** | Windows has no `SIGTERM`. Make graceful shutdown an app-level IPC message with OS-kill as the hard fallback. |
| Own ports for file-watch / config paths / secrets-everywhere | **Use mature crates** | `notify`, `directories`, `keyring` are already cross-platform. Only hand-roll a port where no good crate exists or you need a test seam. |

## Open questions

- Real checkpoint/restart of a `RunActor` is hard (history + tool-call state + partial
  output). For the MVP: a dead worker → `run.failed` + retry. Proper checkpointing is a
  separate effort, not Stage 1.
- Linux `cgroup v2` is "later/optional" for normal dev CLIs, but moves into Stage 1 the
  moment untrusted / MCP / daemonizing tools are in scope (see `PROCESS_SANDBOX.md`).
