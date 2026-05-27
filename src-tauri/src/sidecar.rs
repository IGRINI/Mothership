//! Host-side client + supervisor for the Core sidecar process.
//!
//! The desktop host is a thin client: it owns no database, adapters, or vault.
//! It spawns the long-running `mothership-sidecar`, performs the
//! Hello/Initialize/Ready handshake, and from then on serializes
//! [`CoreRequest`]s to it and forwards its [`CoreEvent`]s to the webview.
//!
//! Concurrency model (the multiplexed-stdio shape the design calls for):
//! - exactly one reader (the sidecar's stdout event stream) routes each frame:
//!   a `Response`/`Error` completes the matching pending request; an `Event`
//!   is emitted to the webview;
//! - writes go through the child's stdin under a mutex;
//! - every request carries a transport id, correlated only to its single
//!   terminal reply — streams are correlated by their own domain ids;
//! - if the sidecar dies, every pending request fails deterministically with
//!   [`CoreError::sidecar_lost`] and the supervisor restarts it with backoff,
//!   giving up (and reporting unhealthy) after a few rapid failures.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mothership_core::ipc::{
    ClientFrame, CoreError, CoreEvent, CoreRequest, CoreResponse, ServerFrame, PROTOCOL_VERSION,
};
use tauri::{AppHandle, Emitter};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tokio::sync::{oneshot, watch};

use crate::job::JobHandle;

const SIDECAR_NAME: &str = "mothership-sidecar";
/// How many rapid (never-readied) crashes before we stop trying and report
/// unhealthy. A connection that successfully readies resets the budget.
const MAX_RESTART_ATTEMPTS: u32 = 3;
/// How long a command waits for the sidecar to become ready before failing,
/// rather than hanging forever when the sidecar is unhealthy.
const READY_TIMEOUT: Duration = Duration::from_secs(15);

type ResponseResult = Result<CoreResponse, CoreError>;
type Pending = Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>;

/// State shared between the supervisor task and the request callers.
struct Shared {
    pending: Pending,
    /// The live sidecar's stdin handle; `None` while down or restarting.
    child: Mutex<Option<CommandChild>>,
    next_id: AtomicU64,
    ready_tx: watch::Sender<bool>,
    /// Windows kill-on-close job the sidecar is assigned to (no-op elsewhere).
    job: Option<JobHandle>,
}

/// A cheap, cloneable client handle stored in app state. Cloning shares the same
/// sidecar connection (commands clone it out of `State` so the request future
/// stays `Send`).
#[derive(Clone)]
pub struct Sidecar {
    shared: Arc<Shared>,
    ready_rx: watch::Receiver<bool>,
}

impl Sidecar {
    /// Spawns the sidecar and its supervisor task. Returns immediately; the
    /// handshake completes asynchronously and requests wait for readiness.
    pub fn start(app: &AppHandle, db_path: PathBuf) -> Self {
        let (ready_tx, ready_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            child: Mutex::new(None),
            next_id: AtomicU64::new(1),
            ready_tx,
            job: JobHandle::create(),
        });

        tauri::async_runtime::spawn(supervise(app.clone(), Arc::clone(&shared), db_path));

        Self { shared, ready_rx }
    }

    /// Sends a request and awaits its single terminal reply. Errors (including a
    /// sidecar crash mid-flight) come back as a string for the command layer.
    pub async fn request(&self, request: CoreRequest) -> Result<CoreResponse, String> {
        self.wait_ready().await?;

        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().unwrap().insert(id, tx);

        if let Err(error) = write_frame(&self.shared, &ClientFrame::Request { id, request }) {
            self.shared.pending.lock().unwrap().remove(&id);
            return Err(error);
        }

        match rx.await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(core_error)) => Err(core_error.message),
            // Sender dropped without a reply: the sidecar died after we enqueued.
            Err(_) => Err(CoreError::sidecar_lost().message),
        }
    }

    /// Resolves once the sidecar is ready, or errors after [`READY_TIMEOUT`].
    async fn wait_ready(&self) -> Result<(), String> {
        if *self.ready_rx.borrow() {
            return Ok(());
        }
        let mut rx = self.ready_rx.clone();
        let wait = async {
            loop {
                if rx.changed().await.is_err() {
                    return Err(CoreError::sidecar_lost().message);
                }
                if *rx.borrow() {
                    return Ok(());
                }
            }
        };
        match tokio::time::timeout(READY_TIMEOUT, wait).await {
            Ok(result) => result,
            Err(_) => Err("core sidecar did not become ready in time".to_string()),
        }
    }
}

/// Serializes one frame to the sidecar's stdin as a single line.
fn write_frame(shared: &Shared, frame: &ClientFrame) -> Result<(), String> {
    let mut line = serde_json::to_string(frame).map_err(|error| error.to_string())?;
    line.push('\n');
    let mut guard = shared.child.lock().unwrap();
    match guard.as_mut() {
        Some(child) => child
            .write(line.as_bytes())
            .map_err(|error| format!("sidecar write failed: {error}")),
        None => Err(CoreError::sidecar_lost().message),
    }
}

