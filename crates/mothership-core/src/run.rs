use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mothership_adapter_host::protocol::{PromptBundle, RuntimeContext, ToolDescriptor};

use crate::adapter_pool::{with_adapter_use_observer, AdapterKillHandle};
use crate::agentic::AgenticLoopPolicy;
use crate::auth::FileCredentialVault;
use crate::chat::{
    ActiveRunSummary, ChatCancellationToken, ChatRunEvent, ChatRunEventKind, ChatRunEventSink,
    SendChatMessageResult,
};
use crate::connectors::{ensure_adapter_can_run_chat, find_trusted_adapter_entry};
use crate::llm::{
    LlmChatCompletionEventSink, LlmChatCompletionRequest, LlmChatMessage, LlmChatRole,
    LlmChatRoundRequest, LlmToolCallHandler, LlmToolCallRequest, LlmToolCallResponse,
    LlmTransportKind, ProviderRequestPipeline, ProviderRuntimeKind,
};
use crate::prompt::{
    append_personalization_sections, prompt_preview_for_bundle, runtime_prompt_bundle_for,
    PromptPreview,
};
use crate::provider_runtime::ProviderRuntimeManager;
use crate::tools::{default_tool_catalog, CatalogPin};
use crate::{Database, MothershipError, Result};

const CHAT_CONTEXT_LIMIT: i64 = 80;
const CHAT_DELTA_FLUSH_BYTES: usize = 1024;
const CHAT_DELTA_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const CHAT_CANCEL_PROCESS_GRACE: Duration = Duration::from_secs(10);
const MAX_PENDING_RUN_INPUTS: usize = 64;
const MAX_PENDING_RUN_INPUT_CHARS: usize = 8_000;
#[derive(Default)]
pub struct ChatRunRegistry {
    inner: Mutex<ChatRunRegistryState>,
}

#[derive(Default)]
struct ChatRunRegistryState {
    active: HashMap<String, ActiveChatRun>,
    pending_cancelled: HashSet<String>,
}

struct ActiveChatRun {
    provider_id: String,
    cancellation: ChatCancellationToken,
    /// Kill handle for the adapter process serving the run's current round,
    /// present only while a round is in flight. This is the *actual* process —
    /// resident or ephemeral — so the cancel fallback never has to guess.
    kill_handle: Option<AdapterKillHandle>,
    /// Display snapshot served by `list_active_runs`, so a client connecting
    /// mid-run can show agent activity without re-deriving chat/project names.
    summary: ActiveRunSummary,
    /// User steering messages and runtime notifications that arrived while the
    /// current provider round was busy. Core injects them into the next round's
    /// `extra_messages`; the UI remains a thin sender.
    pending_inputs: Vec<LlmChatMessage>,
}

