//! # process-sandbox
//!
//! The cross-platform process-isolation foundation for Mothership's tool
//! execution: a neutral [`ProcessSandbox`] *port* plus per-OS *adapters* that
//! spawn a child process, let you drain its output in parallel, and **reliably
//! kill its whole process tree**.
//!
//! This crate is an **adapter layer**, not Core. It MAY depend on `tokio` and
//! platform crates. Core (`mothership-core`) must stay completely unaware of it —
//! it depends on the *port*, never on this crate. See the design docs:
//!
//! - `docs/runtime/PROCESS_SANDBOX.md` — the kill-tree asymmetry across OSes and
//!   the `process-wrap` Job Object approach.
//! - `docs/runtime/OUTPUT_AND_IPC.md` — the parallel stdout/stderr drain invariant.
//! - `docs/runtime/CROSS_PLATFORM_PATTERN.md` — ports & adapters, the leak test.
//!
//! ## Two axes of variation (don't conflate them)
//!
//! - **Platform** (Windows / Linux / macOS) → chosen at **compile time** via
//!   `#[cfg]`. That is [`platform_sandbox`], the only place the OS is selected.
//! - **Capability / mock** → chosen at **runtime** via `dyn`. That is
//!   [`MockSandbox`] vs. a real adapter, swapped by whoever constructs the
//!   `Arc<dyn ProcessSandbox>`.
//!
//! ## Kill-tree guarantee by platform
//!
//! ```text
//! Windows : bulletproof        — Job Object (implemented here)
//! Linux   : can-be-bulletproof — cgroup v2 / process group   (T3.5, later)
//! macOS   : best-effort only   — process group/session + reaper (T3.5, later)
//! ```

mod output;
mod port;

pub mod mock;

#[cfg(windows)]
pub mod windows;

#[cfg(not(windows))]
pub mod unix;

pub use mock::{MockResponse, MockSandbox};
pub use output::{drain, drain_parallel, BoundedCapture, OutputPolicy, StreamKind};
pub use port::{ProcessSandbox, SpawnedProcess, ToolExit, ToolSpec};

use std::sync::Arc;

/// Construct the [`ProcessSandbox`] for the current OS.
///
/// This is the **composition root** for the platform axis — the single place that
/// selects the adapter via `#[cfg]`. Callers (the supervisor, tests' production
/// path) receive an `Arc<dyn ProcessSandbox>` and know nothing about Job Objects,
/// `killpg`, or `kqueue`.
///
/// On non-Windows targets this currently returns a placeholder whose `spawn`
/// errors (the real Unix adapter + macOS reaper are T3.5). It still *compiles*
/// everywhere, which is the point.
#[must_use]
pub fn platform_sandbox() -> Arc<dyn ProcessSandbox> {
    #[cfg(windows)]
    {
        Arc::new(windows::JobObjectSandbox::new())
    }
    #[cfg(not(windows))]
    {
        Arc::new(unix::UnimplementedSandbox::new())
    }
}
