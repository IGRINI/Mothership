//! Adapter-side SDK for Mothership provider plugins.
//!
//! A provider author implements [`ProviderAdapter`] (identity, settings schema,
//! auth scheme, models, chat, …) and calls [`run`] from `main`. The SDK owns
//! everything generic — the newline-delimited JSON-RPC stdio framing, request
//! dispatch, the `initialize` version handshake, the `StoreSecret` side channel,
//! and error mapping — so adapters stop hand-rolling that plumbing.
//!
//! Requests are processed one at a time except that `chat_cancel` is accepted
//! while a `chat_start` turn is in flight. The adapter still keeps `&mut self`
//! state — including a long-lived backend connection — across calls without
//! locking.
//!
//! Transport primitives (HTTP-JSON / HTTP-SSE / WebSocket + fallback + timeouts)
//! and OAuth helpers will live in this crate too; for now it provides the
//! runtime + trait so providers share the protocol loop. The wire types are
//! re-exported as [`protocol`].

pub use mothership_adapter_protocol as protocol;

pub mod http;
pub mod media;
pub mod oauth;
pub mod provider_metadata;
pub mod reasoning;
pub mod sse;
pub mod tools;
pub mod ws;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Notify};

/// How often the runtime nudges the adapter's [`ProviderAdapter::on_idle`] while
/// no request is in flight, so it can release idle resources (e.g. close a
/// WebSocket). The adapter decides the actual idle threshold.
const IDLE_TICK: Duration = Duration::from_secs(5);

use protocol::{
    AudioTranscriptionRequest, AudioTranscriptionResult, AuthKind, AuthStatus, ChatMessage,
    ImageGenerationRequest, ImageGenerationResult, Model, ModelManagement, Outbound, PromptBundle,
    ProviderService, ReasoningConfig, Request, RuntimeContext, SettingsField, ToolCallInvocation,
    ToolCallResponse, ToolCallResult, ToolDescriptor, PROTOCOL_VERSION,
};

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub reasoning: Option<ReasoningConfig>,
    pub fast_mode: bool,
    pub prompt: PromptBundle,
    pub runtime_context: RuntimeContext,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDescriptor>,
    pub state: Option<serde_json::Value>,
    pub tool_results: Vec<ToolCallResponse>,
    pub extra_messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Default)]
pub struct ChatRoundOutcome {
    pub state: Option<serde_json::Value>,
    pub tool_calls: Vec<ToolCallInvocation>,
}

/// What a provider plugin implements. The SDK runtime calls these in response to
/// host requests; all stdio framing/dispatch lives in [`run`]. Methods take
/// `&mut self` because requests are processed sequentially.
#[async_trait::async_trait]
pub trait ProviderAdapter: Send {
    /// Stable provider id (e.g. `"codex"`) and human label (e.g. `"Codex"`).
    fn identity(&self) -> (String, String);

    /// Settings fields the host should render and feed back via `set_settings`.
    fn settings_schema(&self) -> Vec<SettingsField> {
        Vec::new()
    }

    /// The adapter's auth scheme.
    fn auth_schema(&self) -> AuthKind {
        AuthKind::None
    }

    fn auth_status(&self) -> AuthStatus {
        match self.auth_schema() {
            AuthKind::None => AuthStatus::not_required(),
            AuthKind::ApiKey { .. } => AuthStatus::missing("API key is not configured"),
            AuthKind::OauthInternal | AuthKind::ExternalProcess => {
                AuthStatus::missing("provider is not authenticated")
            }
        }
    }

    /// Receive current settings (the host pushes them from the shared vault).
    async fn set_settings(&mut self, values: BTreeMap<String, String>) -> Result<()> {
        let _ = values;
        Ok(())
    }

    /// The models this provider offers and how its list is managed. `ctx` lets
    /// the adapter persist a credential it refreshed while fetching the list.
    async fn models(&mut self, ctx: &Context) -> Result<(ModelManagement, Vec<Model>)> {
        let _ = ctx;
        Ok((ModelManagement::Fixed, Vec::new()))
    }