impl ChatRunRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a run after its provider is known. Returns true when a cancel
    /// request already arrived and the token was cancelled immediately.
    fn register(
        &self,
        run_id: &str,
        summary: ActiveRunSummary,
        cancellation: ChatCancellationToken,
    ) -> bool {
        let mut state = self.inner.lock().unwrap();
        let already_cancelled = state.pending_cancelled.remove(run_id);
        if already_cancelled {
            cancellation.cancel();
        }
        state.active.insert(
            run_id.to_string(),
            ActiveChatRun {
                provider_id: summary.provider_id.clone(),
                cancellation,
                kill_handle: None,
                summary,
                pending_inputs: Vec::new(),
            },
        );
        already_cancelled
    }

    /// Snapshot of every in-flight run, for `list_active_runs`. Order is
    /// unspecified; clients sort for display.
    pub fn snapshot(&self) -> Vec<ActiveRunSummary> {
        let state = self.inner.lock().unwrap();
        state
            .active
            .values()
            .map(|run| run.summary.clone())
            .collect()
    }

    /// Records the adapter process currently serving `run_id`'s round.
    fn note_adapter_use(&self, run_id: &str, handle: AdapterKillHandle) {
        let mut state = self.inner.lock().unwrap();
        if let Some(run) = state.active.get_mut(run_id) {
            run.kill_handle = Some(handle);
        }
    }

    /// Drops the recorded adapter process once a round finishes: a resident
    /// may immediately serve other runs, so it must no longer be a kill target
    /// for this one.
    fn clear_adapter_use(&self, run_id: &str) {
        let mut state = self.inner.lock().unwrap();
        if let Some(run) = state.active.get_mut(run_id) {
            run.kill_handle = None;
        }
    }

    /// Marks a run cancelled. If it is active, returns its provider id so the
    /// caller can schedule a process-level fallback. If the run has not reached
    /// provider selection yet, the cancellation is remembered and applied when
    /// it registers.
    pub fn cancel(&self, run_id: &str) -> Option<String> {
        let mut state = self.inner.lock().unwrap();
        if let Some(run) = state.active.get(run_id) {
            run.cancellation.cancel();
            return Some(run.provider_id.clone());
        }
        state.pending_cancelled.insert(run_id.to_string());
        None
    }

    pub fn push_user_input(&self, run_id: &str, content: &str) -> bool {
        self.push_input(
            run_id,
            LlmChatMessage {
                role: LlmChatRole::User,
                content: bounded_pending_input(content),
            },
        )
    }

    pub fn push_runtime_input(&self, run_id: &str, content: String) -> bool {
        self.push_input(
            run_id,
            LlmChatMessage {
                role: LlmChatRole::User,
                content: bounded_pending_input(&content),
            },
        )
    }

    pub(crate) fn drain_pending_inputs(&self, run_id: &str) -> Vec<LlmChatMessage> {
        let mut state = self.inner.lock().unwrap();
        state
            .active
            .get_mut(run_id)
            .map(|run| std::mem::take(&mut run.pending_inputs))
            .unwrap_or_default()
    }

    fn push_input(&self, run_id: &str, input: LlmChatMessage) -> bool {
        let mut state = self.inner.lock().unwrap();
        let Some(run) = state.active.get_mut(run_id) else {
            return false;
        };
        if run.cancellation.is_cancelled() || run.pending_inputs.len() >= MAX_PENDING_RUN_INPUTS {
            return false;
        }
        run.pending_inputs.push(input);
        true
    }

    /// The kill handle of the adapter process serving `run_id`, if that run is
    /// still active and has been cancelled. `None` means the run already
    /// finished, was never cancelled, or is not currently inside an adapter
    /// round — in all of which cases there is nothing safe to kill.
    fn cancelled_kill_target(&self, run_id: &str) -> Option<AdapterKillHandle> {
        let state = self.inner.lock().unwrap();
        state
            .active
            .get(run_id)
            .filter(|run| run.cancellation.is_cancelled())
            .and_then(|run| run.kill_handle.clone())
    }

    fn finish(&self, run_id: &str) {
        let mut state = self.inner.lock().unwrap();
        state.active.remove(run_id);
        state.pending_cancelled.remove(run_id);
    }
}

fn bounded_pending_input(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.chars().count() <= MAX_PENDING_RUN_INPUT_CHARS {
        return trimmed.to_string();
    }
    let mut out = trimmed
        .chars()
        .take(MAX_PENDING_RUN_INPUT_CHARS)
        .collect::<String>();
    out.push_str("\n[message truncated]");
    out
}

/// Wall-clock unix milliseconds, for `ActiveRunSummary.started_at_ms`.
fn unix_timestamp_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// Orchestrates a single streaming chat run inside Core.
///
/// It selects the configured model, finds the subprocess adapter that provides
/// it, builds the chat context, drives the completion through that adapter, and
/// records the terminal state in the database. The core is provider-agnostic —
/// every provider is a runtime-loaded adapter. All observable run progress is
/// forwarded to the injected [`ChatRunEventSink`].
pub struct ChatRunService<'a> {
    database: &'a Database,
    providers: Arc<ProviderRuntimeManager>,
    tool_handler: Option<Arc<dyn LlmToolCallHandler>>,
    extra_tools: Vec<ToolDescriptor>,
    provider_pipeline: ProviderRequestPipeline,
}

impl<'a> ChatRunService<'a> {
    pub fn new(database: &'a Database, providers: Arc<ProviderRuntimeManager>) -> Self {
        Self {
            database,
            providers,
            tool_handler: None,
            extra_tools: Vec::new(),
            provider_pipeline: ProviderRequestPipeline::default(),
        }
    }

    pub fn with_tool_handler(mut self, handler: Arc<dyn LlmToolCallHandler>) -> Self {
        self.tool_handler = Some(handler);
        self
    }

    pub fn with_extra_tools(mut self, tools: Vec<ToolDescriptor>) -> Self {
        self.extra_tools = tools;
        self
    }

    pub fn with_provider_request_pipeline(mut self, pipeline: ProviderRequestPipeline) -> Self {
        self.provider_pipeline = pipeline;
        self
    }

