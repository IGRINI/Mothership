//! Windows [`ProcessSandbox`] adapter, built on `process-wrap`'s Job Object
//! wrapper.
//!
//! ## Why this is the bulletproof platform
//!
//! A Windows [Job Object] with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` kills *every*
//! process in the job atomically. `process-wrap`'s [`JobObject`] wrapper sets that
//! up for us, and crucially creates the child **suspended** (`CREATE_SUSPENDED`),
//! assigns it to the job, *then* resumes it — closing the race where a grandchild
//! spawns before the job assignment lands. We therefore do **not** hand-roll any
//! Win32; `process-wrap` does it race-free (see `PROCESS_SANDBOX.md`).
//!
//! `kill_tree` terminates the whole Job Object via the wrapper's `start_kill`
//! (`TerminateJobObject`), so children *and grandchildren* die — not just the
//! direct child. This is the property the kill-tree integration test proves.
//!
//! [Job Object]: https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
//! [`JobObject`]: process_wrap::tokio::JobObject

use std::process::Stdio;

use anyhow::Context;
use process_wrap::tokio::{ChildWrapper, CommandWrap, JobObject};
use tokio::io::AsyncRead;
use tokio::process::Command;

use crate::port::{ProcessSandbox, SpawnedProcess, ToolExit, ToolSpec};

/// Spawns processes inside a Windows Job Object so the whole tree can be killed.
#[derive(Debug, Clone, Default)]
pub struct JobObjectSandbox;

impl JobObjectSandbox {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl ProcessSandbox for JobObjectSandbox {
    async fn spawn(&self, spec: ToolSpec) -> anyhow::Result<Box<dyn SpawnedProcess>> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        for (k, v) in &spec.env {
            command.env(k, v);
        }
        // We manage tree-kill explicitly via the Job Object; do NOT also enable
        // tokio's kill_on_drop (it only kills the direct child anyway).
        command.kill_on_drop(false);

        // Lazily wrap with the Job Object. `process-wrap` applies CREATE_SUSPENDED
        // and the job assignment at spawn time, race-free.
        let mut wrap = CommandWrap::from(command);
        wrap.wrap(JobObject);

        let child = wrap
            .spawn()
            .with_context(|| format!("failed to spawn process: {:?}", spec.program))?;

        let pid = child
            .id()
            .context("spawned child has no PID (already exited?)")?;

        Ok(Box::new(JobObjectProcess {
            child: Some(child),
            pid,
        }))
    }
}

/// A child running inside a Job Object.
struct JobObjectProcess {
    /// `Option` so [`SpawnedProcess::kill_tree`] / [`SpawnedProcess::wait`] can
    /// take ownership of the boxed wrapper when they need `self`-by-value style
    /// access, while leaving a consistent state behind.
    child: Option<Box<dyn ChildWrapper>>,
    pid: u32,
}

#[async_trait::async_trait]
impl SpawnedProcess for JobObjectProcess {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        self.child
            .as_mut()?
            .stdout()
            .take()
            .map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        self.child
            .as_mut()?
            .stderr()
            .take()
            .map(|s| Box::new(s) as Box<dyn AsyncRead + Send + Unpin>)
    }

    async fn wait(&mut self) -> anyhow::Result<ToolExit> {
        let child = self
            .child
            .as_mut()
            .context("process handle already consumed")?;
        // The JobObjectChild::wait reaps the parent and then the rest of the job.
        let status = child.wait().await.context("waiting on process failed")?;
        Ok(ToolExit::from(status))
    }

    async fn kill_tree(&mut self) -> anyhow::Result<()> {
        let child = match self.child.as_mut() {
            Some(c) => c,
            None => return Ok(()), // already taken/killed — nothing to do
        };
        // `kill()` => `start_kill()` (TerminateJobObject on the whole job) then
        // `wait()`. Terminating the job kills children AND grandchildren.
        Box::into_pin(child.kill())
            .await
            .context("terminating job object failed")?;
        Ok(())
    }
}
