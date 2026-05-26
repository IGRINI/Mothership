# Process Sandbox

The port that spawns tool processes and **reliably kills their whole tree**, with a
per-OS adapter behind it. This is the lowest layer of tool execution; the existing
`Terminal`, `GitClient`, and `run_command` adapters from
[`../ARCHITECTURE.md`](../ARCHITECTURE.md) should be built on top of it rather than
each re-solving spawn + kill-on-cancel.

See [`CROSS_PLATFORM_PATTERN.md`](CROSS_PLATFORM_PATTERN.md) for the ports & adapters
discipline this follows.

## The hard part: killing a process tree

`git`, `npm`, `cargo` spawn grandchildren. Killing the direct child leaves them
running. The guarantee we can offer differs sharply by OS — and it is the reverse of
most people's intuition:

```text
Windows : bulletproof        — Job Object + JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
Linux   : can-be-bulletproof — cgroup v2 (cgroup.kill is atomic); else process group
macOS   : best-effort only   — process group/session; setsid() escapes, no cgroups
```

Consequence: the supervisor must **not assume** a clean tree-kill. It needs a reaper
that tracks PIDs and finishes off orphans — primarily for macOS.

> `tokio`'s `Child::kill_on_drop(true)` only kills the *direct* child, not the tree.
> Do not rely on it for tool isolation.

## Base crate

Use **`process-wrap`** (the successor to `command-group`, same maintainer). Its Windows
`JobObject` wrapper always sets `CREATE_SUSPENDED` internally: the process is created
suspended, assigned to the job, *then* resumed — which closes the race where a
grandchild spawns before the assign. Verified against its docs:

- https://docs.rs/process-wrap/latest/process_wrap/
- https://github.com/watchexec/process-wrap

This means we do **not** hand-roll `CREATE_SUSPENDED` on Windows; the crate does it
race-free. Raw Win32 stays as a last resort behind the Windows adapter only.

## Adapters

```text
WindowsSandbox  -> process-wrap JobObject; TerminateJobObject / kill-on-job-close
LinuxSandbox    -> process group/session; cgroup v2 escalation when needed
MacSandbox      -> process group/session + PID reaper from day one
```

### macOS reaper

```text
on spawn:   remember root pid; start reaper
running:    track descendants; watch exits/forks via kqueue EVFILT_PROC
            (NOTE_EXIT / NOTE_FORK) where possible
on kill:    kill the process group/session, then known descendant PIDs,
            rescan a few times, force-kill leftovers
```

Two honest limits:

- **Best-effort, not a guarantee.** A tool that deliberately `setsid()`s and detaches
  fast can be lost without a stronger sandbox.
- **PID-reuse hazard.** Between observing a PID and killing it, the process may exit and
  the PID be reused by something unrelated. Prefer killing the **group/session**;
  use bare-PID kill only as the macOS fallback, knowingly best-effort.

## Graceful shutdown is an IPC message, not a signal

Windows has no `SIGTERM`, so "send SIGTERM, wait, then SIGKILL" does not port. Model
graceful shutdown as an app-level protocol message → timeout → hard kill
(`TerminateJobObject` / process-group kill).

For **tool** processes this rarely matters — `git`/`cargo`/`rg` are fine to hard-kill
on timeout/cancel. Graceful shutdown matters for long-lived **agent-worker / terminal**
processes, and there it belongs in the IPC layer, not in `ProcessSandbox`. Keep
`ProcessSandbox` focused on spawn + hard `kill_tree`.

## The port

Returns boxed `AsyncRead` rather than concrete `tokio::process` handles, so the trait
stays a real platform boundary (a future cgroup-exec or remote-exec adapter can satisfy
it too).

```rust
use tokio::io::AsyncRead;

#[async_trait::async_trait]
pub trait ProcessSandbox: Send + Sync {
    async fn spawn(&self, spec: ToolSpec) -> anyhow::Result<Box<dyn SpawnedProcess>>;
}

#[async_trait::async_trait]
pub trait SpawnedProcess: Send {
    fn pid(&self) -> u32;
    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>>;
    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>>;
    async fn wait(&mut self) -> anyhow::Result<ToolExit>;
    async fn kill_tree(&mut self) -> anyhow::Result<()>;
}
```

`#[async_trait]` is still needed for `dyn` with async methods.

## Cargo: target-gated dependencies

Keep `windows-sys` from compiling on Linux, etc.:

```toml
[dependencies]
process-wrap = { version = "*", features = ["tokio1"] } # confirm exact feature name

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "*", features = ["Win32_System_JobObjects"] }

[target.'cfg(target_os = "linux")'.dependencies]
# cgroups-rs or a thin cgroup v2 wrapper

[target.'cfg(target_os = "macos")'.dependencies]
# libproc / kqueue for the reaper
```
