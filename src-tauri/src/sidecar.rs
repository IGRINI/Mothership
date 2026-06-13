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
//!   parking in the terminal `failed` state after a few rapid failures until
//!   a manual `restart_sidecar` command wakes it with a fresh budget.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mothership_core::ipc::{
    ClientFrame, CoreError, CoreEvent, CoreRequest, CoreResponse, ServerFrame, PROTOCOL_VERSION,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tokio::sync::{oneshot, watch};

use crate::job::JobHandle;

const SIDECAR_NAME: &str = "mothership-sidecar";
/// How many rapid (never-readied) crashes before we stop trying and report
/// `failed`. A connection that successfully readies resets the budget.
const MAX_RESTART_ATTEMPTS: u32 = 3;
/// How long a command waits for the sidecar to become ready before failing,
/// rather than hanging forever when the sidecar is unhealthy.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// Backoff between automatic restart attempts: starts here and doubles up to
/// [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// How long a manual restart waits for the new sidecar to become ready before
/// reporting failure (covers a full retry cycle: ~3.5s of backoff + spawns).
const RESTART_TIMEOUT: Duration = Duration::from_secs(20);
/// Error returned to request callers while the supervisor is parked in the
/// terminal `failed` state (only `restart_sidecar` leaves it).
const SIDECAR_FAILED_MESSAGE: &str =
    "core sidecar is stopped after repeated crashes; restart it manually";

/// Supervisor-level health of the Core sidecar, as seen by the host process.
///
/// Every transition is pushed to the webview as a `sidecar-status` event with
/// exactly this serialized shape, and the current value is returned by the
/// `get_sidecar_health` / `restart_sidecar` commands (so a listener that
/// mounts after an event already fired can still seed itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SidecarHealth {
    /// The sidecar is being launched (first boot or a manual restart); the
    /// handshake has not completed yet.
    Starting,
    /// Handshake complete; requests are served.
    Ready,
    /// The connection died; the supervisor is restarting it with backoff.
    /// `attempt` is the upcoming restart attempt (1-based) out of
    /// `max_attempts`. Transient: `ready` or `failed` follows.
    Down {
        attempt: u32,
        #[serde(rename = "maxAttempts")]
        max_attempts: u32,
    },
    /// Terminal: the restart budget is exhausted and the supervisor stopped
    /// trying. Only a manual `restart_sidecar` leaves this state. `permanent`
    /// is always `true` — a stable flag so the UI can tell this apart from
    /// the transient `down` without matching on state names.
    Failed { permanent: bool },
}

type ResponseResult = Result<CoreResponse, CoreError>;
type Pending = Mutex<HashMap<u64, oneshot::Sender<ResponseResult>>>;

/// State shared between the supervisor task and the request callers.
struct Shared {
    pending: Pending,
    /// The live sidecar's stdin handle; `None` while down or restarting.
    child: Mutex<Option<CommandChild>>,
    next_id: AtomicU64,
    /// Single source of truth for supervisor-level health; every send is
    /// mirrored to the webview by [`publish_health`].
    health_tx: watch::Sender<SidecarHealth>,
    /// Restart generation, bumped by [`Sidecar::restart`] to wake a supervisor
    /// parked in the `failed` state.
    restart_tx: watch::Sender<u64>,
    /// Windows kill-on-close job the sidecar is assigned to (no-op elsewhere).
    job: Option<JobHandle>,
}

/// A cheap, cloneable client handle stored in app state. Cloning shares the same
/// sidecar connection (commands clone it out of `State` so the request future
/// stays `Send`).
#[derive(Clone)]
pub struct Sidecar {
    shared: Arc<Shared>,
    health_rx: watch::Receiver<SidecarHealth>,
}

impl Sidecar {
    /// Spawns the sidecar and its supervisor task. Returns immediately; the
    /// handshake completes asynchronously and requests wait for readiness.
    pub fn start(app: &AppHandle, db_path: PathBuf) -> Self {
        let (health_tx, health_rx) = watch::channel(SidecarHealth::Starting);
        let (restart_tx, restart_rx) = watch::channel(0u64);
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            child: Mutex::new(None),
            next_id: AtomicU64::new(1),
            health_tx,
            restart_tx,
            job: JobHandle::create(),
        });

        tauri::async_runtime::spawn(supervise(
            app.clone(),
            Arc::clone(&shared),
            db_path,
            restart_rx,
        ));

        Self { shared, health_rx }
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
    /// Fails fast (no timeout wait) when the supervisor has given up: only a
    /// manual restart leaves that state, so waiting would just hang callers.
    async fn wait_ready(&self) -> Result<(), String> {
        let mut rx = self.health_rx.clone();
        let wait = async {
            loop {
                match *rx.borrow_and_update() {
                    SidecarHealth::Ready => return Ok(()),
                    SidecarHealth::Failed { .. } => return Err(SIDECAR_FAILED_MESSAGE.to_string()),
                    SidecarHealth::Starting | SidecarHealth::Down { .. } => {}
                }
                if rx.changed().await.is_err() {
                    return Err(CoreError::sidecar_lost().message);
                }
            }
        };
        match tokio::time::timeout(READY_TIMEOUT, wait).await {
            Ok(result) => result,
            Err(_) => Err("core sidecar did not become ready in time".to_string()),
        }
    }

    /// Current supervisor-level health (the same value the latest
    /// `sidecar-status` event carried).
    pub fn health(&self) -> SidecarHealth {
        *self.health_rx.borrow()
    }

    /// Manual restart for the gave-up state.
    ///
    /// - Alive or already restarting (`starting`/`ready`/`down`): kills
    ///   nothing, returns the current health unchanged.
    /// - `failed`: wakes the parked supervisor with a fresh restart budget on
    ///   this same handle (the pending map and child slot keep working), then
    ///   resolves with `Ready` once the new sidecar handshakes, or errors if
    ///   it fails again or [`RESTART_TIMEOUT`] elapses.
    pub async fn restart(&self) -> Result<SidecarHealth, String> {
        let mut rx = self.health_rx.clone();
        let current = *rx.borrow_and_update();
        if !matches!(current, SidecarHealth::Failed { .. }) {
            return Ok(current);
        }

        // Wake the parked supervisor. Safe even if it raced out of `failed`:
        // the supervisor marks the generation seen *before* publishing
        // `failed`, so a stale bump is ignored by the next park.
        self.shared
            .restart_tx
            .send_modify(|generation| *generation += 1);

        let wait = async {
            loop {
                if rx.changed().await.is_err() {
                    return Err("core sidecar supervisor is gone".to_string());
                }
                match *rx.borrow_and_update() {
                    SidecarHealth::Ready => return Ok(SidecarHealth::Ready),
                    SidecarHealth::Failed { .. } => {
                        return Err(format!(
                            "sidecar restart failed: it crashed {MAX_RESTART_ATTEMPTS} times in a row and gave up again"
                        ))
                    }
                    SidecarHealth::Starting | SidecarHealth::Down { .. } => {}
                }
            }
        };
        match tokio::time::timeout(RESTART_TIMEOUT, wait).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "sidecar did not become ready within {}s after restart",
                RESTART_TIMEOUT.as_secs()
            )),
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

