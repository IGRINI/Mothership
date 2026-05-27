//! Adapter-side SDK for Mothership provider plugins.
//!
//! A provider author implements [`ProviderAdapter`] (identity, settings schema,
//! auth scheme, models, chat, …) and calls [`run`] from `main`. The SDK owns
//! everything generic — the newline-delimited JSON-RPC stdio framing, request
//! dispatch, the `initialize` version handshake, the `StoreSecret` side channel,
//! and error mapping — so adapters stop hand-rolling that plumbing.
//!
//! Requests are processed one at a time (the host serializes per provider via
//! its resident-adapter pool), so an adapter keeps `&mut self` state — including
//! a long-lived backend connection — across calls without locking.
//!
//! Transport primitives (HTTP-JSON / HTTP-SSE / WebSocket + fallback + timeouts)
//! and OAuth helpers will live in this crate too; for now it provides the
//! runtime + trait so providers share the protocol loop. The wire types are
//! re-exported as [`protocol`].

pub use mothership_adapter_protocol as protocol;

use std::collections::BTreeMap;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use protocol::{
    AuthKind, ChatMessage, Model, ModelManagement, Outbound, Request, SettingsField,
    PROTOCOL_VERSION,
};

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

    /// Receive current settings (the host pushes them from the shared vault).
    async fn set_settings(&mut self, values: BTreeMap<String, String>) -> Result<()> {
        let _ = values;
        Ok(())
    }

    /// The models this provider offers and how its list is managed.
    async fn models(&mut self) -> Result<(ModelManagement, Vec<Model>)> {
        Ok((ModelManagement::Fixed, Vec::new()))
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
        model: &str,
        messages: Vec<ChatMessage>,
        ctx: &Context,
        sink: &mut ChatSink,
    ) -> Result<()>;

    /// Revoke / clean up the current auth before the host forgets it.
    async fn logout(&mut self, ctx: &Context) -> Result<()> {
        let _ = ctx;
        Ok(())
    }
}

/// Lets an adapter persist secrets (e.g. a freshly minted/refreshed OAuth token)
/// into the host's shared vault at any time via the `StoreSecret` side channel.
#[derive(Clone)]
pub struct Context {
    outbox: mpsc::UnboundedSender<Outbound>,
}

impl Context {
    /// Persist these values in the shared credential store (merged by the host).
    pub fn store_secret(&self, values: BTreeMap<String, String>) {
        let _ = self.outbox.send(Outbound::StoreSecret { values });
    }
}

/// Emits streamed chat deltas for the in-flight `chat_start` request.
pub struct ChatSink {
    id: u64,
    outbox: mpsc::UnboundedSender<Outbound>,
}

impl ChatSink {
    /// Push a chunk of assistant output to the host.
    pub fn delta(&self, text: impl Into<String>) {
        let _ = self.outbox.send(Outbound::Delta {
            id: self.id,
            text: text.into(),
        });
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
    };

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(trimmed) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("adapter-sdk: ignoring unparseable request: {error}");
                continue;
            }
        };
        dispatch(&mut adapter, &ctx, &outbox, request).await;
    }

    drop(outbox);
    drop(ctx);
    let _ = writer.await;
    Ok(())
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
        Request::SetSettings { id, values } => match adapter.set_settings(values).await {
            Ok(()) => send(outbox, Outbound::Ack { id }),
            Err(error) => send_error(outbox, id, error),
        },
        Request::GetModels { id } => match adapter.models().await {
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
        Request::Authenticate { id } => match adapter.authenticate(ctx).await {
            Ok(()) => send(outbox, Outbound::Ack { id }),
            Err(error) => send_error(outbox, id, error),
        },
        Request::ChatStart { id, model, messages } => {
            let mut sink = ChatSink {
                id,
                outbox: outbox.clone(),
            };
            match adapter.chat(&model, messages, ctx, &mut sink).await {
                Ok(()) => send(outbox, Outbound::Done { id }),
                Err(error) => send_error(outbox, id, error),
            }
        }
        // Sequential processing means a cancel can't arrive mid-turn yet; ack the
        // turn as finished. Mid-stream cancellation lands with the host stop button.
        Request::ChatCancel { id } => send(outbox, Outbound::Done { id }),
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