    /// Runs the chat completion for `run` to completion, emitting `Started`,
    /// `TransportSelected`, `Delta`, and finally `Completed` or `Failed`
    /// events through `sink`.
    pub fn run(
        &self,
        run: &SendChatMessageResult,
        registry: Arc<ChatRunRegistry>,
        sink: &mut dyn ChatRunEventSink,
    ) {
        sink.emit(ChatRunEvent {
            run_id: run.run_id.clone(),
            chat_id: run.chat.id.clone(),
            message_id: run.assistant_message.id.clone(),
            kind: ChatRunEventKind::Started,
            delta: None,
            message: Some(run.assistant_message.clone()),
            chat: Some(run.chat.clone()),
            transport: None,
            tool_call_id: None,
            removed_message_ids: run.removed_message_ids.clone(),
            error: None,
        });

        if let Err(error) = self.complete(run, Arc::clone(&registry), sink) {
            let event = self
                .database
                .fail_chat_run(
                    &run.run_id,
                    &run.chat.id,
                    &run.assistant_message.id,
                    &error.to_string(),
                )
                .unwrap_or_else(|_| ChatRunEvent {
                    run_id: run.run_id.clone(),
                    chat_id: run.chat.id.clone(),
                    message_id: run.assistant_message.id.clone(),
                    kind: ChatRunEventKind::Failed,
                    delta: None,
                    message: None,
                    chat: None,
                    transport: None,
                    tool_call_id: None,
                    removed_message_ids: Vec::new(),
                    error: Some(error.to_string()),
                });
            sink.emit(event);
        }
        registry.finish(&run.run_id);
    }

    fn complete(
        &self,
        run: &SendChatMessageResult,
        registry: Arc<ChatRunRegistry>,
        sink: &mut dyn ChatRunEventSink,
    ) -> Result<()> {
        let database = self.database;

        // Use the model FROZEN onto this run's assistant placeholder at start
        // (begin_*_run resolved it from the chat's model, falling back to the
        // global default). We deliberately do NOT re-read the global selection
        // here: switching the chat/global model mid-run must not redirect an
        // in-flight answer to a different provider.
        let provider_id = run
            .assistant_message
            .provider_id
            .clone()
            .unwrap_or_default();
        let model_id = run.assistant_message.model_id.clone().unwrap_or_default();
        if model_id.trim().is_empty() {
            return Err(MothershipError::InvalidRequest(
                "no LLM model selected; install a provider adapter and choose a model first"
                    .to_string(),
            ));
        }

        // Every provider is a subprocess adapter. Find the one that owns this
        // model; the adapter handles its own transport and auth.
        let entry = find_trusted_adapter_entry(&plugins_store_path(database), &provider_id)?;
        let runtime_kind =
            ensure_adapter_can_run_chat(&entry).map_err(MothershipError::InvalidRequest)?;

        let messages = database.llm_chat_context(
            &run.chat.id,
            &run.assistant_message.id,
            CHAT_CONTEXT_LIMIT,
            &run.context,
        )?;
        if messages.is_empty() {
            return Err(MothershipError::InvalidRequest(
                "chat context is empty".to_string(),
            ));
        }
        let project = database.chat_project(&run.chat.id)?;

        let cancellation = ChatCancellationToken::default();
        let already_cancelled = registry.register(
            &run.run_id,
            ActiveRunSummary {
                run_id: run.run_id.clone(),
                chat_id: run.chat.id.clone(),
                chat_title: run.chat.title.clone(),
                project_id: project.as_ref().map(|project| project.id.clone()),
                project_name: project.as_ref().map(|project| project.name.clone()),
                message_id: run.assistant_message.id.clone(),
                provider_id: provider_id.clone(),
                model_id: model_id.clone(),
                started_at_ms: unix_timestamp_ms(),
            },
            cancellation.clone(),
        );
        if already_cancelled {
            schedule_cancel_fallback(
                Arc::clone(&self.providers),
                Arc::clone(&registry),
                run.run_id.clone(),
                provider_id.clone(),
            );
        }

        let mut llm_sink = DbForwardingSink::new(
            database,
            sink,
            &run.run_id,
            &run.chat.id,
            &run.assistant_message.id,
        );
        // The adapter owns its own system prompt; settings (api key, base url,
        // user model list, OAuth tokens, …) live in the app's shared credential
        // vault, keyed by provider, and are pushed to the adapter on spawn.
        let vault = FileCredentialVault::new(auth_store_path(database));
        let tools = self
            .tool_handler
            .as_ref()
            .map(|_| {
                let mut tools = default_tool_catalog();
                tools.extend(self.extra_tools.clone());
                tools
            })
            .unwrap_or_default();
        // Catalog stability: pin the catalog's canonical bytes for the process and
        // warn if they ever drift — a silent tool-definition change between
        // rebuilds poisons the provider's prefix cache. When MCP/dynamic tools are
        // merged into `tools`, this check should run on the merged set (the
        // MCP-byte-pin attaches here).
        if !tools.is_empty() && self.extra_tools.is_empty() {
            static CATALOG_PIN: std::sync::OnceLock<CatalogPin> = std::sync::OnceLock::new();
            if let Err(drift) = CATALOG_PIN
                .get_or_init(|| CatalogPin::pin(&tools))
                .check(&tools)
            {
                eprintln!("mothership-core: {drift}");
            }
        }
        // Compose the prompt: Core's locked base/project sections, then the
        // user's personalization (global → provider → model) appended after them.
        // Look up by provider/model BEFORE they're moved into the request.
        let prompt = compose_runtime_prompt(
            database,
            project.as_ref(),
            runtime_kind,
            &provider_id,
            &model_id,
        );
        let request = self.provider_pipeline.apply(LlmChatCompletionRequest {
            provider_id,
            model_id,
            reasoning: run.context.reasoning.clone(),
            fast_mode: run.context.fast_mode,
            prompt,
            runtime_context: runtime_context_for(project.as_ref()),
            tools,
            messages,
        })?;

        match runtime_kind {
            ProviderRuntimeKind::CoreManaged => {
                self.complete_agentic_loop(
                    &registry,
                    entry,
                    vault,
                    request,
                    &cancellation,
                    &mut llm_sink,
                )?;
            }
            ProviderRuntimeKind::SelfManaged => {
                self.complete_self_managed_agent(
                    &registry,
                    entry,
                    vault,
                    request,
                    &cancellation,
                    &mut llm_sink,
                )?;
            }
        }

        llm_sink.flush();

        if cancellation.is_cancelled() {
            let event =
                database.cancel_chat_run(&run.run_id, &run.chat.id, &run.assistant_message.id)?;
            llm_sink.emit(event);
            return Ok(());
        }

        let event =
            database.complete_chat_run(&run.run_id, &run.chat.id, &run.assistant_message.id)?;
        llm_sink.emit(event);
        Ok(())
    }

