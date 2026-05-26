# Execution Model

How agent work is placed, isolated, and resource-managed inside Core. Extends the
*Actor and Job Model* section of [`../ARCHITECTURE.md`](../ARCHITECTURE.md).

See [`README.md`](README.md) for the two theses this all follows from
(control-plane vs data-plane; logical ≠ physical concurrency).

## Topology

```text
Desktop / Phone / Web client
  ↓  commands / queries / events
Core runtime (thin supervisor)
  ├─ RunActors                 agent loops, as tokio tasks
  ├─ RunActorWatchdog          detects blocked / runaway tasks
  ├─ ToolSupervisor            owns tool execution
  │    └─ ProcessSandbox       per-OS process isolation (see PROCESS_SANDBOX.md)
  ├─ ResourceBroker            local buckets + sharding (NOT a central queue)
  ├─ CpuWorkerPool             indexing / diff / parsing / embeddings
  ├─ EventBatcher              coalesce + backpressure
  └─ EventStore                append-only log (see ARCHITECTURE.md)

ToolExecutionJob → ToolSupervisor → ProcessSandbox → child process (git/cargo/shell/…)
```

The supervisor is **control-plane only**. It routes, schedules, persists compact
events, monitors health, and hands out resource leases. It must never run an agent
loop's heavy work, execute a tool, scan a workspace, compute a large diff, or block on
git/fs/network. The moment Core does heavy work, it stops being a supervisor and
becomes the bottleneck.

## Agent-loop placement

A `RunActor` runs **in-process as a tokio task** by default. An agent loop is cheap:
it awaits an LLM stream, awaits a tool result, updates a state machine, routes events.
There is no reason to pay for a separate process for that.

Runtime choice:

```text
shared Core runtime          -> multi_thread, small fixed worker count
isolated AgentWorker process -> current_thread   (only if a run is escalated, below)
```

Why the split: on the **shared** Core, `current_thread` would let one blocked task
starve all other runs — it must be `multi_thread`. `current_thread` only makes sense
once a run lives alone in its own process.

## Tool execution: always isolated

Anything in this list runs in an isolated child process via `ProcessSandbox`, never
inline in a `RunActor`:

```text
shell   git   cargo   npm   python   ripgrep
build   tests   formatter   linter
workspace scan   indexing   large diff   embedding batch
```

Inline execution inside an actor is forbidden:

```rust
// FORBIDDEN inside a RunActor — blocks the runtime, no isolation, no kill-on-cancel
let out = std::process::Command::new("cargo").arg("test").output()?;
```

```rust
// Correct: the actor only awaits a result; isolation/kill/limits live in the sandbox
let result = tool_supervisor.run(ToolRequest {
    program: "cargo".into(),
    args: vec!["test".into()],
    cwd, timeout, output: OutputPolicy::default(),
}).await?;
```

## Escalation policy

Placement is decided by **resource/risk class**, not by "is this chat important".

```rust
enum Placement {
    InActorTask,        // pure LLM call / light subagent, no tools, short-lived
    DedicatedProcess,   // a whole RunActor escalated to its own process
    ToolRunner,         // shell/git/fs/workspace mutation
    CpuPool,            // indexing / large diff / embeddings
}
```

- A subagent that only thinks / makes an LLM call stays an `InActorTask`.
- A subagent that runs tools, mutates a workspace, can hang, or floods stdout gets a
  `ToolRunner` (its tool calls do) — it does **not** get a whole process by default.
- A `RunActor` is escalated to `DedicatedProcess` only on evidence: it pegs CPU, holds
  too much memory, hosts an untrusted plugin/MCP, or repeatedly destabilizes Core.

500 subagents may exist logically. The scheduler decides which are actually in an
expensive phase right now. Cheap phases (awaiting model, planning) cost nothing.

## Resource model

Bound the resource classes, not the chats. Plausible kinds:

```rust
enum ResourceKind {
    ModelRequest,
    ShellProcess,
    GitOperation { repo: RepoId },
    WorkspaceScan { repo: RepoId },
    CpuHeavy,
    DiskHeavy,
    DbWrite,
}
```

Reasonable defaults (tune later):

```text
max_cpu_heavy_jobs        = cores - 1
max_git_ops_per_repo      = 1          // serialize per repo, not globally
max_workspace_scans/repo  = 1
max_db_writers            = 1          // sqlite single-writer; batch writes
max_shell_processes       = configurable
```

**The broker is not a central server.** Every git/shell/cpu call routing through one
central lease holder would itself become an `O(agents)` hot-path chokepoint. Instead:

```text
global policy        rarely updates the limits
local token buckets  make the fast accept/queue decision in-process
shards by class      git_bucket_by_repo, cpu_bucket, disk_bucket, model_bucket
```

A run that can't get a lease does not block a thread — it parks in
`WaitingForResource` and is resumed when capacity frees up.

## Watchdog (Stage 1, not later)

Because agent loops live in the shared Core, they share fate: one task that blocks the
runtime or busy-loops degrades all 100 runs with no isolation. The watchdog is the
**price of the in-core default**, so it ships in Stage 1, not as a future nicety.

```text
- per-task instrumentation (tokio-metrics::TaskMonitor; tokio-console in dev)
- runtime-lag / slow-poll detection
- enforce: never block the loop — heavy work goes to spawn_blocking or a child process
- tracing spans keyed by run / agent id
```

## Build order

```text
Stage 1  (max ROI)
  RunActors as tokio tasks on a multi_thread Core
  ToolSupervisor + ProcessSandbox port
  parallel stdout/stderr drain + timeout + cancel + bounded output (spill, don't kill)
  RunActorWatchdog
  event batching + bounded queues

Stage 2
  local resource buckets; per-repo git serialization
  CpuWorkerPool; disk-heavy queue
  blob refs for large output; stuck-tool watchdog

Stage 3
  agent-level watchdog escalation: detect blocking/huge-memory runs
  optional DedicatedProcess for selected runs

Stage 4
  real checkpoint/restart; stronger sandboxing for untrusted tools
```
