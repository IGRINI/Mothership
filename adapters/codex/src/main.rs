//! Codex provider adapter — built on the shared adapter SDK.
//!
//! Provider-specific logic only: Codex OAuth endpoints/request shapes, the
//! model catalog, and wiring the OpenAI Responses transport. Everything generic
//! comes from `mothership-adapter-sdk` and `mothership-openai-responses`.

mod adapter;
mod auth;
mod chat;
mod models;
mod services;

use adapter::CodexAdapter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    mothership_adapter_sdk::run(CodexAdapter::new()?).await
}
