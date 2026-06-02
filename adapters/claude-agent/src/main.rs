//! Claude Agent SDK adapter.
//!
//! This adapter is intentionally self-managed: it launches the Claude Agent SDK
//! bundled Claude Code binary in headless stream-json mode. Mothership Core
//! owns process lifecycle, auth storage, chat persistence, and UI events; the
//! upstream Claude runtime owns its internal agent loop, tools, compaction, and
//! subagents.

mod adapter;
mod bridge;
mod cli;
mod mcp;
mod models;
mod settings;

use adapter::ClaudeAgentAdapter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("mcp-bridge") {
        return mcp::run_from_args(&args[1..]).await;
    }

    mothership_adapter_sdk::run(ClaudeAgentAdapter::default()).await
}