/// Completes every pending request with `sidecar_lost` — called when the
/// connection dies so no caller hangs.
fn fail_all_pending(shared: &Shared) {
    let mut pending = shared.pending.lock().unwrap();
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(CoreError::sidecar_lost()));
    }
}

/// Spawns and re-spawns the sidecar, surfacing health to the webview.
async fn supervise(app: AppHandle, shared: Arc<Shared>, db_path: PathBuf) {
    let mut attempts = 0u32;
    let mut backoff = Duration::from_millis(500);

    loop {
        let readied = run_connection(&app, &shared, db_path.clone()).await;

        // The connection ended: tear down shared state and notify the UI.
        let _ = shared.ready_tx.send(false);
        *shared.child.lock().unwrap() = None;
        fail_all_pending(&shared);
        let _ = app.emit("sidecar-status", "down");

        if readied {
            // A healthy session that later died gets a fresh restart budget.
            attempts = 0;
            backoff = Duration::from_millis(500);
        }

        attempts += 1;
        if attempts > MAX_RESTART_ATTEMPTS {
            eprintln!("sidecar: giving up after {} failed attempts", attempts - 1);
            let _ = app.emit("sidecar-status", "unhealthy");
            return;
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

/// Runs one sidecar connection from spawn to death. Returns whether it ever
/// reached `Ready` (so the supervisor can reset its backoff for healthy runs).
async fn run_connection(app: &AppHandle, shared: &Arc<Shared>, db_path: PathBuf) -> bool {
    let spawned = match app.shell().sidecar(SIDECAR_NAME) {
        Ok(command) => command.spawn(),
        Err(error) => {
            eprintln!("sidecar: cannot resolve sidecar binary: {error}");
            return false;
        }
    };
    let (mut events, child) = match spawned {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("sidecar: spawn failed: {error}");
            return false;
        }
    };

    // Tie its lifetime to ours before it can spawn any adapters.
    if let Some(job) = &shared.job {
        job.assign(child.pid());
    }
    *shared.child.lock().unwrap() = Some(child);

    let mut readied = false;
    let mut buffer: Vec<u8> = Vec::new();

    'conn: while let Some(event) = events.recv().await {
        match event {
            CommandEvent::Stdout(bytes) => {
                buffer.extend_from_slice(&bytes);
                while let Some(line) = take_line(&mut buffer) {
                    if line.is_empty() {
                        continue;
                    }
                    let frame: ServerFrame = match serde_json::from_str(&line) {
                        Ok(frame) => frame,
                        Err(error) => {
                            eprintln!("sidecar: undecodable frame: {error}");
                            continue;
                        }
                    };
                    match frame {
                        ServerFrame::Hello {
                            protocol_version, ..
                        } => {
                            if protocol_version != PROTOCOL_VERSION {
                                eprintln!(
                                    "sidecar: protocol mismatch (sidecar {protocol_version}, host {PROTOCOL_VERSION})"
                                );
                                break 'conn;
                            }
                            if let Err(error) = write_frame(
                                shared,
                                &ClientFrame::Initialize {
                                    db_path: db_path.clone(),
                                },
                            ) {
                                eprintln!("sidecar: failed to send initialize: {error}");
                                break 'conn;
                            }
                        }
                        ServerFrame::Ready => {
                            readied = true;
                            let _ = shared.ready_tx.send(true);
                            let _ = app.emit("sidecar-status", "ready");
                        }
                        ServerFrame::Response { id, result } => {
                            if let Some(tx) = shared.pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(Ok(result));
                            }
                        }
                        ServerFrame::Error { id, error } => {
                            if let Some(tx) = shared.pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(Err(error));
                            }
                        }
                        ServerFrame::Event { event } => forward_event(app, event),
                        ServerFrame::Notification { .. } | ServerFrame::Unknown => {}
                    }
                }
            }
            CommandEvent::Stderr(bytes) => {
                eprintln!("[sidecar] {}", String::from_utf8_lossy(&bytes).trim_end());
            }
            CommandEvent::Terminated(_) => break,
            CommandEvent::Error(error) => {
                eprintln!("[sidecar] stream error: {error}");
                break;
            }
            _ => {}
        }
    }

    readied
}

/// Forwards a streamed Core event to the webview. Chat-run events keep the exact
/// `chat-run-event` channel + payload the frontend already listens on.
fn forward_event(app: &AppHandle, event: CoreEvent) {
    match event {
        CoreEvent::ChatRun(run_event) => {
            let _ = app.emit("chat-run-event", run_event);
        }
        CoreEvent::Unknown => {}
    }
}

/// Pops the next complete `\n`-terminated line from a byte buffer, trimming any
/// trailing `\r`. Returns `None` until a full line is buffered.
fn take_line(buffer: &mut Vec<u8>) -> Option<String> {
    let newline = buffer.iter().position(|&byte| byte == b'\n')?;
    let line: Vec<u8> = buffer.drain(..=newline).collect();
    Some(
        String::from_utf8_lossy(&line[..line.len() - 1])
            .trim_end_matches('\r')
            .to_string(),
    )
}
