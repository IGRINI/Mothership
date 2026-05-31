use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mothership_adapter_host::AdapterEntry;

use crate::adapter_pool::AdapterPool;
use crate::auth::FileCredentialVault;
use crate::llm::{LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest};
use crate::subprocess_gateway::SubprocessChatGateway;
use crate::{ChatCancellationToken, Result};

/// Tracks provider runtime health without throttling agent execution.
///
/// Mothership's product model allows many agents and subagents to run at once.
/// This manager must not serialize or cap provider calls globally. Concurrency is
/// delegated to the provider adapter layer: a free resident process is reused;
/// otherwise the adapter pool starts an independent one-off process for that
/// request. This object only keeps counters and exposes emergency eviction.
pub struct ProviderRuntimeManager {
    pool: Arc<AdapterPool>,
    providers: Mutex<HashMap<String, ProviderRuntimeStats>>,
}

#[derive(Default)]
struct ProviderRuntimeStats {
    active: usize,
    completed: u64,
    failed: u64,
    cancelled: u64,
    last_error: Option<String>,
}

struct ProviderRunGuard<'a> {
    manager: &'a ProviderRuntimeManager,
    provider_id: String,
}

impl ProviderRuntimeManager {
    pub fn new(pool: Arc<AdapterPool>) -> Self {
        Self {
            pool,
            providers: Mutex::new(HashMap::new()),
        }
    }

    pub fn force_evict(&self, provider_id: &str) {
        self.pool.force_evict(provider_id);
    }

    pub fn complete_subprocess_chat(
        &self,
        entry: AdapterEntry,
        vault: FileCredentialVault,
        request: LlmChatCompletionRequest,
        cancellation: &ChatCancellationToken,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<String> {
        let provider_id = request.provider_id.clone();
        let _guard = self.start_run(&provider_id);
        let result = SubprocessChatGateway::new(Arc::clone(&self.pool), entry, vault)
            .complete_chat(request, cancellation, sink);

        if cancellation.is_cancelled() {
            self.record_cancelled(&provider_id);
        } else if let Err(error) = &result {
            self.record_failure(&provider_id, error.to_string());
        } else {
            self.record_success(&provider_id);
        }

        result
    }

    fn start_run(&self, provider_id: &str) -> ProviderRunGuard<'_> {
        let mut providers = self.providers.lock().unwrap();
        providers
            .entry(provider_id.to_string())
            .or_default()
            .active += 1;
        ProviderRunGuard {
            manager: self,
            provider_id: provider_id.to_string(),
        }
    }

    fn finish_run(&self, provider_id: &str) {
        if let Some(stats) = self.providers.lock().unwrap().get_mut(provider_id) {
            stats.active = stats.active.saturating_sub(1);
        }
    }

    fn record_success(&self, provider_id: &str) {
        let mut providers = self.providers.lock().unwrap();
        let stats = providers.entry(provider_id.to_string()).or_default();
        stats.completed += 1;
        stats.last_error = None;
    }

    fn record_cancelled(&self, provider_id: &str) {
        let mut providers = self.providers.lock().unwrap();
        providers
            .entry(provider_id.to_string())
            .or_default()
            .cancelled += 1;
    }

    fn record_failure(&self, provider_id: &str, error: String) {
        let mut providers = self.providers.lock().unwrap();
        let stats = providers.entry(provider_id.to_string()).or_default();
        stats.failed += 1;
        stats.last_error = Some(error);
    }
}

impl Default for ProviderRuntimeManager {
    fn default() -> Self {
        Self::new(Arc::new(AdapterPool::new()))
    }
}

impl Drop for ProviderRunGuard<'_> {
    fn drop(&mut self) {
        self.manager.finish_run(&self.provider_id);
    }
}