    fn complete_agentic_loop(
        &self,
        registry: &Arc<ChatRunRegistry>,
        entry: mothership_adapter_host::AdapterEntry,
        vault: FileCredentialVault,
        request: LlmChatCompletionRequest,
        cancellation: &ChatCancellationToken,
        sink: &mut DbForwardingSink<'_>,
    ) -> Result<()> {
        let policy = AgenticLoopPolicy::default();
        let run_id = sink.run_id;
        let mut round_request = LlmChatRoundRequest::from_completion(request.clone());

        for _ in 0..policy.max_rounds() {
            round_request
                .extra_messages
                .extend(registry.drain_pending_inputs(run_id));
            let round = track_round_adapter(registry, run_id, || {
                self.providers.complete_subprocess_round(
                    entry.clone(),
                    vault.clone(),
                    Some(run_id.to_string()),
                    round_request,
                    None,
                    cancellation,
                    sink,
                )
            })?;
            sink.flush();

            if cancellation.is_cancelled() {
                return Ok(());
            }
            if round.tool_calls.is_empty() {
                let extra_messages = registry.drain_pending_inputs(run_id);
                if extra_messages.is_empty() {
                    return Ok(());
                }
                round_request = next_round_request(
                    &request,
                    round.state.unwrap_or(serde_json::Value::Null),
                    Vec::new(),
                    extra_messages,
                    true,
                );
                continue;
            }
            let state = round.state.ok_or_else(|| {
                MothershipError::InvalidRequest(
                    "adapter returned tool calls without continuation state".to_string(),
                )
            })?;
            let tool_results = self.execute_tool_batch(round.tool_calls, cancellation, sink)?;
            if cancellation.is_cancelled() {
                return Ok(());
            }

            let extra_messages = registry.drain_pending_inputs(run_id);
            round_request = next_round_request(&request, state, tool_results, extra_messages, true);
        }

        let final_request = next_round_request(
            &request,
            round_request.state.unwrap_or(serde_json::Value::Null),
            round_request.tool_results,
            vec![LlmChatMessage {
                role: LlmChatRole::User,
                content: policy.final_synthesis_prompt().to_string(),
            }],
            false,
        );

        match track_round_adapter(registry, run_id, || {
            self.providers.complete_subprocess_round(
                entry,
                vault,
                Some(run_id.to_string()),
                final_request,
                None,
                cancellation,
                sink,
            )
        }) {
            Ok(round) if round.tool_calls.is_empty() && !round.text.trim().is_empty() => Ok(()),
            Ok(_) | Err(_) => {
                sink.delta(policy.fallback_message());
                Ok(())
            }
        }
    }

