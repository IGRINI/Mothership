//! Mothership core sidecar: the long-running backend process.
//!
//! Core (database, provider adapters, chat runs, the credential vault) lives
//! here, out of the desktop host. The host is a thin client that speaks the
//! newline-delimited JSON protocol in [`mothership_core::ipc`] over this
//! process's stdio.
//!
//! Lifecycle: emit `Hello`, wait for the host's `Initialize` (which carries the
//! database path), open + migrate the database, emit `Ready`, then serve
//! requests until stdin closes (host gone) or a `Shutdown` arrives.
//!
//! stdout is **protocol-only** — a single writer thread owns it and every other
//! line of output goes to stderr, so nothing can corrupt the framing. Each
//! request is handled on its own worker thread, so a long flow (browser OAuth, a
//! streaming chat turn) never blocks the reader or another request.

use std::io::{BufRead, Write};
use std::sync::{mpsc, Arc};
use std::thread;

use mothership_core::ipc::{
    ClientFrame, CoreEvent, CoreError, CoreRequest, CoreResponse, ServerFrame, PROTOCOL_VERSION,
};
use mothership_core::{
    AuthProcessRegistry, ChatRunEvent, ChatRunEventSink, ChatRunService, ConnectorService, Database,
    SendChatMessageResult,
};

/// Frames queued for the writer thread, which alone owns stdout.
type Outbox = mpsc::Sender<ServerFrame>;

fn main() {
    if let Err(error) = serve() {
        eprintln!("sidecar: fatal: {error:#}");
        std::process::exit(1);
    }
}

fn serve() -> anyhow::Result<()> {
    // One writer thread owns stdout; everyone else sends frames through `outbox`.
    let (outbox, frames) = mpsc::channel::<ServerFrame>();
    let writer = thread::spawn(move || write_frames(frames));

    // Advertise the protocol before doing anything else.
    let _ = outbox.send(ServerFrame::Hello {
        protocol_version: PROTOCOL_VERSION,
        sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
    });

    let stdin = std::io::stdin();
    let mut input = stdin.lock();

    // Phase 1: wait for Initialize (carrying the database path).
    let Some(db_path) = wait_for_initialize(&mut input)? else {
        return Ok(()); // stdin closed / Shutdown before Initialize
    };

    // Phase 2: open + migrate + recover. This is the work the host used to do at
    // startup; it now lives with the data it touches.
    let database = Database::open(&db_path)?;
    database.recover_interrupted_chat_runs()?;
    let _ = outbox.send(ServerFrame::Ready);

    // In-flight auth flows live here now (the host no longer tracks adapter PIDs).
    let auth_registry: Arc<AuthProcessRegistry> = Arc::new(AuthProcessRegistry::default());

    // Phase 3: serve. Each request runs on its own worker so a slow flow can't
    // stall the reader or sibling requests.
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break; // stdin EOF: the host is gone.
        }
        let frame: ClientFrame = match serde_json::from_str(line.trim()) {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("sidecar: ignoring unparseable frame: {error}");
                continue;
            }
        };
        match frame {
            ClientFrame::Request { id, request } => {
                let database = database.clone();
                let outbox = outbox.clone();
                let auth_registry = Arc::clone(&auth_registry);
                thread::spawn(move || handle_request(id, request, database, outbox, auth_registry));
            }
            ClientFrame::Shutdown => break,
            // Initialize after the handshake is a protocol slip; ignore it.
            ClientFrame::Initialize { .. } => {}
        }
    }

    drop(outbox);
    let _ = writer.join();
    Ok(())
}

/// Reads frames until an `Initialize` arrives, returning its database path.
/// Returns `Ok(None)` if stdin closes (or `Shutdown` arrives) first.
fn wait_for_initialize(
    input: &mut impl BufRead,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        match serde_json::from_str::<ClientFrame>(line.trim()) {
            Ok(ClientFrame::Initialize { db_path }) => return Ok(Some(db_path)),
            Ok(ClientFrame::Shutdown) => return Ok(None),
            Ok(_) => eprintln!("sidecar: dropping request before Initialize"),
            Err(error) => eprintln!("sidecar: ignoring unparseable frame: {error}"),
        }
    }
}

/// The writer thread body: serialize each frame to one stdout line and flush.
fn write_frames(frames: mpsc::Receiver<ServerFrame>) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    while let Ok(frame) = frames.recv() {
        match serde_json::to_string(&frame) {
            Ok(line) => {
                if writeln!(out, "{line}").is_err() || out.flush().is_err() {
                    break; // stdout closed
                }
            }
            Err(error) => eprintln!("sidecar: failed to encode frame: {error}"),
        }
    }
}

