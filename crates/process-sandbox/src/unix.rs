//! Non-Windows placeholder adapter.
//!
//! The real Unix adapter (process group/session) and the macOS reaper are a LATER
//! task (T3.5 in `MIGRATION_PLAN.md`). This stub exists only so the crate
//! **compiles** on Linux/macOS today — per `CROSS_PLATFORM_PATTERN.md`,
//! `#[cfg]`-gated code that no one compiles silently rots, and we must not use
//! `compile_error!`.
//!
//! It deliberately does NOT spawn anything: `spawn` returns an error explaining
//! that the Unix adapter is not implemented yet, so a non-Windows binary fails
//! loudly and obviously rather than silently mis-killing process trees.

use tokio::io::AsyncRead;

use crate::port::{ProcessSandbox, SpawnedProcess, ToolExit, ToolSpec};

/// Placeholder sandbox for non-Windows targets. Not yet implemented.
#[derive(Debug, Clone, Default)]
pub struct UnimplementedSandbox;

impl UnimplementedSandbox {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl ProcessSandbox for UnimplementedSandbox {
    async fn spawn(&self, _spec: ToolSpec) -> anyhow::Result<Box<dyn SpawnedProcess>> {
        anyhow::bail!(
            "process-sandbox: the Unix/macOS adapter is not implemented yet (T3.5). \
             Only the Windows Job Object adapter exists today."
        )
    }
}

/// Placeholder process handle. Its methods are unreachable in practice because
/// [`UnimplementedSandbox::spawn`] never returns one, but they are defined (not
/// `compile_error!`) so the type is complete and the crate builds everywhere.
#[allow(dead_code)]
struct UnimplementedProcess;

#[async_trait::async_trait]
impl SpawnedProcess for UnimplementedProcess {
    fn pid(&self) -> u32 {
        unimplemented!("Unix/macOS ProcessSandbox adapter is not implemented yet (T3.5)")
    }

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        unimplemented!("Unix/macOS ProcessSandbox adapter is not implemented yet (T3.5)")
    }

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        unimplemented!("Unix/macOS ProcessSandbox adapter is not implemented yet (T3.5)")
    }

    async fn wait(&mut self) -> anyhow::Result<ToolExit> {
        unimplemented!("Unix/macOS ProcessSandbox adapter is not implemented yet (T3.5)")
    }

    async fn kill_tree(&mut self) -> anyhow::Result<()> {
        unimplemented!("Unix/macOS ProcessSandbox adapter is not implemented yet (T3.5)")
    }
}