    fn complete_self_managed_agent(
        &self,
        registry: &Arc<ChatRunRegistry>,
        entry: mothership_adapter_host::AdapterEntry,
        vault: FileCredentialVault,
        request: LlmChatCompletionRequest,
        cancellation: &ChatCancellationToken,
        sink: &mut DbForwardingSink<'_>,
    ) -> Result<()> {
        let provider_id = request.provider_id.clone();
        let run_id = sink.run_id;
        let mut round_request = LlmChatRoundRequest::from_completion(request);
        round_request.state = self
            .database
            .chat_provider_state(sink.chat_id, &provider_id)?;

        let round = track_round_adapter(registry, run_id, || {
            self.providers.complete_subprocess_round(
                entry,
                vault,
                Some(run_id.to_string()),
                round_request,
                self.tool_handler.clone(),
                cancellation,
                sink,
            )
        })?;
        sink.flush();

        if !round.tool_calls.is_empty() {
            return Err(MothershipError::InvalidRequest(
                "self-managed agent adapter returned Core tool calls; advertise core-managed chat instead"
                    .to_string(),
            ));
        }

        if let Some(state) = round.state {
            self.database
                .save_chat_provider_state(sink.chat_id, &provider_id, &state)?;
        }

        Ok(())
    }

    fn execute_tool_batch(
        &self,
        mut calls: Vec<LlmToolCallRequest>,
        cancellation: &ChatCancellationToken,
        sink: &mut DbForwardingSink<'_>,
    ) -> Result<Vec<LlmToolCallResponse>> {
        let handler = self.tool_handler.as_ref().ok_or_else(|| {
            MothershipError::InvalidRequest(
                "model requested a tool call, but tools are not enabled for this run".to_string(),
            )
        })?;
        for call in &mut calls {
            call.run_id = Some(sink.run_id.to_string());
            sink.before_tool_call(&call.tool_call_id);
        }
        let results = handler.handle_tool_calls(calls.clone(), cancellation);
        if results.len() != calls.len() {
            return Err(MothershipError::Runtime(format!(
                "tool handler returned {} result(s) for {} tool call(s)",
                results.len(),
                calls.len()
            )));
        }
        Ok(calls
            .into_iter()
            .zip(results)
            .map(|(call, result)| LlmToolCallResponse {
                tool_call_id: call.tool_call_id,
                result,
            })
            .collect())
    }
}

pub fn chat_prompt_preview(database: &Database, chat_id: &str) -> Result<PromptPreview> {
    let conversation = database.get_chat(chat_id, 1)?;
    let selected = database.selected_llm_model()?;
    let (provider_id, model_id) = match (
        conversation.chat.provider_id.as_deref(),
        conversation.chat.model_id.as_deref(),
    ) {
        (Some(provider_id), Some(model_id))
            if !provider_id.trim().is_empty() && !model_id.trim().is_empty() =>
        {
            (provider_id.to_string(), model_id.to_string())
        }
        _ => (selected.provider_id, selected.model_id),
    };
    if model_id.trim().is_empty() {
        return Err(MothershipError::InvalidRequest(
            "no LLM model selected; install a provider adapter and choose a model first"
                .to_string(),
        ));
    }

    let entry = find_trusted_adapter_entry(&plugins_store_path(database), &provider_id)?;
    let runtime_kind =
        ensure_adapter_can_run_chat(&entry).map_err(MothershipError::InvalidRequest)?;
    let project = database.chat_project(chat_id)?;
    let prompt = compose_runtime_prompt(
        database,
        project.as_ref(),
        runtime_kind,
        &provider_id,
        &model_id,
    );

    Ok(prompt_preview_for_bundle(
        chat_id.to_string(),
        provider_id,
        model_id,
        runtime_kind,
        project.as_ref(),
        prompt,
    ))
}

