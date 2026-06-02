//! A [`ProcessSandbox`] that spawns no real process — it hands back canned
//! stdout/stderr and a fake exit. This is the runtime *capability/mock* axis from
//! `CROSS_PLATFORM_PATTERN.md` (a `dyn` swap), distinct from the compile-time
//! platform axis. Use it to test the supervisor/drain layers without touching the
//! OS.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::io::AsyncRead;

use crate::port::{ProcessSandbox, SpawnedProcess, ToolExit, ToolSpec};

/// Canned behaviour for one spawn.
#[derive(Debug, Clone, Default)]
pub struct MockResponse {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit: Option<ToolExit>,
}

/// A fake sandbox returning a fixed [`MockResponse`] for every spawn.
#[derive(Debug, Clone)]
pub struct MockSandbox {
    response: MockResponse,
    next_pid: Arc<AtomicU32>,
}

impl MockSandbox {
    /// A mock that returns the given stdout, empty stderr, and exit code 0.
    #[must_use]
    pub fn with_stdout(stdout: impl Into<Vec<u8>>) -> Self {
        Self::new(MockResponse {
            stdout: stdout.into(),
            stderr: Vec::new(),
            exit: Some(ToolExit::success()),
        })
    }

    /// A fully-specified mock.
    #[must_use]
    pub fn new(response: MockResponse) -> Self {
        Self {
            response,
            // Start from a recognizable fake-PID base so it can't be confused with
            // a real OS pid in logs/tests.
            next_pid: Arc::new(AtomicU32::new(900_000)),
        }
    }
}

impl Default for MockSandbox {
    fn default() -> Self {
        Self::new(MockResponse {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: Some(ToolExit::success()),
        })
    }
}

#[async_trait::async_trait]
impl ProcessSandbox for MockSandbox {
    async fn spawn(&self, _spec: ToolSpec) -> anyhow::Result<Box<dyn SpawnedProcess>> {
        let pid = self.next_pid.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(MockProcess {
            pid,
            stdout: Some(self.response.stdout.clone()),
            stderr: Some(self.response.stderr.clone()),
            exit: self.response.exit.unwrap_or_else(ToolExit::success),
            killed: false,
        }))
    }
}

#[derive(Debug)]
struct MockProcess {
    pid: u32,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
    exit: ToolExit,
    killed: bool,
}

#[async_trait::async_trait]
impl SpawnedProcess for MockProcess {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        self.stdout
            .take()
            .map(|b| Box::new(std::io::Cursor::new(b)) as Box<dyn AsyncRead + Send + Unpin>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        self.stderr
            .take()
            .map(|b| Box::new(std::io::Cursor::new(b)) as Box<dyn AsyncRead + Send + Unpin>)
    }

    async fn wait(&mut self) -> anyhow::Result<ToolExit> {
        // A killed mock reports a non-zero, signal-like exit.
        if self.killed {
            return Ok(ToolExit { code: Some(1) });
        }
        Ok(self.exit)
    }

    async fn kill_tree(&mut self) -> anyhow::Result<()> {
        self.killed = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{drain_parallel, OutputPolicy};

    #[tokio::test]
    async fn mock_yields_canned_output_and_exit() {
        let sandbox = MockSandbox::new(MockResponse {
            stdout: b"hello stdout".to_vec(),
            stderr: b"hello stderr".to_vec(),
            exit: Some(ToolExit { code: Some(0) }),
        });

        let mut proc = sandbox.spawn(ToolSpec::new("echo", ["hi"])).await.unwrap();
        assert!(proc.pid() >= 900_000);

        let out = proc.take_stdout();
        let err = proc.take_stderr();
        // Second take is None (already taken).
        assert!(proc.take_stdout().is_none());

        let (o, e) = drain_parallel(out, err, OutputPolicy::default())
            .await
            .unwrap();
        assert_eq!(o.unwrap().into_string_lossy(), "hello stdout");
        assert_eq!(e.unwrap().into_string_lossy(), "hello stderr");

        let exit = proc.wait().await.unwrap();
        assert!(exit.is_success());
    }

    #[tokio::test]
    async fn mock_kill_changes_exit() {
        let sandbox = MockSandbox::with_stdout("data");
        let mut proc = sandbox
            .spawn(ToolSpec::new("sleep", ["100"]))
            .await
            .unwrap();
        proc.kill_tree().await.unwrap();
        let exit = proc.wait().await.unwrap();
        assert_eq!(exit.code, Some(1));
        assert!(!exit.is_success());
    }
}
