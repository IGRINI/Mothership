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

use std::fs;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;

use mothership_core::ipc::{
    ClientFrame, CoreError, CoreEvent, CoreRequest, CoreResponse, ServerFrame, PROTOCOL_VERSION,
};
use mothership_core::{
    schedule_cancel_fallback, trusted_built_in_adapter_sha256, AdapterPool, AuthProcessRegistry,
    ChatRunCancellationResult, ChatRunEvent, ChatRunEventSink, ChatRunRegistry, ChatRunService,
    ConnectorManager, ConnectorSettingsEvent, ConnectorSettingsEventKind, Database,
    ProviderRuntimeManager, SendChatMessageResult,
};
use sha2::{Digest, Sha256};

struct BuiltInAdapter {
    provider_id: &'static str,
    provider_label: &'static str,
    binary_name: &'static str,
    icon_svg: &'static str,
    capabilities: &'static [&'static str],
}

const BUILT_IN_ADAPTERS: &[BuiltInAdapter] = &[
    BuiltInAdapter {
        provider_id: "codex",
        provider_label: "Codex",
        binary_name: "codex-adapter",
        icon_svg: include_str!("../../adapters/codex/icon.svg"),
        capabilities: &[
            "llm.models",
            "llm.chat",
            "settings.read",
            "auth.interactive",
            "auth.logout",
            "network",
            "browser.open",
            "localhost.listen",
        ],
    },
    BuiltInAdapter {
        provider_id: "openrouter",
        provider_label: "OpenRouter",
        binary_name: "openrouter-adapter",
        icon_svg: include_str!("../../adapters/openrouter/icon.svg"),
        capabilities: &[
            "llm.models",
            "llm.chat",
            "settings.read",
            "settings.write",
            "network",
        ],
    },
];

/// Frames queued for the writer thread, which alone owns stdout.
type Outbox = mpsc::Sender<ServerFrame>;

struct RequestOutcome {
    response: CoreResponse,
    event: Option<CoreEvent>,
    connector_refresh: Option<ConnectorRefreshScope>,
}

enum ConnectorRefreshScope {
    All,
    Provider(String),
}

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
    install_built_in_adapters(&db_path)?;
    database.recover_interrupted_chat_runs()?;

    // In-flight auth flows live here now (the host no longer tracks adapter PIDs).
    let auth_registry: Arc<AuthProcessRegistry> = Arc::new(AuthProcessRegistry::default());
    // Resident adapter processes, reused across operations instead of spawning
    // one per request.
    let pool = Arc::new(AdapterPool::new());
    let connector_manager = Arc::new(ConnectorManager::new(database.clone(), Arc::clone(&pool)));
    let provider_manager = Arc::new(ProviderRuntimeManager::new(Arc::clone(&pool)));
    let chat_registry = Arc::new(ChatRunRegistry::new());
    let _ = outbox.send(ServerFrame::Ready);
    start_connector_refresh(
        Arc::clone(&connector_manager),
        outbox.clone(),
        ConnectorRefreshScope::All,
    );

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
                let connector_manager = Arc::clone(&connector_manager);
                let provider_manager = Arc::clone(&provider_manager);
                let chat_registry = Arc::clone(&chat_registry);
                thread::spawn(move || {
                    handle_request(
                        id,
                        request,
                        database,
                        outbox,
                        auth_registry,
                        connector_manager,
                        provider_manager,
                        chat_registry,
                    )
                });
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

fn install_built_in_adapters(db_path: &Path) -> anyhow::Result<()> {
    let Some(app_data_dir) = db_path.parent() else {
        return Ok(());
    };
    let plugins_dir = app_data_dir.join("plugins");
    fs::create_dir_all(&plugins_dir)?;

    for adapter in BUILT_IN_ADAPTERS {
        let Some(program) = bundled_program_path(adapter.binary_name) else {
            eprintln!(
                "sidecar: cannot resolve built-in adapter binary for {}",
                adapter.provider_id
            );
            continue;
        };
        if !program.is_file() {
            eprintln!(
                "sidecar: built-in adapter binary is missing for {}: {}",
                adapter.provider_id,
                program.display()
            );
            remove_built_in_manifest(&plugins_dir, adapter.provider_id);
            continue;
        }

        let Some(expected_sha256) = trusted_built_in_adapter_sha256(adapter.provider_id) else {
            eprintln!(
                "sidecar: no trusted build hash embedded for built-in adapter {}",
                adapter.provider_id
            );
            remove_built_in_manifest(&plugins_dir, adapter.provider_id);
            continue;
        };

        let actual_sha256 = file_sha256_hex(&program)?;
        if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
            eprintln!(
                "sidecar: built-in adapter {} failed integrity check: expected {}, got {}",
                adapter.provider_id, expected_sha256, actual_sha256
            );
        }

        let adapter_dir = plugins_dir.join(adapter.provider_id);
        fs::create_dir_all(&adapter_dir)?;
        write_if_changed(adapter_dir.join("icon.svg"), adapter.icon_svg.as_bytes())?;

        let manifest = serde_json::json!({
            "provider_id": adapter.provider_id,
            "provider_label": adapter.provider_label,
            "program": program,
            "icon": "icon.svg",
            "capabilities": adapter.capabilities,
            "integrity": {
                "algorithm": "sha256",
                "sha256": expected_sha256,
            },
        });
        let manifest = serde_json::to_vec_pretty(&manifest)?;
        let mut manifest_with_newline = manifest;
        manifest_with_newline.push(b'\n');
        write_if_changed(adapter_dir.join("adapter.json"), &manifest_with_newline)?;
    }

    Ok(())
}