fn compose_runtime_prompt(
    database: &Database,
    project: Option<&crate::ProjectSummary>,
    runtime_kind: ProviderRuntimeKind,
    provider_id: &str,
    model_id: &str,
) -> PromptBundle {
    let mut prompt = runtime_prompt_bundle_for(project, runtime_kind);
    append_personalization_sections(
        &mut prompt,
        database
            .personalization_for(provider_id, model_id)
            .unwrap_or_default(),
    );
    prompt
}

fn runtime_context_for(project: Option<&crate::ProjectSummary>) -> RuntimeContext {
    RuntimeContext {
        project_id: project.map(|project| project.id.clone()),
        project_name: project.map(|project| project.name.clone()),
        project_root: project.map(|project| project.path.clone()),
    }
}

fn next_round_request(
    base: &LlmChatCompletionRequest,
    state: serde_json::Value,
    tool_results: Vec<LlmToolCallResponse>,
    extra_messages: Vec<LlmChatMessage>,
    include_tools: bool,
) -> LlmChatRoundRequest {
    LlmChatRoundRequest {
        provider_id: base.provider_id.clone(),
        model_id: base.model_id.clone(),
        reasoning: base.reasoning.clone(),
        fast_mode: base.fast_mode,
        prompt: base.prompt.clone(),
        runtime_context: base.runtime_context.clone(),
        tools: include_tools
            .then(|| base.tools.clone())
            .unwrap_or_default(),
        messages: base.messages.clone(),
        state: Some(state),
        tool_results,
        extra_messages,
    }
}

/// Schedules the process-level fallback for a cancelled run: if cooperative
/// cancellation hasn't ended the run within the grace period, kill the adapter
/// process actually serving it — the resident (evicting it from the pool) or
/// the run's own ephemeral spawn — never another run's healthy adapter.
///
/// `_providers` / `_provider_id` are kept for caller compatibility; targeting
/// now flows through the registry's per-run kill handle instead of blanket
/// provider eviction.
pub fn schedule_cancel_fallback(
    _providers: Arc<ProviderRuntimeManager>,
    registry: Arc<ChatRunRegistry>,
    run_id: String,
    _provider_id: String,
) {
    thread::spawn(move || {
        thread::sleep(CHAT_CANCEL_PROCESS_GRACE);
        if let Some(handle) = registry.cancelled_kill_target(&run_id) {
            handle.kill();
        }
    });
}

/// Runs one provider round with kill-handle tracking: the adapter process the
/// pool serves the round on is recorded in the registry for the cancel
/// fallback, and cleared again as soon as the round returns (a freed resident
/// may immediately serve other runs).
fn track_round_adapter<T>(
    registry: &Arc<ChatRunRegistry>,
    run_id: &str,
    round: impl FnOnce() -> T,
) -> T {
    let observer_registry = Arc::clone(registry);
    let observer_run_id = run_id.to_string();
    let result = with_adapter_use_observer(
        move |handle| observer_registry.note_adapter_use(&observer_run_id, handle),
        round,
    );
    registry.clear_adapter_use(run_id);
    result
}

/// Bridges the LLM gateway's streaming callbacks to durable database state and
/// the run event sink. Both consumers are COALESCED on the same cadence
/// (`CHAT_DELTA_FLUSH_BYTES` / `CHAT_DELTA_FLUSH_INTERVAL`): a fast model
/// otherwise produces one full sidecar→host→webview hop per token, which is
/// pure fan-out overhead — the UI animates the reveal client-side, so it only
/// needs the text in batches.
struct DbForwardingSink<'a> {
    database: &'a Database,
    run_id: &'a str,
    chat_id: &'a str,
    assistant_message_id: &'a str,
    sink: &'a mut dyn ChatRunEventSink,
    pending_delta: String,
    pending_event_delta: String,
    last_flush: Instant,
}

impl<'a> DbForwardingSink<'a> {
    fn new(
        database: &'a Database,
        sink: &'a mut dyn ChatRunEventSink,
        run_id: &'a str,
        chat_id: &'a str,
        assistant_message_id: &'a str,
    ) -> Self {
        Self {
            database,
            run_id,
            chat_id,
            assistant_message_id,
            sink,
            pending_delta: String::new(),
            pending_event_delta: String::new(),
            last_flush: Instant::now(),
        }
    }

    fn emit(&mut self, event: ChatRunEvent) {
        self.sink.emit(event);
    }

