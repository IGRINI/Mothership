//! Claude Agent SDK adapter.
//!
//! This adapter is intentionally self-managed: it launches the Claude Agent SDK
//! bundled Claude Code binary in headless stream-json mode. Mothership Core
//! owns process lifecycle, auth storage, chat persistence, and UI events; the
//! upstream Claude runtime owns its internal agent loop, tools, compaction, and
//! subagents.

mod adapter;
mod auth;
mod bridge;
mod builtin_models;
mod cli;
mod mcp;
mod model_catalog;
mod models;
mod settings;

use adapter::ClaudeAgentAdapter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("mcp-bridge") {
        return mcp::run_from_args(&args[1..]).await;
    }

    // Old adapter versions left per-round temp config dirs (with plaintext
    // credentials) behind whenever the host hard-killed the process; clean
    // those up before serving requests.
    auth::sweep_legacy_temp_config_dirs();

    mothership_adapter_sdk::run(ClaudeAgentAdapter::default()).await
}
