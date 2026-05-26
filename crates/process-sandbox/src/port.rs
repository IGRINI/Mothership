//! The neutral contract: [`ProcessSandbox`] / [`SpawnedProcess`] plus the DTOs
//! that cross the boundary ([`ToolSpec`], [`ToolExit`]).
//!
//! This speaks Core's vocabulary — "spawn a tool, kill its whole tree, stream its
//! output" — never the platform's ("create job object", "killpg", "kqueue"). Per
//! [`CROSS_PLATFORM_PATTERN.md`], the trait is the *port*; each per-OS impl is an
//! *adapter* selected at compile time via `#[cfg]` (see [`crate::platform_sandbox`]).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::AsyncRead;

/// What to run. A neutral description of a child process to spawn — no platform
/// detail leaks in here.
#[derive(Debug, Clone, Default)]
pub struct ToolSpec {
    /// The program/executable to run (e.g. `git`, `cargo`, `cmd`).
    pub program: OsString,
    /// Arguments passed to the program, in order.
    pub args: Vec<OsString>,
    /// Working directory for the child. `None` inherits the parent's cwd.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables to set on the child, merged over the inherited
    /// environment. A `BTreeMap` keeps iteration order deterministic (nice for tests).
    pub env: BTreeMap<OsString, OsString>,
    /// Optional wall-clock timeout. `ProcessSandbox` itself does not enforce this —
    /// it is metadata for the supervisor layer, which kills on timeout/cancel
    /// (see `OUTPUT_AND_IPC.md`: never kill for output *volume*, only for
    /// timeout / hang / explicit cancel / sandbox violation).
    pub timeout: Option<Duration>,
}

impl ToolSpec {
    /// Convenience constructor from a program and its args.
    pub fn new(
        program: impl Into<OsString>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: None,
            env: BTreeMap::new(),
            timeout: None,
        }
    }

    /// Builder: set the working directory.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Builder: set an environment variable.
    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Builder: set the wall-clock timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// How a child finished. Kept minimal on purpose; head/tail capture and log refs
/// live in the supervisor layer (see `OUTPUT_AND_IPC.md`), not in the port DTO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolExit {
    /// The process exit code, if one was reported. `None` when the process was
    /// terminated by a signal (Unix) or otherwise exited without a numeric code.
    pub code: Option<i32>,
}

impl ToolExit {
    /// A process that exited cleanly with code `0`.
    #[must_use]
    pub fn success() -> Self {
        Self { code: Some(0) }
    }

    /// `true` when the process exited with code `0`.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.code == Some(0)
    }
}

impl From<std::process::ExitStatus> for ToolExit {
    fn from(status: std::process::ExitStatus) -> Self {
        Self {
            code: status.code(),
        }
    }
}

/// The lowest layer of tool execution: spawn a child process and hand back a
/// handle that can stream its output, be waited on, and have its **whole tree**
/// killed.
///
/// `async_trait` is required for `dyn` dispatch with async methods.
#[async_trait::async_trait]
pub trait ProcessSandbox: Send + Sync {
    /// Spawn the process described by `spec`. The returned [`SpawnedProcess`]
    /// owns the child; dropping it follows the adapter's drop policy (the Windows
    /// adapter does **not** kill on drop — callers must call [`SpawnedProcess::kill_tree`]
    /// explicitly).
    async fn spawn(&self, spec: ToolSpec) -> anyhow::Result<Box<dyn SpawnedProcess>>;
}

/// A handle to a spawned child process.
///
/// `take_stdout`/`take_stderr` return boxed [`AsyncRead`] rather than concrete
/// `tokio::process` handles, so the trait stays a real platform boundary (a future
/// cgroup-exec or remote-exec adapter can satisfy it too). Drain both streams in
/// **separate concurrent tasks** — never one-then-the-other — or the child can
/// block writing to the pipe you are not reading (`OUTPUT_AND_IPC.md`).
#[async_trait::async_trait]
pub trait SpawnedProcess: Send {
    /// The process ID of the top-level spawned process.
    fn pid(&self) -> u32;

    /// Take ownership of the child's stdout stream. Returns `None` if stdout was
    /// not captured or has already been taken.
    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>>;

    /// Take ownership of the child's stderr stream. Returns `None` if stderr was
    /// not captured or has already been taken.
    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>>;

    /// Wait for the process to exit and return its [`ToolExit`].
    ///
    /// Implementations must be safe to call repeatedly after exit and return the
    /// same result each time.
    async fn wait(&mut self) -> anyhow::Result<ToolExit>;

    /// Kill the **entire process tree** rooted at this child (children,
    /// grandchildren, …) and wait for it to be reaped.
    ///
    /// The strength of this guarantee differs by OS (`PROCESS_SANDBOX.md`):
    /// Windows is bulletproof (Job Object), Linux can be (cgroup v2), macOS is
    /// best-effort (process group/session + reaper). Callers must therefore *not*
    /// assume a perfectly clean tree-kill on every platform.
    async fn kill_tree(&mut self) -> anyhow::Result<()>;
}