    fn flush(&mut self) {
        // UI first: the visible stream must never lag behind the DB write, and
        // a failed write must not swallow text the user should see.
        if !self.pending_event_delta.is_empty() {
            let delta = std::mem::take(&mut self.pending_event_delta);
            self.sink.emit(ChatRunEvent {
                run_id: self.run_id.to_string(),
                chat_id: self.chat_id.to_string(),
                message_id: self.assistant_message_id.to_string(),
                kind: ChatRunEventKind::Delta,
                delta: Some(delta),
                message: None,
                chat: None,
                transport: None,
                tool_call_id: None,
                removed_message_ids: Vec::new(),
                error: None,
            });
        }
        if self.pending_delta.is_empty() {
            return;
        }
        let delta = std::mem::take(&mut self.pending_delta);
        match self.database.append_chat_run_delta(
            self.run_id,
            self.chat_id,
            self.assistant_message_id,
            &delta,
        ) {
            Ok(()) => {
                self.last_flush = Instant::now();
            }
            Err(error) => {
                // The terminal complete/fail write persists the full message, so
                // a dropped intermediate flush self-heals — but never silently.
                eprintln!(
                    "chat run {}: failed to persist streamed delta ({} bytes): {error}",
                    self.run_id,
                    delta.len()
                );
            }
        }
    }
}

impl LlmChatCompletionEventSink for DbForwardingSink<'_> {
    fn transport_selected(&mut self, transport: LlmTransportKind) {
        if let Ok(event) = self.database.mark_chat_run_transport(
            self.run_id,
            self.chat_id,
            self.assistant_message_id,
            transport.as_str(),
        ) {
            self.sink.emit(event);
        }
    }

    fn delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.pending_delta.push_str(delta);
        self.pending_event_delta.push_str(delta);
        if self.pending_delta.len() >= CHAT_DELTA_FLUSH_BYTES
            || self.last_flush.elapsed() >= CHAT_DELTA_FLUSH_INTERVAL
        {
            self.flush();
        }
    }

    fn before_tool_call(&mut self, tool_call_id: &str) {
        self.flush();
        if self
            .database
            .record_chat_tool_call_part(
                self.run_id,
                self.chat_id,
                self.assistant_message_id,
                tool_call_id,
            )
            .is_ok()
        {
            self.sink.emit(ChatRunEvent {
                run_id: self.run_id.to_string(),
                chat_id: self.chat_id.to_string(),
                message_id: self.assistant_message_id.to_string(),
                kind: ChatRunEventKind::ToolCall,
                delta: None,
                message: None,
                chat: None,
                transport: None,
                tool_call_id: Some(tool_call_id.to_string()),
                removed_message_ids: Vec::new(),
                error: None,
            });
        }
    }
}

fn plugins_store_path(database: &Database) -> PathBuf {
    database
        .path()
        .parent()
        .map(|path| path.join("plugins"))
        .unwrap_or_else(|| PathBuf::from("plugins"))
}