/// Dispatches one request to the matching Core service and writes its terminal
/// `Response`/`Error`. `SendChatMessage` is special: it answers immediately with
/// the persisted placeholder, then streams the run as `Event::ChatRun`s.
fn handle_request(
    id: u64,
    request: CoreRequest,
    database: Database,
    outbox: Outbox,
    auth_registry: Arc<AuthProcessRegistry>,
) {
    // Streaming requests answer immediately with the persisted placeholder, then
    // stream the run; handle them before the uniform request/response path.
    if let CoreRequest::SendChatMessage { chat_id, content } = &request {
        let started = database.begin_chat_run(chat_id.as_deref(), content);
        run_chat_message(id, started, database, outbox);
        return;
    }
    if let CoreRequest::RetryChatMessage { chat_id } = &request {
        let started = database.begin_retry_run(chat_id);
        run_chat_message(id, started, database, outbox);
        return;
    }

    let result = compute(request, &database, &auth_registry);
    let _ = match result {
        Ok(response) => outbox.send(ServerFrame::Response { id, result: response }),
        Err(error) => outbox.send(ServerFrame::Error { id, error }),
    };
}

/// Handles every non-streaming request. `?` short-circuits to a `CoreError`.
fn compute(
    request: CoreRequest,
    database: &Database,
    auth_registry: &AuthProcessRegistry,
) -> Result<CoreResponse, CoreError> {
    Ok(match request {
        CoreRequest::DashboardSnapshot => CoreResponse::Dashboard(database.snapshot()?),
        CoreRequest::AppendActivityEvent { message } => {
            database.append_activity_event(&message)?;
            CoreResponse::Dashboard(database.snapshot()?)
        }
        CoreRequest::ListChats { limit } => {
            CoreResponse::ChatList(database.list_chats(limit.unwrap_or(100))?)
        }
        CoreRequest::CreateChat => CoreResponse::Chat(database.create_chat()?),
        CoreRequest::GetChat { chat_id, limit } => {
            CoreResponse::Chat(database.get_chat(&chat_id, limit.unwrap_or(200))?)
        }
        CoreRequest::ConnectorSettings => {
            CoreResponse::ConnectorSettings(ConnectorService::new(database).snapshot()?)
        }
        CoreRequest::SetSelectedModel {
            provider_id,
            model_id,
        } => CoreResponse::ConnectorSettings(
            ConnectorService::new(database).set_selected_model(&provider_id, &model_id)?,
        ),
        CoreRequest::SaveAdapterSettings {
            provider_id,
            values,
        } => CoreResponse::ConnectorSettings(
            ConnectorService::new(database).save_adapter_settings(&provider_id, values)?,
        ),
        CoreRequest::Authenticate { provider_id } => CoreResponse::ConnectorSettings(
            ConnectorService::new(database).authenticate(&provider_id, auth_registry)?,
        ),
        CoreRequest::CancelAuthenticate { provider_id } => CoreResponse::ConnectorSettings(
            ConnectorService::new(database).cancel_authenticate(&provider_id, auth_registry)?,
        ),
        CoreRequest::Logout { provider_id } => {
            CoreResponse::ConnectorSettings(ConnectorService::new(database).logout(&provider_id)?)
        }
        CoreRequest::SidecarStatus => CoreResponse::SidecarStatus(database.sidecar_status()?),
        // Streaming cases handled in `handle_request` before reaching here.
        CoreRequest::SendChatMessage { .. } | CoreRequest::RetryChatMessage { .. } => {
            unreachable!("handled as a streaming request")
        }
    })
}

/// The streaming path: given the begun run (a fresh send or a retry), answer the
/// request with the persisted placeholder, then drive the completion, forwarding
/// every run event. A failure to even begin the run is a terminal error reply.
fn run_chat_message(
    id: u64,
    started: mothership_core::Result<SendChatMessageResult>,
    database: Database,
    outbox: Outbox,
) {
    match started {
        Ok(started) => {
            let _ = outbox.send(ServerFrame::Response {
                id,
                result: CoreResponse::ChatMessageStarted(started.clone()),
            });
            let mut sink = ProtocolChatRunSink { outbox };
            ChatRunService::new(&database).run(&started, &mut sink);
        }
        Err(error) => {
            let _ = outbox.send(ServerFrame::Error {
                id,
                error: error.into(),
            });
        }
    }
}

/// Forwards Core's chat-run events to the host as `Event::ChatRun` frames.
struct ProtocolChatRunSink {
    outbox: Outbox,
}

impl ChatRunEventSink for ProtocolChatRunSink {
    fn emit(&mut self, event: ChatRunEvent) {
        let _ = self.outbox.send(ServerFrame::Event {
            event: CoreEvent::ChatRun(event),
        });
    }
}
