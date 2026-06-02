use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mothership_adapter_host::AdapterEntry;
use serde::{Deserialize, Serialize};

use crate::adapter_pool::AdapterPool;
use crate::auth::FileCredentialVault;
use crate::llm::{
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest,
    LlmToolCallHandler,
};
use crate::subprocess_gateway::SubprocessChatGateway;
use crate::{ChatCancellationToken, MothershipError, Result};

const BACKOFF_AFTER_CONSECUTIVE_FAILURES: u32 = 3;
const BASE_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRuntimeHealth {
    Healthy,
    Degraded,
    CoolingDown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRuntimeStatus {
    pub health: ProviderRuntimeHealth,
    pub active: usize,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub consecutive_failures: u32,
    pub retry_after_ms: Option<u64>,
    pub last_error: Option<String>,
}

impl ProviderRuntimeStatus {
    pub fn idle() -> Self {
        Self {
            health: ProviderRuntimeHealth::Healthy,
            active: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
            consecutive_failures: 0,
            retry_after_ms: None,
            last_error: None,
        }
    }
}

/// Tracks provider runtime health and protects Core from repeated provider churn.
///
/// Mothership's product model allows many agents and subagents to run at once.
/// This manager must not serialize or cap provider calls globally. Concurrency is
/// delegated to the provider adapter layer: a free resident process is reused;
/// otherwise the adapter pool starts an independent one-off process for that
/// request. This object keeps counters, exposes emergency eviction, and applies
/// a short per-provider cool-down only after repeated failures.
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
    consecutive_failures: u32,
    backoff_until: Option<Instant>,
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
        run_id: Option<String>,
        tool_handler: Option<Arc<dyn LlmToolCallHandler>>,
        request: LlmChatCompletionRequest,
        cancellation: &ChatCancellationToken,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<String> {
        let provider_id = request.provider_id.clone();
        let _guard = self.start_run(&provider_id)?;
        let mut gateway = SubprocessChatGateway::new(Arc::clone(&self.pool), entry, vault);
        if let Some(run_id) = run_id {
            gateway = gateway.with_run_id(run_id);
        }
        if let Some(tool_handler) = tool_handler {
            gateway = gateway.with_tool_handler(tool_handler);
        }
        let result = gateway.complete_chat(request, cancellation, sink);

        if cancellation.is_cancelled() {
            self.record_cancelled(&provider_id);
        } else if let Err(error) = &result {
            self.record_failure(&provider_id, error.to_string());
        } else {
            self.record_success(&provider_id);
        }

        result
    }

    pub fn status(&self, provider_id: &str) -> ProviderRuntimeStatus {
        let providers = self.providers.lock().unwrap();
        providers
            .get(provider_id)
            .map(status_from_stats)
            .unwrap_or_else(ProviderRuntimeStatus::idle)
    }

    fn start_run(&self, provider_id: &str) -> Result<ProviderRunGuard<'_>> {
        let mut providers = self.providers.lock().unwrap();
        let stats = providers.entry(provider_id.to_string()).or_default();
        if let Some(backoff_until) = stats.backoff_until {
            let now = Instant::now();
            if backoff_until > now {
                let retry_after = backoff_until.saturating_duration_since(now);
                return Err(MothershipError::InvalidRequest(format!(
                    "provider {provider_id} is cooling down after repeated failures; retry in {} ms",
                    retry_after.as_millis()
                )));
            }
            stats.backoff_until = None;
        }
        stats.active += 1;
        Ok(ProviderRunGuard {
            manager: self,
            provider_id: provider_id.to_string(),
        })
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
        stats.consecutive_failures = 0;
        stats.backoff_until = None;
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
        stats.consecutive_failures = stats.consecutive_failures.saturating_add(1);
        if stats.consecutive_failures >= BACKOFF_AFTER_CONSECUTIVE_FAILURES {
            stats.backoff_until =
                Some(Instant::now() + backoff_duration(stats.consecutive_failures));
        }
        stats.last_error = Some(error);
    }
}

fn status_from_stats(stats: &ProviderRuntimeStats) -> ProviderRuntimeStatus {
    let retry_after = stats.backoff_until.and_then(|until| {
        let now = Instant::now();
        (until > now).then(|| until.saturating_duration_since(now))
    });
    let health = if retry_after.is_some() {
        ProviderRuntimeHealth::CoolingDown
    } else if stats.consecutive_failures > 0 || stats.last_error.is_some() {
        ProviderRuntimeHealth::Degraded
    } else {
        ProviderRuntimeHealth::Healthy
    };

    ProviderRuntimeStatus {
        health,
        active: stats.active,
        completed: stats.completed,
        failed: stats.failed,
        cancelled: stats.cancelled,
        consecutive_failures: stats.consecutive_failures,
        retry_after_ms: retry_after.map(|duration| duration.as_millis() as u64),
        last_error: stats.last_error.clone(),
    }
}

fn backoff_duration(consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures
        .saturating_sub(BACKOFF_AFTER_CONSECUTIVE_FAILURES)
        .min(4);
    let multiplier = 1u32.checked_shl(exponent).unwrap_or(16);
    (BASE_BACKOFF * multiplier).min(MAX_BACKOFF)
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