/// Root of the app's shared credential vault (sibling of the database). Adapter
/// settings and secrets are stored here, keyed by provider — the same vault the
/// auth subsystem uses.
fn auth_store_path(database: &Database) -> PathBuf {
    database
        .path()
        .parent()
        .map(|path| path.join("auth"))
        .unwrap_or_else(|| PathBuf::from("auth"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::chat::{
        ChatMessage, ChatMessageRole, ChatMessageStatus, ChatRunEvent, ChatRunEventKind,
        ChatRunEventSink, SendChatMessageResult,
    };
    use crate::Database;

    #[derive(Default)]
    struct CapturingSink {
        events: Vec<ChatRunEvent>,
    }

    impl ChatRunEventSink for CapturingSink {
        fn emit(&mut self, event: ChatRunEvent) {
            self.events.push(event);
        }
    }

    impl CapturingSink {
        fn kinds(&self) -> Vec<ChatRunEventKind> {
            self.events.iter().map(|event| event.kind).collect()
        }

        fn error_text(&self) -> String {
            self.events
                .last()
                .and_then(|event| event.error.clone())
                .unwrap_or_default()
        }
    }

    fn temp_database_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("mothership_run_{name}_{stamp}.sqlite"))
    }

    /// A run handle over a freshly created (message-less) chat. Enough to drive
    /// the orchestration's early error paths without any network.
    fn run_handle(database: &Database) -> SendChatMessageResult {
        let project_path = database.path().with_file_name("run_test_project");
        fs::create_dir_all(&project_path).expect("create project dir");
        let snapshot = database
            .open_project(project_path.to_str().expect("project path"))
            .expect("open project");
        let project_id = snapshot.active_project_id.expect("active project id");
        let conversation = database
            .create_chat(&project_id, None)
            .expect("create chat");
        let chat = conversation.chat;
        let assistant_message = ChatMessage {
            id: "chat_message_assistant_test".to_string(),
            chat_id: chat.id.clone(),
            position: 1,
            role: ChatMessageRole::Assistant,
            content: String::new(),
            status: ChatMessageStatus::Sending,
            created_at: "0".to_string(),
            error: None,
            provider_id: None,
            model_id: None,
        };
        let user_message = ChatMessage {
            id: "chat_message_user_test".to_string(),
            chat_id: chat.id.clone(),
            position: 0,
            role: ChatMessageRole::User,
            content: "Hello there".to_string(),
            status: ChatMessageStatus::Complete,
            created_at: "0".to_string(),
            error: None,
            provider_id: None,
            model_id: None,
        };

        SendChatMessageResult {
            run_id: "chat_run_test".to_string(),
            chat,
            user_message,
            assistant_message,
            removed_message_ids: Vec::new(),
            context: Default::default(),
        }
    }

    fn test_summary(run_id: &str, provider_id: &str) -> ActiveRunSummary {
        ActiveRunSummary {
            run_id: run_id.to_string(),
            chat_id: "chat_1".to_string(),
            chat_title: "Test chat".to_string(),
            project_id: Some("project_1".to_string()),
            project_name: Some("Test project".to_string()),
            message_id: "message_1".to_string(),
            provider_id: provider_id.to_string(),
            model_id: "model_a".to_string(),
            started_at_ms: 1,
        }
    }

    #[test]
    fn cancel_fallback_targets_the_recorded_adapter_process() {
        let registry = ChatRunRegistry::new();
        let token = ChatCancellationToken::default();
        registry.register("run_1", test_summary("run_1", "provider_a"), token.clone());
        registry.note_adapter_use("run_1", AdapterKillHandle::ephemeral_for_tests(4242));

        // Not cancelled yet: nothing to kill.
        assert!(registry.cancelled_kill_target("run_1").is_none());

        assert_eq!(registry.cancel("run_1").as_deref(), Some("provider_a"));
        let target = registry
            .cancelled_kill_target("run_1")
            .expect("cancelled active run with a round in flight has a target");
        assert_eq!(target.pid(), 4242);

        // Round finished: the process may serve other runs — no longer a target.
        registry.clear_adapter_use("run_1");
        assert!(registry.cancelled_kill_target("run_1").is_none());

        // Finished runs are never targets.
        registry.note_adapter_use("run_1", AdapterKillHandle::ephemeral_for_tests(4243));
        registry.finish("run_1");
        assert!(registry.cancelled_kill_target("run_1").is_none());
    }

    #[test]
    fn snapshot_lists_runs_until_finished() {
        let registry = ChatRunRegistry::new();
        assert!(registry.snapshot().is_empty());

        registry.register(
            "run_1",
            test_summary("run_1", "provider_a"),
            ChatCancellationToken::default(),
        );
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].run_id, "run_1");
        assert_eq!(snapshot[0].chat_title, "Test chat");
        assert_eq!(snapshot[0].project_name.as_deref(), Some("Test project"));

        registry.finish("run_1");
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn run_fails_when_no_model_selected() {
        let database_path = temp_database_path("no_model_selected");
        let database = Database::open(&database_path).expect("open database");

        let selected = database.selected_llm_model().expect("selected model");
        assert!(selected.model_id.trim().is_empty());

        let mut run = run_handle(&database);
        run.removed_message_ids = vec!["chat_message_old_failed".to_string()];
        let mut sink = CapturingSink::default();
        ChatRunService::new(&database, Arc::new(ProviderRuntimeManager::default())).run(
            &run,
            Arc::new(ChatRunRegistry::new()),
            &mut sink,
        );

        assert_eq!(
            sink.kinds(),
            vec![ChatRunEventKind::Started, ChatRunEventKind::Failed]
        );
        assert_eq!(
            sink.events[0].removed_message_ids,
            vec!["chat_message_old_failed"]
        );
        assert!(sink.error_text().contains("no LLM model selected"));

        let _ = fs::remove_file(database_path);
    }
}
