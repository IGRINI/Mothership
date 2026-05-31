use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::AsyncRead;

#[derive(Debug, Clone, Default)]
pub struct ToolProcessSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolProcessExit {
    pub code: Option<i32>,
}

impl ToolProcessExit {
    pub fn success() -> Self {
        Self { code: Some(0) }
    }

    pub fn is_success(self) -> bool {
        self.code == Some(0)
    }
}

#[async_trait::async_trait]
pub trait ToolProcessSandbox: Send + Sync {
    async fn spawn(&self, spec: ToolProcessSpec) -> crate::Result<Box<dyn SpawnedToolProcess>>;
}

#[async_trait::async_trait]
pub trait SpawnedToolProcess: Send {
    fn pid(&self) -> u32;

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>>;

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>>;

    async fn wait(&mut self) -> crate::Result<ToolProcessExit>;

    async fn kill_tree(&mut self) -> crate::Result<()>;
}