    /// Non-chat provider services (image generation, STT, speech, etc.) this
    /// adapter can provide independently of the active chat model.
    async fn services(&mut self, ctx: &Context) -> Result<Vec<ProviderService>> {
        let _ = ctx;
        Ok(Vec::new())
    }

    async fn generate_image(
        &mut self,
        request: ImageGenerationRequest,
        ctx: &Context,
    ) -> Result<ImageGenerationResult> {
        let _ = request;
        let _ = ctx;
        anyhow::bail!("image generation is not supported by this adapter")
    }

    async fn transcribe_audio(
        &mut self,
        request: AudioTranscriptionRequest,
        ctx: &Context,
    ) -> Result<AudioTranscriptionResult> {
        let _ = request;
        let _ = ctx;
        anyhow::bail!("audio transcription is not supported by this adapter")
    }

    /// Run the provider's own auth flow (e.g. browser OAuth). Persist any minted
    /// credential via [`Context::store_secret`].
    async fn authenticate(&mut self, ctx: &Context) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Run one chat turn, emitting streamed text via `sink`. `ctx` lets the
    /// adapter persist a refreshed credential mid-turn via `StoreSecret`.
    async fn chat(
        &mut self,
        request: ChatRequest,
        ctx: &Context,
        sink: &mut ChatSink,
    ) -> Result<ChatRoundOutcome>;

    /// Revoke / clean up the current auth before the host forgets it.
    async fn logout(&mut self, ctx: &Context) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Called periodically by the runtime while no request is in flight. Use it
    /// to release idle resources — e.g. close a WebSocket that's been quiet — so
    /// the process stays warm but holds nothing open. Default: no-op.
    async fn on_idle(&mut self, ctx: &Context) {
        let _ = ctx;
    }
}

/// Cooperative cancellation signal for an in-flight chat turn.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

