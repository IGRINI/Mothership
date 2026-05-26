# Output & IPC

How tool output is drained and bounded, and how Core talks to processes and clients
without drowning. Pairs with the event log in
[`../ARCHITECTURE.md`](../ARCHITECTURE.md).

## stdout/stderr: drain both, in parallel, always

A child process blocks when the pipe you are not reading fills up. This is the single
most common cause of "the tool hung" that is actually "we stopped reading it."

**Invariant:** drain stdout and stderr in **separate concurrent tasks**. Never read one
to EOF and then the other — the child can block writing stderr while you wait on stdout.

```rust
let stdout_task = tokio::spawn(drain(stdout, StreamKind::Stdout));
let stderr_task = tokio::spawn(drain(stderr, StreamKind::Stderr));

tokio::select! {
    exit = proc.wait()       => { /* finished */ }
    _    = cancel.cancelled() => { proc.kill_tree().await?; }
}
```

```rust
// WRONG — reintroduces the pipe-full deadlock
read_to_end(stdout).await;
read_to_end(stderr).await;
```

## Output policy: bound memory, never kill for size

Volume of output is **not** a hang. A large `cargo build` is legitimate. Bound what you
hold in memory and what you stream; spill the rest to a file; let the process run.

```rust
struct OutputPolicy {
    memory_preview_bytes: usize,   // e.g. 256 KiB held in RAM
    ui_stream_bytes_per_sec: usize,// e.g. 128 KiB/s, throttled
    agent_tail_bytes: usize,       // e.g. 64 KiB given back to the model
    spill_to_file: bool,           // full log to disk, unbounded
}
```

Kill a tool **only** on:

```text
timeout   |   hang / no-progress timeout   |   explicit cancel   |   sandbox violation
```

Never on:

```text
10 MB of stdout   |   100 MB of stderr   |   lots of warnings
```

## What the agent gets back

Not the whole log — the model rarely needs it and it wastes context:

```text
exit_code
first N lines + last N lines   (head/tail)
parsed diagnostics, if any
full_log_ref                   (path/blob id for the complete output)
truncated_for_context: bool
```

The UI gets a sampled/throttled preview plus the `full_log_ref` for on-demand paging
(consistent with the virtualized/paginated rule in `ARCHITECTURE.md`).

## IPC transport

Start simplest and identical on all three OSes:

```text
v1   : stdin/stdout NDJSON           — zero platform divergence, easy to debug/kill
later: interprocess crate            — named pipes (Windows) / unix sockets (Unix)
```

Do **not** use `tokio::net::UnixListener` — it is `#[cfg(unix)]` and absent on Windows.
Do not hand-branch named-pipe vs unix-socket code; if you outgrow stdio, the
`interprocess` crate gives one local-socket API across all three.

## Large payloads go by reference

Never push megabytes through IPC as inline events.

```json
{ "type": "tool_output_chunk", "tool": "a1",
  "blob": "logs/chat-a/tool-42.log", "offset": 1048576, "len": 65536 }
```

not

```json
{ "type": "stdout", "text": "…20 MB of log…" }
```

## Event backpressure

100 agents × subagents × token streams × tool logs × progress events will bury an
unbounded event path. Required policies:

```text
bounded mailboxes / queues everywhere
token deltas       -> batch (e.g. every ~50 ms)
progress events    -> coalesce by run/agent id
tool stdout        -> chunk + backpressure (above)
debug/trace        -> ring buffer + sampling, droppable
large payloads     -> blob refs, not inline
```

Core should be boring on the hot path: receive a small event, merge/persist it, forward
it, forget it. No `O(number_of_agents)` work per tick; the scheduler works
incrementally, it does not rescan all pending jobs every tick.