fn bundled_program_path(binary_name: &str) -> Option<PathBuf> {
    let sidecar_path = std::env::current_exe().ok()?;
    let directory = sidecar_path.parent()?;
    let file_name = sidecar_path.file_name()?.to_str()?;
    let sidecar_prefix = "mothership-sidecar-";

    if let Some(target_suffix) = file_name.strip_prefix(sidecar_prefix) {
        return Some(directory.join(format!("{binary_name}-{target_suffix}")));
    }

    Some(directory.join(format!("{binary_name}{}", std::env::consts::EXE_SUFFIX)))
}

fn write_if_changed(path: impl AsRef<Path>, bytes: &[u8]) -> anyhow::Result<()> {
    let path = path.as_ref();
    if fs::read(path)
        .map(|existing| existing == bytes)
        .unwrap_or(false)
    {
        return Ok(());
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn remove_built_in_manifest(plugins_dir: &Path, provider_id: &str) {
    let manifest = plugins_dir.join(provider_id).join("adapter.json");
    if let Err(error) = fs::remove_file(&manifest) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "sidecar: failed to remove invalid built-in adapter manifest {}: {error}",
                manifest.display()
            );
        }
    }
}

fn file_sha256_hex(path: &Path) -> anyhow::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Reads frames until an `Initialize` arrives, returning its database path.
/// Returns `Ok(None)` if stdin closes (or `Shutdown` arrives) first.
fn wait_for_initialize(input: &mut impl BufRead) -> anyhow::Result<Option<std::path::PathBuf>> {
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
    connector_manager: Arc<ConnectorManager>,
    provider_manager: Arc<ProviderRuntimeManager>,
    chat_registry: Arc<ChatRunRegistry>,
) {
    // Streaming requests answer immediately with the persisted placeholder, then
    // stream the run; handle them before the uniform request/response path.
    if let CoreRequest::SendChatMessage { chat_id, content } = &request {
        let started = database.begin_chat_run(chat_id.as_deref(), content);
        run_chat_message(id, started, database, outbox, provider_manager, chat_registry);
        return;
    }
    if let CoreRequest::RetryChatMessage { chat_id } = &request {
        let started = database.begin_retry_run(chat_id);
        run_chat_message(id, started, database, outbox, provider_manager, chat_registry);
        return;
    }

    let result = compute(
        request,
        &database,
        &auth_registry,
        &connector_manager,
        &provider_manager,
        &chat_registry,
    );
    let _ = match result {
        Ok(outcome) => {
            let response_result = outbox.send(ServerFrame::Response {
                id,
                result: outcome.response,
            });
            if response_result.is_ok() {
                if let Some(event) = outcome.event {
                    let _ = outbox.send(ServerFrame::Event { event });
                }
                if let Some(scope) = outcome.connector_refresh {
                    start_connector_refresh(Arc::clone(&connector_manager), outbox.clone(), scope);
                }
            }
            response_result
        }
        Err(error) => outbox.send(ServerFrame::Error { id, error }),
    };
}

