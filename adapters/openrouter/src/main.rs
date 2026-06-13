//! OpenRouter provider adapter.
//!
//! Provider-specific logic only: settings, user-defined model list, model
//! metadata, and the OpenAI-compatible streaming HTTP call. The shared adapter
//! SDK owns stdio framing, request dispatch, and cooperative cancellation.

mod adapter;
mod chat;
mod models;
mod services;
mod settings;

use adapter::OpenRouterAdapter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    mothership_adapter_sdk::run(OpenRouterAdapter::new()?).await
}