#[derive(Debug)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    fn new() -> Self {
        Self {
            inner: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    /// Returns true once the host has requested cancellation for this turn.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves when the host requests cancellation for this turn.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Lets an adapter persist secrets (e.g. a freshly minted/refreshed OAuth token)
/// into the host's shared vault at any time via the `StoreSecret` side channel.
#[derive(Clone)]
pub struct Context {
    outbox: mpsc::UnboundedSender<Outbound>,
    chat_request_id: Option<u64>,
    cancellation: Option<CancellationToken>,
    tool_responses: Arc<Mutex<HashMap<String, oneshot::Sender<ToolCallResult>>>>,
}

impl Context {
    /// Persist these values in the shared credential store (merged by the host).
    pub fn store_secret(&self, values: BTreeMap<String, String>) {
        let _ = self.outbox.send(Outbound::StoreSecret { values });
    }

    /// Ask Mothership Core to execute one of the tools supplied in
    /// [`ChatRequest::tools`] and wait for the result. This is the generic
    /// mid-turn bridge used by self-managed adapters: the adapter can expose the
    /// tools through its own upstream mechanism (MCP, CLI callbacks, etc.) while
    /// Core still owns supervision, permissions, history, and cancellation.
    pub async fn request_tool(
        &self,
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> ToolCallResult {
        let Some(id) = self.chat_request_id else {
            return failed_tool_result("tool requests are only available during chat_start");
        };
        if self.is_cancelled() {
            return failed_tool_result("chat turn was cancelled before tool execution");
        }

        let tool_call_id = tool_call_id.into();
        let name = name.into();
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.tool_responses.lock().unwrap();
            if pending.contains_key(&tool_call_id) {
                return failed_tool_result(format!("duplicate tool call id `{tool_call_id}`"));
            }
            pending.insert(tool_call_id.clone(), sender);
        }

        if self
            .outbox
            .send(Outbound::ToolRequest {
                id,
                tool_call_id: tool_call_id.clone(),
                name,
                arguments,
            })
            .is_err()
        {
            self.tool_responses.lock().unwrap().remove(&tool_call_id);
            return failed_tool_result("host disconnected before tool request was sent");
        }

        tokio::select! {
            result = receiver => match result {
                Ok(result) => result,
                Err(_) => failed_tool_result("host dropped the tool response channel"),
            },
            _ = self.cancelled() => {
                self.tool_responses.lock().unwrap().remove(&tool_call_id);
                failed_tool_result("chat turn was cancelled while waiting for tool execution")
            }
        }
    }

    /// Returns true when the current chat turn has been cancelled. Non-chat
    /// request contexts are never cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .map(CancellationToken::is_cancelled)
            .unwrap_or(false)
    }

    /// Resolves when the current chat turn is cancelled. For non-chat request
    /// contexts this waits forever.
    pub async fn cancelled(&self) {
        match &self.cancellation {
            Some(cancellation) => cancellation.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    }

    /// Returns the underlying cancellation token for advanced integrations that
    /// need to share cancellation with helper tasks.
    pub fn cancellation_token(&self) -> Option<CancellationToken> {
        self.cancellation.clone()
    }

    fn with_chat(&self, chat_request_id: u64, cancellation: CancellationToken) -> Self {
        Self {
            outbox: self.outbox.clone(),
            chat_request_id: Some(chat_request_id),
            cancellation: Some(cancellation),
            tool_responses: Arc::clone(&self.tool_responses),
        }
    }

    fn resolve_tool_result(&self, tool_call_id: &str, result: ToolCallResult) -> bool {
        self.tool_responses
            .lock()
            .unwrap()
            .remove(tool_call_id)
            .map(|sender| sender.send(result).is_ok())
            .unwrap_or(false)
    }
}

/// Emits streamed chat deltas for the in-flight `chat_start` request.
#[derive(Clone)]
pub struct ChatSink {
    id: u64,
    outbox: mpsc::UnboundedSender<Outbound>,
    cancellation: CancellationToken,
}

impl ChatSink {
    /// Push a chunk of assistant output to the host.
    pub fn delta(&self, text: impl Into<String>) {
        if self.is_cancelled() {
            return;
        }
        let _ = self.outbox.send(Outbound::Delta {
            id: self.id,
            text: text.into(),
        });
    }

    /// Returns true once the host has requested cancellation for this turn.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// Resolves when the host requests cancellation for this turn.
    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    /// Returns a cloneable cancellation token for helper code that cannot hold a
    /// `ChatSink` borrow.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

/// Runs the adapter against the host over stdio until stdin closes. Call this
/// from an adapter's `#[tokio::main] async fn main`.
pub async fn run<A: ProviderAdapter>(mut adapter: A) -> Result<()> {
    // One writer task owns stdout; everyone emits frames through `outbox` so
    // streamed deltas and StoreSecret pushes never interleave a half-written line.
    let (outbox, mut frames) = mpsc::unbounded_channel::<Outbound>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(frame) = frames.recv().await {
            match serde_json::to_string(&frame) {
                Ok(mut line) => {
                    line.push('\n');
                    if stdout.write_all(line.as_bytes()).await.is_err()
                        || stdout.flush().await.is_err()
                    {
                        break;
                    }
                }
                Err(error) => eprintln!("adapter-sdk: failed to encode frame: {error}"),
            }
        }
    });

    let ctx = Context {
        outbox: outbox.clone(),
        chat_request_id: None,
        cancellation: None,
        tool_responses: Arc::new(Mutex::new(HashMap::new())),
    };

    // Read stdin on a dedicated task so the main loop can `select!` an idle timer
    // against requests without ever cancelling a half-read line (`next_line` is
    // not cancel-safe; channel `recv` is).
    let (requests_tx, mut requests) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if requests_tx.send(line).is_err() {
                break;
            }
        }
        // Dropping requests_tx on stdin EOF closes the channel below.
    });

    let mut pending = VecDeque::new();

    loop {
        if let Some(request) = pending.pop_front() {
            match request {
                Request::ChatStart {
                    id,
                    model,
                    reasoning,
                    fast_mode,
                    prompt,
                    runtime_context,
                    messages,
                    tools,
                    state,
                    tool_results,
                    extra_messages,
                } => {
                    if !run_chat_turn(
                        &mut adapter,
                        &ctx,
                        &outbox,
                        &mut requests,
                        &mut pending,
                        id,
                        ChatRequest {
                            model,
                            reasoning,
                            fast_mode,
                            prompt,
                            runtime_context,
                            messages,
                            tools,
                            state,
                            tool_results,
                            extra_messages,
                        },
                    )
                    .await
                    {
                        break;
                    }
                }
                Request::ChatCancel { id } => send(&outbox, Outbound::Ack { id }),
                request => dispatch(&mut adapter, &ctx, &outbox, request).await,
            }
            continue;
        }

        tokio::select! {
            line = requests.recv() => {
                let Some(line) = line else { break }; // stdin closed
                let Some(request) = parse_request(&line) else {
                    continue;
                };
                match request {
                    Request::ChatStart {
                        id,
                        model,
                        reasoning,
                        fast_mode,
                        prompt,
                        runtime_context,
                        messages,
                        tools,
                        state,
                        tool_results,
                        extra_messages,
                    } => {
                        if !run_chat_turn(
                            &mut adapter,
                            &ctx,
                            &outbox,
                            &mut requests,
                            &mut pending,
                            id,
                            ChatRequest {
                                model,
                                reasoning,
                                fast_mode,
                                prompt,
                                runtime_context,
                                messages,
                                tools,
                                state,
                                tool_results,
                                extra_messages,
                            },
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Request::ChatCancel { id } => send(&outbox, Outbound::Ack { id }),
                    request => dispatch(&mut adapter, &ctx, &outbox, request).await,
                }
            }
            _ = tokio::time::sleep(IDLE_TICK) => {
                adapter.on_idle(&ctx).await;
            }
        }
    }

    drop(outbox);
    drop(ctx);
    let _ = writer.await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_chat_turn<A: ProviderAdapter>(
    adapter: &mut A,
    ctx: &Context,
    outbox: &mpsc::UnboundedSender<Outbound>,
    requests: &mut mpsc::UnboundedReceiver<String>,
    pending: &mut VecDeque<Request>,
    id: u64,
    request: ChatRequest,
) -> bool {
    let cancellation = CancellationToken::new();
    let chat_ctx = ctx.with_chat(id, cancellation.clone());
    let mut sink = ChatSink {
        id,
        outbox: outbox.clone(),
        cancellation: cancellation.clone(),
    };
    let chat = adapter.chat(request, &chat_ctx, &mut sink);
    tokio::pin!(chat);

    loop {
        tokio::select! {
            result = &mut chat => {
                match result {
                    Ok(outcome) => send(
                        outbox,
                        Outbound::ChatRoundComplete {
                            id,
                            state: outcome.state,
                            tool_calls: outcome.tool_calls,
                        },
                    ),
                    Err(error) => send_error(outbox, id, error),
                }
                return true;
            }
            line = requests.recv() => {
                let Some(line) = line else {
                    cancellation.cancel();
                    return false;
                };
                let Some(request) = parse_request(&line) else {
                    continue;
                };
                match request {
                    Request::ChatCancel { id: cancel_id } => {
                        cancellation.cancel();
                        if cancel_id != id {
                            send(outbox, Outbound::Ack { id: cancel_id });
                        }
                    }
                    Request::ToolResult {
                        id: result_id,
                        tool_call_id,
                        result,
                    } if result_id == id => {
                        if !chat_ctx.resolve_tool_result(&tool_call_id, result) {
                            eprintln!(
                                "adapter-sdk: received tool_result for unknown tool_call_id `{tool_call_id}`"
                            );
                        }
                    }
                    request => pending.push_back(request),
                }
            }
        }
    }
}

fn failed_tool_result(message: impl Into<String>) -> ToolCallResult {
    ToolCallResult {
        ok: false,
        content: message.into(),
    }
}

fn parse_request(line: &str) -> Option<Request> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<Request>(trimmed) {
        Ok(request) => Some(request),
        Err(error) => {
            eprintln!("adapter-sdk: ignoring unparseable request: {error}");
            None
        }
    }
}

async fn dispatch<A: ProviderAdapter>(
    adapter: &mut A,
    ctx: &Context,
    outbox: &mpsc::UnboundedSender<Outbound>,
    request: Request,
) {
    match request {
        Request::Initialize { id, .. } => send(
            outbox,
            Outbound::Initialized {
                id,
                protocol_version: PROTOCOL_VERSION,
            },
        ),
        Request::GetIdentity { id } => {
            let (provider_id, provider_label) = adapter.identity();
            send(
                outbox,
                Outbound::Identity {
                    id,
                    provider_id,
                    provider_label,
                },
            );
        }
        Request::GetSettingsSchema { id } => send(
            outbox,
            Outbound::SettingsSchema {
                id,
                fields: adapter.settings_schema(),
            },
        ),
        Request::GetAuthSchema { id } => send(
            outbox,
            Outbound::AuthSchema {
                id,
                auth: adapter.auth_schema(),
            },
        ),
        Request::GetAuthStatus { id } => send(
            outbox,
            Outbound::AuthStatus {
                id,
                status: adapter.auth_status(),
            },
        ),
        Request::SetSettings { id, values } => match adapter.set_settings(values).await {
            Ok(()) => send(outbox, Outbound::Ack { id }),
            Err(error) => send_error(outbox, id, error),
        },
        Request::GetModels { id } => match adapter.models(ctx).await {
            Ok((management, models)) => send(
                outbox,
                Outbound::Models {
                    id,
                    management,
                    models,
                },
            ),
            Err(error) => send_error(outbox, id, error),
        },
        Request::GetServices { id } => match adapter.services(ctx).await {
            Ok(services) => send(outbox, Outbound::Services { id, services }),
            Err(error) => send_error(outbox, id, error),
        },
        Request::Authenticate { id } => match adapter.authenticate(ctx).await {
            Ok(()) => send(outbox, Outbound::Ack { id }),
            Err(error) => send_error(outbox, id, error),
        },
        Request::ChatStart { id, .. } => send_error(
            outbox,
            id,
            anyhow::anyhow!("internal adapter runtime error: chat_start bypassed cancellable path"),
        ),
        Request::ChatCancel { id } => send(outbox, Outbound::Ack { id }),
        Request::ToolResult { id, .. } => send_error(
            outbox,
            id,
            anyhow::anyhow!("tool_result arrived without an active chat_start"),
        ),
        Request::GenerateImage { id, request } => {
            match adapter.generate_image(request, ctx).await {
                Ok(result) => send(outbox, Outbound::ImageGenerated { id, result }),
                Err(error) => send_error(outbox, id, error),
            }
        }
        Request::TranscribeAudio { id, request } => {
            match adapter.transcribe_audio(request, ctx).await {
                Ok(result) => send(outbox, Outbound::AudioTranscribed { id, result }),
                Err(error) => send_error(outbox, id, error),
            }
        }
        Request::Logout { id } => match adapter.logout(ctx).await {
            Ok(()) => send(outbox, Outbound::Ack { id }),
            Err(error) => send_error(outbox, id, error),
        },
    }
}

fn send(outbox: &mpsc::UnboundedSender<Outbound>, frame: Outbound) {
    let _ = outbox.send(frame);
}

fn send_error(outbox: &mpsc::UnboundedSender<Outbound>, id: u64, error: anyhow::Error) {
    let _ = outbox.send(Outbound::Error {
        id,
        message: format!("{error:#}"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::oneshot;

    struct CancelAwareAdapter {
        started: Option<oneshot::Sender<()>>,
        saw_cancel: bool,
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for CancelAwareAdapter {
        fn identity(&self) -> (String, String) {
            ("test".to_string(), "Test".to_string())
        }

        async fn chat(
            &mut self,
            _request: ChatRequest,
            ctx: &Context,
            sink: &mut ChatSink,
        ) -> Result<ChatRoundOutcome> {
            sink.delta("before");
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            ctx.cancelled().await;
            self.saw_cancel = ctx.is_cancelled() && sink.is_cancelled();
            sink.delta("after");
            Ok(ChatRoundOutcome::default())
        }
    }

    #[tokio::test]
    async fn chat_cancel_is_observable_during_active_turn() {
        let (outbox, mut frames) = mpsc::unbounded_channel();
        let ctx = Context {
            outbox: outbox.clone(),
            chat_request_id: None,
            cancellation: None,
            tool_responses: Arc::new(Mutex::new(HashMap::new())),
        };
        let (requests_tx, mut requests) = mpsc::unbounded_channel();
        let mut pending = VecDeque::new();
        let (started_tx, started_rx) = oneshot::channel();
        let mut adapter = CancelAwareAdapter {
            started: Some(started_tx),
            saw_cancel: false,
        };

        let run = run_chat_turn(
            &mut adapter,
            &ctx,
            &outbox,
            &mut requests,
            &mut pending,
            42,
            ChatRequest {
                model: "test-model".to_string(),
                reasoning: None,
                fast_mode: false,
                prompt: PromptBundle::default(),
                runtime_context: RuntimeContext::default(),
                messages: Vec::new(),
                tools: Vec::new(),
                state: None,
                tool_results: Vec::new(),
                extra_messages: Vec::new(),
            },
        );
        let cancel = async {
            started_rx.await.expect("chat starts");
            let cancel = serde_json::to_string(&Request::ChatCancel { id: 42 }).unwrap();
            requests_tx.send(cancel).expect("send cancel");
        };

        let (stdin_open, _) = tokio::join!(run, cancel);

        assert!(stdin_open);
        assert!(adapter.saw_cancel);
        assert!(pending.is_empty());
        let mut emitted = Vec::new();
        while let Ok(frame) = frames.try_recv() {
            emitted.push(frame);
        }
        assert!(matches!(
            emitted.as_slice(),
            [
                Outbound::Delta { id: 42, text },
                Outbound::ChatRoundComplete {
                    id: 42,
                    state,
                    tool_calls
                },
            ] if text == "before" && state.is_none() && tool_calls.is_empty()
        ));
    }

    #[test]
    fn chat_sink_suppresses_delta_after_cancel() {
        let (outbox, mut frames) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let sink = ChatSink {
            id: 7,
            outbox,
            cancellation: cancellation.clone(),
        };

        sink.delta("before");
        cancellation.cancel();
        sink.delta("after");

        assert!(matches!(
            frames.try_recv(),
            Ok(Outbound::Delta { id: 7, text }) if text == "before"
        ));
        assert!(frames.try_recv().is_err());
    }

    struct ToolRequestingAdapter;

    #[async_trait::async_trait]
    impl ProviderAdapter for ToolRequestingAdapter {
        fn identity(&self) -> (String, String) {
            ("test".to_string(), "Test".to_string())
        }

        async fn chat(
            &mut self,
            _request: ChatRequest,
            ctx: &Context,
            sink: &mut ChatSink,
        ) -> Result<ChatRoundOutcome> {
            let result = ctx
                .request_tool(
                    "tool-1",
                    "run_command",
                    json!({
                        "program": "echo",
                        "args": ["hello"],
                    }),
                )
                .await;
            sink.delta(format!("tool result: {}", result.content));
            Ok(ChatRoundOutcome::default())
        }
    }

    #[tokio::test]
    async fn chat_can_wait_for_mid_turn_tool_result() {
        let (outbox, mut frames) = mpsc::unbounded_channel();
        let ctx = Context {
            outbox: outbox.clone(),
            chat_request_id: None,
            cancellation: None,
            tool_responses: Arc::new(Mutex::new(HashMap::new())),
        };
        let (requests_tx, mut requests) = mpsc::unbounded_channel();
        let mut pending = VecDeque::new();
        let mut adapter = ToolRequestingAdapter;

        let run = run_chat_turn(
            &mut adapter,
            &ctx,
            &outbox,
            &mut requests,
            &mut pending,
            9,
            ChatRequest {
                model: "test-model".to_string(),
                reasoning: None,
                fast_mode: false,
                prompt: PromptBundle::default(),
                runtime_context: RuntimeContext::default(),
                messages: Vec::new(),
                tools: Vec::new(),
                state: None,
                tool_results: Vec::new(),
                extra_messages: Vec::new(),
            },
        );
        let host = async {
            let frame = frames.recv().await.expect("tool request frame");
            let Outbound::ToolRequest {
                id,
                tool_call_id,
                name,
                arguments,
            } = frame
            else {
                panic!("expected tool request frame");
            };
            assert_eq!(id, 9);
            assert_eq!(tool_call_id, "tool-1");
            assert_eq!(name, "run_command");
            assert_eq!(arguments["program"], "echo");
            let result = Request::ToolResult {
                id,
                tool_call_id,
                result: ToolCallResult {
                    ok: true,
                    content: "done".to_string(),
                },
            };
            requests_tx
                .send(serde_json::to_string(&result).expect("encode tool result"))
                .expect("send tool result");
        };

        let (stdin_open, _) = tokio::join!(run, host);

        assert!(stdin_open);
        let mut emitted = Vec::new();
        while let Ok(frame) = frames.try_recv() {
            emitted.push(frame);
        }
        assert!(matches!(
            emitted.as_slice(),
            [
                Outbound::Delta { id: 9, text },
                Outbound::ChatRoundComplete {
                    id: 9,
                    state,
                    tool_calls
                },
            ] if text == "tool result: done" && state.is_none() && tool_calls.is_empty()
        ));
    }

    #[test]
    fn chat_round_outcome_defaults_to_no_continuation_or_tools() {
        let outcome = ChatRoundOutcome::default();

        assert!(outcome.state.is_none());
        assert!(outcome.tool_calls.is_empty());
    }

    struct DefaultServiceAdapter;

    #[async_trait::async_trait]
    impl ProviderAdapter for DefaultServiceAdapter {
        fn identity(&self) -> (String, String) {
            ("test".to_string(), "Test".to_string())
        }

        async fn chat(
            &mut self,
            _request: ChatRequest,
            _ctx: &Context,
            _sink: &mut ChatSink,
        ) -> Result<ChatRoundOutcome> {
            Ok(ChatRoundOutcome::default())
        }
    }

    #[tokio::test]
    async fn default_service_methods_are_backward_compatible_unsupported() {
        let (outbox, _frames) = mpsc::unbounded_channel();
        let ctx = Context {
            outbox,
            chat_request_id: None,
            cancellation: None,
            tool_responses: Arc::new(Mutex::new(HashMap::new())),
        };
        let mut adapter = DefaultServiceAdapter;

        assert!(adapter.services(&ctx).await.unwrap().is_empty());

        let image_error = adapter
            .generate_image(
                ImageGenerationRequest {
                    model: "image-model".to_string(),
                    prompt: "test prompt".to_string(),
                    options: serde_json::Value::Null,
                },
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(image_error.to_string().contains("not supported"));

        let audio_error = adapter
            .transcribe_audio(
                AudioTranscriptionRequest {
                    model: "stt-model".to_string(),
                    input_path: "input.wav".to_string(),
                    options: serde_json::Value::Null,
                },
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(audio_error.to_string().contains("not supported"));
    }
}
