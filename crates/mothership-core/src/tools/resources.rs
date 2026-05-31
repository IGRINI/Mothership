use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use crate::{MothershipError, Result};

use super::cancellation::ToolCancellationToken;
use super::types::{event, ToolExecutionEventKind, ToolExecutionEventSink, ToolExecutionRequest};

#[derive(Debug, Clone, Copy)]
pub struct ToolResourceLimits {
    pub max_shell_processes: usize,
    pub max_shell_processes_per_project: usize,
    pub max_git_ops_per_project: usize,
}

impl Default for ToolResourceLimits {
    fn default() -> Self {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4);
        Self {
            max_shell_processes: (workers * 2).clamp(4, 32),
            max_shell_processes_per_project: 4,
            max_git_ops_per_project: 1,
        }
    }
}

pub(crate) struct ToolResourceGate {
    limits: ToolResourceLimits,
    shell_global: Arc<Semaphore>,
    shell_by_project: Mutex<HashMap<String, Arc<Semaphore>>>,
    git_by_project: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl ToolResourceGate {
    pub(crate) fn new(limits: ToolResourceLimits) -> Self {
        Self {
            shell_global: Arc::new(Semaphore::new(limits.max_shell_processes)),
            shell_by_project: Mutex::new(HashMap::new()),
            git_by_project: Mutex::new(HashMap::new()),
            limits,
        }
    }

    pub(crate) async fn acquire(
        &self,
        request: &ToolExecutionRequest,
        cancellation: &ToolCancellationToken,
        sink: &Arc<dyn ToolExecutionEventSink>,
    ) -> Result<ResourceLeases> {
        let mut semaphores = vec![Arc::clone(&self.shell_global)];
        let project_key = project_key(request);

        if let Some(project_key) = project_key.as_deref() {
            semaphores.push(self.project_shell(project_key).await);
            if is_git_command(request) {
                semaphores.push(self.project_git(project_key).await);
            }
        }

        if semaphores
            .iter()
            .any(|semaphore| semaphore.available_permits() == 0)
        {
            let mut event = event(request, ToolExecutionEventKind::WaitingForResource);
            event.message = Some("waiting for tool execution capacity".to_string());
            sink.emit(event);
        }

        let mut leases = Vec::with_capacity(semaphores.len());
        for semaphore in semaphores {
            let permit = tokio::select! {
                permit = semaphore.acquire_owned() => permit.map_err(|_| {
                    MothershipError::Runtime("tool resource gate was closed".to_string())
                })?,
                _ = cancellation.cancelled() => {
                    return Err(MothershipError::Runtime("tool call cancelled while waiting for resources".to_string()));
                }
            };
            leases.push(permit);
        }

        Ok(ResourceLeases { _permits: leases })
    }

    async fn project_shell(&self, project_key: &str) -> Arc<Semaphore> {
        let mut map = self.shell_by_project.lock().await;
        Arc::clone(map.entry(project_key.to_string()).or_insert_with(|| {
            Arc::new(Semaphore::new(self.limits.max_shell_processes_per_project))
        }))
    }

    async fn project_git(&self, project_key: &str) -> Arc<Semaphore> {
        let mut map = self.git_by_project.lock().await;
        Arc::clone(
            map.entry(project_key.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(self.limits.max_git_ops_per_project))),
        )
    }
}

pub(crate) struct ResourceLeases {
    _permits: Vec<OwnedSemaphorePermit>,
}

fn project_key(request: &ToolExecutionRequest) -> Option<String> {
    request
        .project_id
        .clone()
        .or_else(|| request.cwd.as_ref().map(|path| path.display().to_string()))
}

fn is_git_command(request: &ToolExecutionRequest) -> bool {
    request
        .command
        .program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&request.command.program)
        .eq_ignore_ascii_case("git")
}