/// Handles every non-streaming request. `?` short-circuits to a `CoreError`.
fn compute(
    request: CoreRequest,
    database: &Database,
    auth_registry: &AuthProcessRegistry,
    connector_manager: &Arc<ConnectorManager>,
    provider_manager: &Arc<ProviderRuntimeManager>,
    chat_registry: &Arc<ChatRunRegistry>,
) -> Result<RequestOutcome, CoreError> {
    Ok(match request {
        CoreRequest::DashboardSnapshot => response(CoreResponse::Dashboard(database.snapshot()?)),
        CoreRequest::AppendActivityEvent { message } => {
            database.append_activity_event(&message)?;
            response(CoreResponse::Dashboard(database.snapshot()?))
        }
        CoreRequest::ListChats { limit } => response(CoreResponse::ChatList(
            database.list_chats(limit.unwrap_or(100))?,
        )),
        CoreRequest::CreateChat => response(CoreResponse::Chat(database.create_chat()?)),
        CoreRequest::GetChat { chat_id, limit } => response(CoreResponse::Chat(
            database.get_chat(&chat_id, limit.unwrap_or(200))?,
        )),
        CoreRequest::CancelChatRun { run_id } => {
            if let Some(provider_id) = chat_registry.cancel(&run_id) {
                schedule_cancel_fallback(
                    Arc::clone(provider_manager),
                    Arc::clone(chat_registry),
                    run_id.clone(),
                    provider_id,
                );
            }
            response(CoreResponse::ChatRunCancellation(
                ChatRunCancellationResult {
                    run_id,
                    accepted: true,
                },
            ))
        }
        CoreRequest::ConnectorSettings => response_with_connector_refresh(
            CoreResponse::ConnectorSettings(connector_manager.snapshot()?),
            Some(ConnectorRefreshScope::All),
        ),
        CoreRequest::SetSelectedModel {
            provider_id,
            model_id,
        } => connector_settings_changed(
            ConnectorSettingsEventKind::SelectedModelChanged,
            connector_manager.set_selected_model(&provider_id, &model_id)?,
            None,
        ),
        CoreRequest::SaveAdapterSettings {
            provider_id,
            values,
        } => connector_settings_changed(
            ConnectorSettingsEventKind::AdapterSettingsSaved,
            connector_manager.save_adapter_settings(&provider_id, values)?,
            Some(ConnectorRefreshScope::Provider(provider_id)),
        ),
        CoreRequest::Authenticate { provider_id } => connector_settings_changed(
            ConnectorSettingsEventKind::AuthenticationFinished,
            connector_manager.authenticate(&provider_id, auth_registry)?,
            Some(ConnectorRefreshScope::Provider(provider_id)),
        ),
        CoreRequest::CancelAuthenticate { provider_id } => connector_settings_changed(
            ConnectorSettingsEventKind::AuthenticationCancelled,
            connector_manager.cancel_authenticate(&provider_id, auth_registry)?,
            Some(ConnectorRefreshScope::Provider(provider_id)),
        ),
        CoreRequest::Logout { provider_id } => connector_settings_changed(
            ConnectorSettingsEventKind::LoggedOut,
            connector_manager.logout(&provider_id)?,
            Some(ConnectorRefreshScope::Provider(provider_id)),
        ),
        CoreRequest::SidecarStatus => {
            response(CoreResponse::SidecarStatus(database.sidecar_status()?))
        }
        // Streaming cases handled in `handle_request` before reaching here.
        CoreRequest::SendChatMessage { .. } | CoreRequest::RetryChatMessage { .. } => {
            unreachable!("handled as a streaming request")
        }
    })
}

fn response(response: CoreResponse) -> RequestOutcome {
    response_with_connector_refresh(response, None)
}

fn response_with_connector_refresh(
    response: CoreResponse,
    connector_refresh: Option<ConnectorRefreshScope>,
) -> RequestOutcome {
    RequestOutcome {
        response,
        event: None,
        connector_refresh,
    }
}

fn connector_settings_changed(
    kind: ConnectorSettingsEventKind,
    snapshot: mothership_core::ConnectorSettingsSnapshot,
    connector_refresh: Option<ConnectorRefreshScope>,
) -> RequestOutcome {
    RequestOutcome {
        response: CoreResponse::ConnectorSettings(snapshot.clone()),
        event: Some(CoreEvent::ConnectorSettings(ConnectorSettingsEvent {
            kind,
            snapshot,
        })),
        connector_refresh,
    }
}

fn start_connector_refresh(
    connector_manager: Arc<ConnectorManager>,
    outbox: Outbox,
    scope: ConnectorRefreshScope,
) {
    thread::spawn(move || {
        let mut emit = |event: ConnectorSettingsEvent| {
            let _ = outbox.send(ServerFrame::Event {
                event: CoreEvent::ConnectorSettings(event),
            });
        };

        match scope {
            ConnectorRefreshScope::All => connector_manager.refresh_all(&mut emit),
            ConnectorRefreshScope::Provider(provider_id) => {
                connector_manager.refresh_provider(&provider_id, &mut emit);
            }
        }
    });
}

/// The streaming path: given the begun run (a fresh send or a retry), answer the
/// request with the persisted placeholder, then drive the completion, forwarding
/// every run event. A failure to even begin the run is a terminal error reply.
fn run_chat_message(
    id: u64,
    started: mothership_core::Result<SendChatMessageResult>,
    database: Database,
    outbox: Outbox,
    provider_manager: Arc<ProviderRuntimeManager>,
    chat_registry: Arc<ChatRunRegistry>,
) {
    match started {
        Ok(started) => {
            let _ = outbox.send(ServerFrame::Response {
                id,
                result: CoreResponse::ChatMessageStarted(started.clone()),
            });
            let mut sink = ProtocolChatRunSink { outbox };
            ChatRunService::new(&database, provider_manager).run(&started, chat_registry, &mut sink);
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