/// Records the new supervisor-level health and mirrors it to the webview as a
/// `sidecar-status` event, so the queryable watch and the event stream can
/// never disagree.
fn publish_health(app: &AppHandle, shared: &Shared, health: SidecarHealth) {
    let _ = shared.health_tx.send(health);
    let _ = app.emit("sidecar-status", health);
}

/// Spawns and re-spawns the sidecar, surfacing health to the webview. When the
/// restart budget is exhausted it parks in the terminal `failed` state instead
/// of returning, so [`Sidecar::restart`] can wake it with a fresh budget on
/// the same shared handle.
async fn supervise(
    app: AppHandle,
    shared: Arc<Shared>,
    db_path: PathBuf,
    mut restart_rx: watch::Receiver<u64>,
) {
    let mut attempts = 0u32;
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let readied = run_connection(&app, &shared, db_path.clone()).await;

        // The connection ended: tear down shared state so no caller hangs.
        *shared.child.lock().unwrap() = None;
        fail_all_pending(&shared);

        if readied {
            // A healthy session that later died gets a fresh restart budget.
            attempts = 0;
            backoff = INITIAL_BACKOFF;
        }

        attempts += 1;
        if attempts > MAX_RESTART_ATTEMPTS {
            eprintln!(
                "sidecar: giving up after {} failed attempts; waiting for a manual restart",
                attempts - 1
            );
            // Mark the current restart generation seen *before* publishing
            // `failed`: a restart command only bumps it after observing
            // `failed`, so its wake-up can never be swallowed as stale.
            restart_rx.borrow_and_update();
            publish_health(&app, &shared, SidecarHealth::Failed { permanent: true });
            if restart_rx.changed().await.is_err() {
                return; // Restart handle dropped; nothing left to supervise for.
            }
            attempts = 0;
            backoff = INITIAL_BACKOFF;
            publish_health(&app, &shared, SidecarHealth::Starting);
            continue;
        }

        publish_health(
            &app,
            &shared,
            SidecarHealth::Down {
                attempt: attempts,
                max_attempts: MAX_RESTART_ATTEMPTS,
            },
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
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
                            eprintln!(
                                "sidecar: undecodable frame: {error}; {}",
                                invalid_frame_preview(&line)
                            );
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
                            publish_health(app, shared, SidecarHealth::Ready);
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
        CoreEvent::ToolExecution(tool_event) => {
            let _ = app.emit("tool-execution-event", tool_event);
        }
        CoreEvent::ConnectorSettings(connector_event) => {
            let _ = app.emit("connector-settings-event", connector_event);
        }
        CoreEvent::ChatUpdated(chat_event) => {
            let _ = app.emit("chat-updated", chat_event);
        }
        CoreEvent::ChangeSet(change_event) => {
            let _ = app.emit("change-set-event", change_event);
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

fn invalid_frame_preview(line: &str) -> String {
    let first = line
        .trim_start()
        .chars()
        .next()
        .map(|character| character.escape_debug().to_string())
        .unwrap_or_else(|| "<empty>".to_string());
    let prefix_hex = line
        .as_bytes()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        "len={}, first='{}', prefix_hex=[{}]",
        line.len(),
        first,
        prefix_hex
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The UI switches on this exact wire shape (the `sidecar-status` event
    /// payload and the `get_sidecar_health` / `restart_sidecar` results), so
    /// lock every variant down.
    #[test]
    fn sidecar_health_serializes_to_the_documented_contract() {
        assert_eq!(
            serde_json::to_value(SidecarHealth::Starting).unwrap(),
            json!({ "state": "starting" })
        );
        assert_eq!(
            serde_json::to_value(SidecarHealth::Ready).unwrap(),
            json!({ "state": "ready" })
        );
        assert_eq!(
            serde_json::to_value(SidecarHealth::Down {
                attempt: 2,
                max_attempts: MAX_RESTART_ATTEMPTS,
            })
            .unwrap(),
            json!({ "state": "down", "attempt": 2, "maxAttempts": 3 })
        );
        assert_eq!(
            serde_json::to_value(SidecarHealth::Failed { permanent: true }).unwrap(),
            json!({ "state": "failed", "permanent": true })
        );
    }
}
