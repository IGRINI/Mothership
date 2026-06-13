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
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;

mod tool_runtime;

use mothership_core::ipc::{
    ClientFrame, CoreError, CoreEvent, CoreRequest, CoreResponse, ServerFrame, PROTOCOL_VERSION,
};
use mothership_core::{
    audio_transcribe_tool_descriptor, chat_prompt_preview, default_credential_guard,
    image_generate_tool_descriptor, redact_event, schedule_cancel_fallback,
    trusted_built_in_adapter_sha256, AdapterPool, AuthProcessRegistry, ChangeEventSink,
    ChangeRecorder, ChangeSetEvent, ChangeSetEventKind, ChangesService, ChatRunCancellationResult,
    ChatRunEvent, ChatRunEventSink, ChatRunRegistry, ChatRunService, ChatUpdatedEvent,
    ConnectorManager, ConnectorSettingsEvent, ConnectorSettingsEventKind, Database, FileBlobStore,
    FileToolOutputStore, LlmToolCallHandler, PendingToolApprovalGate, ProviderRuntimeManager,
    RedactingOutputStore, RevertOutcome, SendChatMessageResult, SnapshotBlobStore, StdFileSystem,
    ToolApprovalAnswer, ToolApprovalDecision, ToolApprovalMode, ToolApprovalModeStore,
    ToolCancellationToken, ToolDescriptor, ToolExecutionAccepted, ToolExecutionCancellationResult,
    ToolExecutionEvent, ToolExecutionEventKind, ToolExecutionEventSink, ToolExecutionRegistry,
    ToolExecutionRequest, ToolExecutionResult, ToolExecutionStatus, ToolKind, ToolPolicyStore,
    ToolRepeatGuard, ToolResourceLimits, ToolSupervisor, UserAwareCommandPermissionPolicy,
    Workspace,
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
            "media.image.generate",
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
            "media.image.generate",
            "network",
        ],
    },
    BuiltInAdapter {
        provider_id: "claude-agent",
        provider_label: "Claude Agent SDK",
        binary_name: "claude-agent-adapter",
        icon_svg: include_str!("../../adapters/claude-agent/icon.svg"),
        capabilities: &[
            "llm.models",
            "agent.runtime",
            "agent.self_managed_tools",
            "settings.read",
            "settings.write",
            "network",
            "process.spawn",
        ],
    },
];

const FEATURE_IMAGE_GENERATE: &str = "media.image.generate";
const FEATURE_AUDIO_TRANSCRIBE: &str = "audio.transcribe";

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
    let provider_manager = Arc::new(ProviderRuntimeManager::new(Arc::clone(&pool)));
    let connector_manager = Arc::new(ConnectorManager::with_runtime(
        database.clone(),
        Arc::clone(&pool),
        Arc::clone(&provider_manager),
    ));
    let async_runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("mothership-tool-runtime")
            .build()?,
    );
    let tool_approvals = PendingToolApprovalGate::new();
    let tool_approval_mode = ToolApprovalModeStore::new(ToolApprovalMode::Manual);
    // User-authored command allow/deny + tool toggles, loaded from the database
    // so they survive restarts. Shared between the command permission policy and
    // the typed-tool dispatcher; updated live when the user saves on the
    // Permissions screen.
    let tool_policy = ToolPolicyStore::new(database.tool_policy_settings().unwrap_or_default());
    let tool_repeat_guard = Arc::new(ToolRepeatGuard::default());
    let app_data_root = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let artifacts_root = app_data_root.join("artifacts");
    let tmp_root = app_data_root.join("tmp");
    // Shared output store: the supervisor uses it to spill command output, and
    // the file-tool runner reuses it to spill oversized read_file content.
    let tool_output_store: Option<Arc<dyn mothership_core::ToolOutputStore>> =
        db_path.parent().map(|parent| {
            // Credential firewall: durable spilled blobs (command output, file-tool
            // diffs/reads/search results) are redacted on the way to disk, so the
            // full content behind a logRef never leaks a secret — not just previews.
            let base = Arc::new(
                FileToolOutputStore::new(artifacts_root.clone())
                    .with_legacy_root(parent.join("tool-logs")),
            ) as Arc<dyn mothership_core::ToolOutputStore>;
            Arc::new(RedactingOutputStore::new(
                base,
                Arc::new(mothership_core::PatternCredentialGuard::new()),
            )) as Arc<dyn mothership_core::ToolOutputStore>
        });
    // Content-addressed snapshot store for the change journal. Lives beside the
    // database (never inside the user's `.git`); per-project sharding is handled
    // by the store itself.
    let change_blob_store: Arc<dyn SnapshotBlobStore> = {
        let base = db_path
            .parent()
            .map(|parent| parent.join("changes").join("blobs"))
            .unwrap_or_else(|| PathBuf::from("changes/blobs"));
        Arc::new(FileBlobStore::new(base))
    };
    let tool_supervisor = Arc::new(
        ToolSupervisor::new(
            Arc::new(tool_runtime::ProcessSandboxToolAdapter::new(
                process_sandbox::platform_sandbox(),
            )),
            tool_output_store.clone(),
            ToolResourceLimits::default(),
        )
        .with_policy(Arc::new(UserAwareCommandPermissionPolicy::new(
            Arc::clone(&tool_policy),
            Arc::clone(&tool_approval_mode),
        )))
        .with_repeat_guard(tool_repeat_guard),
    );
    let tool_registry = Arc::new(ToolExecutionRegistry::new());
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
                let tool_supervisor = Arc::clone(&tool_supervisor);
                let tool_approvals = Arc::clone(&tool_approvals);
                let tool_output_store = tool_output_store.clone();
                let tool_registry = Arc::clone(&tool_registry);
                let async_runtime = Arc::clone(&async_runtime);
                let chat_registry = Arc::clone(&chat_registry);
                let change_blob_store = Arc::clone(&change_blob_store);
                let tool_approval_mode = Arc::clone(&tool_approval_mode);
                let tool_policy = Arc::clone(&tool_policy);
                let artifacts_root = artifacts_root.clone();
                let tmp_root = tmp_root.clone();
                thread::spawn(move || {
                    // Panic isolation: a panicking handler must answer its
                    // request id (with a structured error) instead of dying
                    // silently and leaving the client waiting forever.
                    let panic_outbox = outbox.clone();
                    handle_request_isolated(id, &panic_outbox, move || {
                        handle_request(
                            id,
                            request,
                            database,
                            outbox,
                            auth_registry,
                            connector_manager,
                            provider_manager,
                            tool_supervisor,
                            tool_approvals,
                            tool_output_store,
                            tool_registry,
                            async_runtime,
                            chat_registry,
                            change_blob_store,
                            tool_approval_mode,
                            tool_policy,
                            artifacts_root,
                            tmp_root,
                        )
                    });
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

fn app_data_path_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "item".to_string()
    } else {
        out
    }
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

/// Run one request handler body with panic isolation: a panic is contained,
/// logged to stderr, and answered as a structured `internal_panic` error frame
/// for `id`, so the requesting client is never left waiting forever and the
/// serve loop keeps going. The outbox (and the writer thread that owns stdout)
/// stays fully usable afterwards.
fn handle_request_isolated(id: u64, outbox: &Outbox, body: impl FnOnce()) {
    if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(body)) {
        let message = panic_message(panic.as_ref());
        eprintln!("sidecar: request {id} handler panicked: {message}");
        let _ = outbox.send(ServerFrame::Error {
            id,
            error: CoreError::new(
                "internal_panic",
                format!("internal error: the request handler panicked: {message}"),
                false,
            ),
        });
    }
}

/// Best-effort extraction of a panic payload's message (`&str` and `String`
/// cover `panic!` / `unwrap` / `expect`; anything else is reported as opaque).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
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
    tool_supervisor: Arc<ToolSupervisor>,
    tool_approvals: Arc<PendingToolApprovalGate>,
    tool_output_store: Option<Arc<dyn mothership_core::ToolOutputStore>>,
    tool_registry: Arc<ToolExecutionRegistry>,
    async_runtime: Arc<tokio::runtime::Runtime>,
    chat_registry: Arc<ChatRunRegistry>,
    change_blob_store: Arc<dyn SnapshotBlobStore>,
    tool_approval_mode: Arc<ToolApprovalModeStore>,
    tool_policy: Arc<ToolPolicyStore>,
    artifacts_root: PathBuf,
    tmp_root: PathBuf,
) {
    // Streaming requests answer immediately with the persisted placeholder, then
    // stream the run; handle them before the uniform request/response path.
    if let CoreRequest::SendChatMessage {
        chat_id,
        project_id,
        content,
        reasoning,
        fast_mode,
    } = &request
    {
        let started = database.begin_chat_run(
            chat_id.as_deref(),
            project_id.as_deref(),
            content,
            reasoning.clone(),
            *fast_mode,
        );
        run_chat_message(
            id,
            started,
            database,
            outbox,
            Arc::clone(&connector_manager),
            provider_manager,
            tool_supervisor,
            tool_approvals,
            tool_output_store,
            tool_registry,
            async_runtime,
            chat_registry,
            change_blob_store,
            Arc::clone(&tool_approval_mode),
            Arc::clone(&tool_policy),
            artifacts_root.clone(),
            tmp_root.clone(),
        );
        return;
    }
    if let CoreRequest::EditChatUserMessage {
        chat_id,
        message_id,
        content,
    } = &request
    {
        let started = database.begin_edited_chat_run(chat_id, message_id, content);
        run_chat_message(
            id,
            started,
            database,
            outbox,
            Arc::clone(&connector_manager),
            provider_manager,
            tool_supervisor,
            tool_approvals,
            tool_output_store,
            tool_registry,
            async_runtime,
            chat_registry,
            change_blob_store,
            Arc::clone(&tool_approval_mode),
            Arc::clone(&tool_policy),
            artifacts_root.clone(),
            tmp_root.clone(),
        );
        return;
    }
    if let CoreRequest::RetryChatMessage { chat_id } = &request {
        let started = database.begin_retry_run(chat_id);
        run_chat_message(
            id,
            started,
            database,
            outbox,
            Arc::clone(&connector_manager),
            provider_manager,
            tool_supervisor,
            tool_approvals,
            tool_output_store,
            tool_registry,
            async_runtime,
            chat_registry,
            change_blob_store,
            Arc::clone(&tool_approval_mode),
            Arc::clone(&tool_policy),
            artifacts_root.clone(),
            tmp_root.clone(),
        );
        return;
    }
    if let CoreRequest::ContinueChatMessage { chat_id } = &request {
        let started = database.begin_continue_run(chat_id);
        run_chat_message(
            id,
            started,
            database,
            outbox,
            Arc::clone(&connector_manager),
            provider_manager,
            tool_supervisor,
            tool_approvals,
            tool_output_store,
            tool_registry,
            async_runtime,
            chat_registry,
            change_blob_store,
            Arc::clone(&tool_approval_mode),
            Arc::clone(&tool_policy),
            artifacts_root.clone(),
            tmp_root.clone(),
        );
        return;
    }
    if let CoreRequest::RunToolCommand { request } = request {
        run_tool_command(
            id,
            request,
            outbox,
            tool_supervisor,
            tool_registry,
            tool_approvals,
            async_runtime,
        );
        return;
    }
    // Lazy artifact paging needs the output store + the async runtime to block on
    // the store read, so it is handled here rather than in the sync `compute`.
    if let CoreRequest::GetToolArtifactRange {
        tool_call_id,
        log_ref,
        offset,
        limit,
    } = &request
    {
        let result = match &tool_output_store {
            Some(store) => async_runtime
                .block_on(store.read_range(tool_call_id, log_ref, *offset, *limit))
                .map_err(CoreError::from),
            None => Err(CoreError::new(
                "artifact_unavailable",
                "no tool output store is configured",
                false,
            )),
        };
        let _ = match result {
            Ok(range) => outbox.send(ServerFrame::Response {
                id,
                result: CoreResponse::ToolArtifactRange(range),
            }),
            Err(error) => outbox.send(ServerFrame::Error { id, error }),
        };
        return;
    }

    if matches!(
        request,
        CoreRequest::GetChatChangeSets { .. }
            | CoreRequest::GetMessageChangeSummary { .. }
            | CoreRequest::GetChangeFileDiff { .. }
            | CoreRequest::ListChangeSetFiles { .. }
            | CoreRequest::RevertChangeSet { .. }
    ) {
        run_change_request(id, request, database, outbox, change_blob_store);
        return;
    }

    let result = compute(
        request,
        &database,
        &auth_registry,
        &connector_manager,
        &provider_manager,
        &tool_approvals,
        &tool_registry,
        &chat_registry,
        &tool_approval_mode,
        &tool_policy,
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
    tool_approvals: &Arc<PendingToolApprovalGate>,
    tool_registry: &Arc<ToolExecutionRegistry>,
    chat_registry: &Arc<ChatRunRegistry>,
    tool_approval_mode: &Arc<ToolApprovalModeStore>,
    tool_policy: &Arc<ToolPolicyStore>,
) -> Result<RequestOutcome, CoreError> {
    Ok(match request {
        CoreRequest::DashboardSnapshot => response(CoreResponse::Dashboard(database.snapshot()?)),
        CoreRequest::AppendActivityEvent { message } => {
            database.append_activity_event(&message)?;
            response(CoreResponse::Dashboard(database.snapshot()?))
        }
        CoreRequest::ListChats { project_id, limit } => response(CoreResponse::ChatList(
            database.list_chats(project_id.as_deref(), limit.unwrap_or(100))?,
        )),
        CoreRequest::CreateChat {
            project_id,
            copy_from_chat_id,
        } => response(CoreResponse::Chat(
            database.create_chat(&project_id, copy_from_chat_id.as_deref())?,
        )),
        CoreRequest::GetChat { chat_id, limit } => response(CoreResponse::Chat(
            database.get_chat(&chat_id, limit.unwrap_or(200))?,
        )),
        CoreRequest::GetPromptPreview { chat_id } => response(CoreResponse::PromptPreview(
            chat_prompt_preview(database, &chat_id)?,
        )),
        CoreRequest::BranchChatFromMessage {
            chat_id,
            message_id,
        } => response(CoreResponse::Chat(
            database.branch_chat_from_message(&chat_id, &message_id)?,
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
        CoreRequest::ListActiveRuns => {
            response(CoreResponse::ActiveRuns(chat_registry.snapshot()))
        }
        CoreRequest::RenameChat { chat_id, title } => {
            let chat = database.rename_chat(&chat_id, &title)?;
            RequestOutcome {
                response: CoreResponse::ChatSummary(chat.clone()),
                event: Some(CoreEvent::ChatUpdated(ChatUpdatedEvent { chat })),
                connector_refresh: None,
            }
        }
        CoreRequest::DeleteChat { chat_id } => {
            // Stop any in-flight run first — its events/persistence must not
            // race the row deletion (and the agent pill drops it via the
            // cancellation event).
            for run in chat_registry.snapshot() {
                if run.chat_id == chat_id {
                    if let Some(provider_id) = chat_registry.cancel(&run.run_id) {
                        schedule_cancel_fallback(
                            Arc::clone(provider_manager),
                            Arc::clone(chat_registry),
                            run.run_id.clone(),
                            provider_id,
                        );
                    }
                }
            }
            database.delete_chat(&chat_id)?;
            response(CoreResponse::Ack)
        }
        CoreRequest::RenameProject { project_id, name } => {
            response(CoreResponse::ProjectSnapshot(
                database.rename_project(&project_id, &name)?,
            ))
        }
        CoreRequest::DeleteProject { project_id } => {
            // Deleting the project deletes its chats — stop their runs first.
            for run in chat_registry.snapshot() {
                if run.project_id.as_deref() == Some(project_id.as_str()) {
                    if let Some(provider_id) = chat_registry.cancel(&run.run_id) {
                        schedule_cancel_fallback(
                            Arc::clone(provider_manager),
                            Arc::clone(chat_registry),
                            run.run_id.clone(),
                            provider_id,
                        );
                    }
                }
            }
            response(CoreResponse::ProjectSnapshot(
                database.delete_project(&project_id)?,
            ))
        }
        CoreRequest::SetProjectAppearance {
            project_id,
            icon,
            icon_color,
        } => response(CoreResponse::ProjectSnapshot(
            database.set_project_appearance(
                &project_id,
                icon.as_deref(),
                icon_color.as_deref(),
            )?,
        )),
        CoreRequest::ApproveToolExecution {
            tool_call_id,
            approved,
            reason,
        } => {
            let decision = if approved {
                ToolApprovalDecision::Approved
            } else {
                ToolApprovalDecision::Denied {
                    reason: reason.unwrap_or_else(|| "denied by user".to_string()),
                }
            };
            let accepted = tool_approvals.decide(&tool_call_id, decision);
            response(CoreResponse::ToolApproval(ToolApprovalAnswer {
                tool_call_id,
                accepted,
            }))
        }
        CoreRequest::CancelToolExecution { tool_call_id } => {
            let accepted = tool_registry.cancel(&tool_call_id);
            response(CoreResponse::ToolExecutionCancellation(
                ToolExecutionCancellationResult {
                    tool_call_id,
                    accepted,
                },
            ))
        }
        CoreRequest::GetPersonalization => response(CoreResponse::Personalization(
            database.personalization_settings()?,
        )),
        CoreRequest::SetPersonalization {
            provider_id,
            model_id,
            content,
        } => response(CoreResponse::Personalization(
            database.set_personalization(provider_id.as_deref(), model_id.as_deref(), &content)?,
        )),
        CoreRequest::GetChangeJournalRetention => response(CoreResponse::ChangeJournalRetention(
            database.change_journal_retention()?,
        )),
        CoreRequest::SetChangeJournalRetention { value } => response(
            CoreResponse::ChangeJournalRetention(database.set_change_journal_retention(value)?),
        ),
        CoreRequest::GetToolPolicy => response(CoreResponse::ToolPolicy(tool_policy.settings())),
        CoreRequest::SetToolPolicy { settings } => {
            // Update the live store first (sanitizing), then persist exactly the
            // canonical form it now holds, so DB and runtime never diverge.
            let stored = tool_policy.set_settings(settings);
            database.set_tool_policy_settings(&stored)?;
            response(CoreResponse::ToolPolicy(stored))
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
        CoreRequest::SetProviderEnabled {
            provider_id,
            enabled,
        } => connector_settings_changed(
            ConnectorSettingsEventKind::ProviderEnabledChanged,
            connector_manager.set_provider_enabled(&provider_id, enabled)?,
            None,
        ),
        CoreRequest::SetFeatureRoute {
            feature,
            provider_id,
            model_id,
            options,
        } => connector_settings_changed(
            ConnectorSettingsEventKind::ProviderUpdated,
            connector_manager.set_feature_route(&feature, &provider_id, &model_id, options)?,
            None,
        ),
        CoreRequest::SetChatModel {
            chat_id,
            provider_id,
            model_id,
        } => {
            // Validate the model exists among installed adapters WITHOUT mutating
            // the global default (that's the whole point of per-chat).
            connector_manager.ensure_model_supported(&provider_id, &model_id)?;
            let chat = database.set_chat_model(&chat_id, &provider_id, &model_id)?;
            RequestOutcome {
                response: CoreResponse::ChatSummary(chat.clone()),
                event: Some(CoreEvent::ChatUpdated(ChatUpdatedEvent { chat })),
                connector_refresh: None,
            }
        }
        CoreRequest::SetChatState {
            chat_id,
            approval_mode,
            reasoning,
            fast_mode,
            draft,
        } => {
            let chat = database.set_chat_state(
                &chat_id,
                approval_mode.as_deref(),
                reasoning.as_deref(),
                fast_mode,
                draft.as_deref(),
            )?;
            // Live-propagate the saved mode to this chat's in-flight run (if
            // any), scoped to THIS chat only — concurrent runs in other chats
            // keep their own gating. Idle chats are not registered; their next
            // run seeds from the value just persisted.
            tool_approval_mode.update_chat_mode(
                &chat_id,
                parse_chat_approval_mode(chat.approval_mode.as_deref()),
            );
            response(CoreResponse::ChatSummary(chat))
        }
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
        CoreRequest::ListProjects => {
            response(CoreResponse::ProjectSnapshot(database.list_projects()?))
        }
        CoreRequest::OpenProject { path } => {
            response(CoreResponse::ProjectSnapshot(database.open_project(&path)?))
        }
        CoreRequest::SetActiveProject { project_id } => response(CoreResponse::ProjectSnapshot(
            database.set_active_project(&project_id)?,
        )),
        CoreRequest::SidecarStatus => {
            response(CoreResponse::SidecarStatus(database.sidecar_status()?))
        }
        CoreRequest::ResolveWorkspacePath { project_id, path } => response(
            CoreResponse::ResolvedPath(resolve_workspace_path(database, &project_id, &path)?),
        ),
        // Streaming + store-backed cases handled in `handle_request` before here.
        CoreRequest::SendChatMessage { .. }
        | CoreRequest::EditChatUserMessage { .. }
        | CoreRequest::RetryChatMessage { .. }
        | CoreRequest::ContinueChatMessage { .. }
        | CoreRequest::RunToolCommand { .. }
        | CoreRequest::GetToolArtifactRange { .. }
        | CoreRequest::GetChatChangeSets { .. }
        | CoreRequest::GetMessageChangeSummary { .. }
        | CoreRequest::GetChangeFileDiff { .. }
        | CoreRequest::ListChangeSetFiles { .. }
        | CoreRequest::RevertChangeSet { .. } => {
            unreachable!("handled before the uniform request path")
        }
    })
}

/// Resolve a (workspace-relative or absolute) path against a project's root,
/// enforcing containment, and return the canonical absolute path for external
/// opening. Rejects anything outside the workspace (capability + containment).
fn resolve_workspace_path(
    database: &Database,
    project_id: &str,
    path: &str,
) -> Result<String, CoreError> {
    let root = database
        .project_root(project_id)?
        .ok_or_else(|| CoreError::new("project_not_found", "unknown project", false))?;
    let workspace = Workspace::new(&root)
        .map_err(|error| CoreError::new("workspace_unavailable", error.to_string(), false))?;
    let resolved = workspace
        .resolve(path)
        .map_err(|error| CoreError::new("path_outside_workspace", error.to_string(), false))?;
    let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
    Ok(canonical.to_string_lossy().into_owned())
}

fn run_tool_command(
    id: u64,
    request: ToolExecutionRequest,
    outbox: Outbox,
    tool_supervisor: Arc<ToolSupervisor>,
    tool_registry: Arc<ToolExecutionRegistry>,
    tool_approvals: Arc<PendingToolApprovalGate>,
    async_runtime: Arc<tokio::runtime::Runtime>,
) {
    if request.tool_call_id.trim().is_empty() {
        let _ = outbox.send(ServerFrame::Error {
            id,
            error: CoreError::new("invalid_request", "tool_call_id cannot be empty", false),
        });
        return;
    }

    let cancellation = ToolCancellationToken::default();
    if !tool_registry.register(&request.tool_call_id, cancellation.clone()) {
        let _ = outbox.send(ServerFrame::Error {
            id,
            error: CoreError::new(
                "invalid_request",
                format!("tool call already active: {}", request.tool_call_id),
                false,
            ),
        });
        return;
    }

    let tool_call_id = request.tool_call_id.clone();
    let response_result = outbox.send(ServerFrame::Response {
        id,
        result: CoreResponse::ToolExecutionAccepted(ToolExecutionAccepted {
            tool_call_id: tool_call_id.clone(),
        }),
    });
    if response_result.is_err() {
        tool_registry.finish(&tool_call_id);
        return;
    }

    let sink: Arc<dyn ToolExecutionEventSink> = Arc::new(ProtocolToolExecutionSink {
        outbox: outbox.clone(),
        database: None,
        chat_id: None,
        message_id: None,
    });
    // The orchestrator is synchronous (it blocks on the runtime for approval and
    // process I/O), so it must run on a dedicated thread, never an async-runtime
    // worker. A spawn failure or non-zero exit surfaces as the orchestrator's own
    // terminal event — no hand-rolled failure path here.
    std::thread::spawn(move || {
        // Panic isolation: the protocol reply (Accepted) already went out, so a
        // panicking orchestrator would otherwise leave the call registered
        // forever with clients watching a tool that never terminates.
        let panic_sink = Arc::clone(&sink);
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(move || {
            tool_runtime::run_command_via_orchestrator(
                request,
                cancellation,
                tool_supervisor,
                tool_approvals,
                async_runtime,
                sink,
            );
        }));
        if let Err(panic) = outcome {
            let message = panic_message(panic.as_ref());
            eprintln!("sidecar: run_command orchestrator panicked for {tool_call_id}: {message}");
            panic_sink.emit(failed_tool_event(
                &tool_call_id,
                format!("internal error: tool execution panicked: {message}"),
            ));
        }
        tool_registry.finish(&tool_call_id);
    });
}

/// A synthetic terminal `Failed` event for a tool call whose worker died
/// abnormally, so clients tracking the call see it end instead of hanging.
fn failed_tool_event(tool_call_id: &str, message: String) -> ToolExecutionEvent {
    ToolExecutionEvent {
        tool_call_id: tool_call_id.to_string(),
        run_id: None,
        project_id: None,
        command: None,
        kind: ToolExecutionEventKind::Failed,
        stream: None,
        chunk: None,
        message: Some(message.clone()),
        result: Some(ToolExecutionResult {
            tool_call_id: tool_call_id.to_string(),
            status: ToolExecutionStatus::Failed,
            exit_code: None,
            stdout_preview: String::new(),
            stderr_preview: String::new(),
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            stdout_bytes: 0,
            stderr_bytes: 0,
            truncated_for_display: false,
            truncated_for_agent: false,
            log_ref: None,
            message: Some(message),
        }),
        tool_kind: Some(ToolKind::RunCommand),
        payload: None,
        touched_paths: Vec::new(),
        artifacts: Vec::new(),
    }
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

/// Parse a chat's stored approval-mode string into the runtime enum, defaulting
/// to the safest mode (manual) for unset/unknown values.
fn parse_chat_approval_mode(value: Option<&str>) -> ToolApprovalMode {
    match value {
        Some("auto_safe") => ToolApprovalMode::AutoSafe,
        Some("yolo") => ToolApprovalMode::Yolo,
        _ => ToolApprovalMode::Manual,
    }
}

fn routed_service_tools(
    connector_manager: &ConnectorManager,
    database: &Database,
) -> Vec<ToolDescriptor> {
    let routes = database.feature_routes().unwrap_or_default();
    let image_routes = routes
        .iter()
        .filter(|route| route.feature == FEATURE_IMAGE_GENERATE)
        .collect::<Vec<_>>();
    let audio_routes = routes
        .iter()
        .filter(|route| route.feature == FEATURE_AUDIO_TRANSCRIBE)
        .collect::<Vec<_>>();
    if image_routes.is_empty() && audio_routes.is_empty() {
        return Vec::new();
    }

    for route in image_routes.iter().chain(audio_routes.iter()) {
        connector_manager.refresh_provider(&route.provider_id, |_| {});
    }
    let Ok(snapshot) = connector_manager.snapshot() else {
        return Vec::new();
    };

    let service_available = |feature: &str, provider_id: &str, model_id: &str| {
        snapshot
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .filter(|provider| provider.enabled && provider.authenticated)
            .and_then(|provider| {
                provider
                    .services
                    .iter()
                    .find(|service| service.feature == feature)
            })
            .is_some_and(|service| service.models.iter().any(|model| model.id == model_id))
    };

    let mut tools = Vec::new();
    if image_routes
        .iter()
        .any(|route| service_available(FEATURE_IMAGE_GENERATE, &route.provider_id, &route.model_id))
    {
        tools.push(image_generate_tool_descriptor());
    }
    if audio_routes.iter().any(|route| {
        service_available(
            FEATURE_AUDIO_TRANSCRIBE,
            &route.provider_id,
            &route.model_id,
        )
    }) {
        tools.push(audio_transcribe_tool_descriptor());
    }
    tools
}

/// The streaming path: given the begun run (a fresh send or a retry), answer the
/// request with the persisted placeholder, then drive the completion, forwarding
/// every run event. A failure to even begin the run is a terminal error reply.
fn run_chat_message(
    id: u64,
    started: mothership_core::Result<SendChatMessageResult>,
    database: Database,
    outbox: Outbox,
    connector_manager: Arc<ConnectorManager>,
    provider_manager: Arc<ProviderRuntimeManager>,
    tool_supervisor: Arc<ToolSupervisor>,
    tool_approvals: Arc<PendingToolApprovalGate>,
    tool_output_store: Option<Arc<dyn mothership_core::ToolOutputStore>>,
    tool_registry: Arc<ToolExecutionRegistry>,
    async_runtime: Arc<tokio::runtime::Runtime>,
    chat_registry: Arc<ChatRunRegistry>,
    change_blob_store: Arc<dyn SnapshotBlobStore>,
    tool_approval_mode: Arc<ToolApprovalModeStore>,
    tool_policy: Arc<ToolPolicyStore>,
    artifacts_root: PathBuf,
    tmp_root: PathBuf,
) {
    match started {
        Ok(started) => {
            let project = database.chat_project(&started.chat.id).ok().flatten();
            let run_tmp_root = tmp_root
                .join("runs")
                .join(app_data_path_component(&started.run_id));
            let scoped_artifact_root = Some(if let Some(project) = project.as_ref() {
                artifacts_root
                    .join("projects")
                    .join(app_data_path_component(&project.id))
                    .join("chats")
                    .join(app_data_path_component(&started.chat.id))
            } else {
                artifacts_root
                    .join("chats")
                    .join(app_data_path_component(&started.chat.id))
            });
            let _ = std::fs::create_dir_all(&run_tmp_root);
            if let Some(root) = &scoped_artifact_root {
                let _ = std::fs::create_dir_all(root);
            }
            let extra_tools = routed_service_tools(&connector_manager, &database);
            // Per-chat approval mode: register this run under ITS chat's saved
            // setting. Tool decisions for the run look up the chat's entry in
            // the shared store, so a concurrent run in another chat (e.g. one
            // set to yolo) can never loosen this chat's gating. The guard
            // releases the entry when the run reaches a terminal state (also
            // on unwind), keeping the store bounded by in-flight runs.
            let _approval_mode_run = tool_approval_mode.begin_run(
                &started.chat.id,
                parse_chat_approval_mode(started.chat.approval_mode.as_deref()),
            );
            let _ = outbox.send(ServerFrame::Response {
                id,
                result: CoreResponse::ChatMessageStarted(started.clone()),
            });
            let tool_sink: Arc<dyn ToolExecutionEventSink> = Arc::new(ProtocolToolExecutionSink {
                outbox: outbox.clone(),
                database: Some(database.clone()),
                chat_id: Some(started.chat.id.clone()),
                message_id: Some(started.assistant_message.id.clone()),
            });
            // Bind the change journal to this run: mutating tools record their
            // workspace changes through this recorder, which emits change-set
            // events onto the same outbox the chat run uses.
            let change_recorder: Option<Arc<ChangeRecorder>> = {
                let service = ChangesService::new(database.clone(), Arc::clone(&change_blob_store));
                let events: Arc<dyn ChangeEventSink> = Arc::new(ProtocolChangeEventSink {
                    outbox: outbox.clone(),
                });
                Some(Arc::new(ChangeRecorder::new(
                    service,
                    events,
                    project.as_ref().map(|project| project.id.clone()),
                    started.chat.id.clone(),
                    started.assistant_message.id.clone(),
                )))
            };
            let tool_handler: Arc<dyn LlmToolCallHandler> =
                Arc::new(tool_runtime::SidecarLlmToolHandler::new(
                    tool_supervisor,
                    tool_registry,
                    async_runtime,
                    tool_sink,
                    project.map(|project| (project.id, PathBuf::from(project.path))),
                    Some(started.chat.id.clone()),
                    tool_approvals,
                    tool_output_store,
                    change_recorder,
                    tool_approval_mode,
                    tool_policy,
                    Arc::clone(&connector_manager),
                    scoped_artifact_root,
                    Some(run_tmp_root),
                ));
            let mut sink = ProtocolChatRunSink { outbox };
            ChatRunService::new(&database, provider_manager)
                .with_tool_handler(tool_handler)
                .with_extra_tools(extra_tools)
                .run(&started, chat_registry, &mut sink);
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

struct ProtocolToolExecutionSink {
    outbox: Outbox,
    database: Option<Database>,
    chat_id: Option<String>,
    message_id: Option<String>,
}

impl ToolExecutionEventSink for ProtocolToolExecutionSink {
    fn emit(&self, event: ToolExecutionEvent) {
        // Credential firewall: scrub obvious secrets from tool output before it is
        // persisted or shown. This is the single chokepoint for everything stored
        // and pushed to the UI, so redacting here covers both.
        let event = redact_event(default_credential_guard(), &event);

        if let (Some(database), Some(chat_id), Some(message_id)) =
            (&self.database, &self.chat_id, &self.message_id)
        {
            // Legacy feed (kept for fallback) + typed storage (source of truth).
            let _ = database.record_chat_tool_execution_event(chat_id, message_id, &event);
            let _ = database.record_typed_tool_event(chat_id, message_id, &event);
        }

        let _ = self.outbox.send(ServerFrame::Event {
            event: CoreEvent::ToolExecution(event),
        });
    }
}

/// Forwards change-journal events to the host as `Event::ChangeSet` frames.
struct ProtocolChangeEventSink {
    outbox: Outbox,
}

impl ChangeEventSink for ProtocolChangeEventSink {
    fn emit(&self, event: ChangeSetEvent) {
        let _ = self.outbox.send(ServerFrame::Event {
            event: CoreEvent::ChangeSet(event),
        });
    }
}

/// Handle the change-journal requests (summaries, lazy per-file diff, revert).
/// These need the snapshot blob store — and, for revert, the project workspace —
/// so they are dispatched here rather than in the sync `compute`. A revert also
/// emits a `change_set` event so every client updates live.
fn run_change_request(
    id: u64,
    request: CoreRequest,
    database: Database,
    outbox: Outbox,
    change_blob_store: Arc<dyn SnapshotBlobStore>,
) {
    let service = ChangesService::new(database.clone(), change_blob_store);
    let result = compute_change_request(request, &service, &database);
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
            }
            response_result
        }
        Err(error) => outbox.send(ServerFrame::Error { id, error }),
    };
}

fn compute_change_request(
    request: CoreRequest,
    service: &ChangesService,
    database: &Database,
) -> Result<RequestOutcome, CoreError> {
    Ok(match request {
        CoreRequest::GetChatChangeSets { chat_id } => {
            response(CoreResponse::ChangeSets(service.chat_summaries(&chat_id)?))
        }
        CoreRequest::GetMessageChangeSummary { message_id } => response(CoreResponse::ChangeSets(
            service.message_summaries(&message_id)?,
        )),
        CoreRequest::GetChangeFileDiff {
            change_file_id,
            offset,
            limit,
            full,
        } => response(CoreResponse::ChangeFileDiff(service.file_diff(
            &change_file_id,
            offset.unwrap_or(0),
            limit.unwrap_or(0),
            full.unwrap_or(false),
        )?)),
        CoreRequest::ListChangeSetFiles {
            change_set_id,
            offset,
            limit,
        } => response(CoreResponse::ChangeFiles(service.list_change_set_files(
            &change_set_id,
            offset.unwrap_or(0),
            limit.unwrap_or(0),
        )?)),
        CoreRequest::RevertChangeSet { change_set_id } => {
            let outcome = revert_change_set(service, database, &change_set_id)?;
            let kind = if outcome.reverted {
                ChangeSetEventKind::Reverted
            } else {
                ChangeSetEventKind::Conflicted
            };
            RequestOutcome {
                response: CoreResponse::ChangeSetReverted(outcome.clone()),
                event: Some(CoreEvent::ChangeSet(ChangeSetEvent {
                    kind,
                    summary: outcome.change_set,
                })),
                connector_refresh: None,
            }
        }
        _ => unreachable!("compute_change_request only handles change requests"),
    })
}

/// Resolve the project workspace for a change set and run its conflict-aware
/// revert. Path containment is enforced by `Workspace`; the std filesystem is the
/// same one the file tools use.
fn revert_change_set(
    service: &ChangesService,
    database: &Database,
    change_set_id: &str,
) -> Result<RevertOutcome, CoreError> {
    let project_id = service
        .change_set_project_id(change_set_id)?
        .ok_or_else(|| CoreError::new("project_not_found", "change set has no project", false))?;
    let root = database.project_root(&project_id)?.ok_or_else(|| {
        CoreError::new("project_not_found", "unknown project for change set", false)
    })?;
    let workspace = Workspace::new(&root)
        .map_err(|error| CoreError::new("workspace_unavailable", error.to_string(), false))?;
    let fs = StdFileSystem::new();
    Ok(service.revert(change_set_id, &workspace, &fs)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panicking_request_handler_answers_with_internal_panic_error() {
        let (outbox, frames) = mpsc::channel::<ServerFrame>();

        handle_request_isolated(7, &outbox, || panic!("boom"));

        match frames
            .try_recv()
            .expect("an error frame for the request id")
        {
            ServerFrame::Error { id, error } => {
                assert_eq!(id, 7);
                assert_eq!(error.code, "internal_panic");
                assert!(!error.retryable);
                assert!(error.message.contains("boom"), "got: {}", error.message);
            }
            other => panic!("expected an Error frame, got {other:?}"),
        }

        // The outbox must remain usable from the catch path onwards.
        outbox
            .send(ServerFrame::Ready)
            .expect("outbox stays usable after a contained panic");
        assert!(matches!(frames.try_recv(), Ok(ServerFrame::Ready)));
    }

    #[test]
    fn successful_request_handler_emits_no_panic_frame() {
        let (outbox, frames) = mpsc::channel::<ServerFrame>();
        handle_request_isolated(8, &outbox, || {});
        assert!(frames.try_recv().is_err());
    }

    #[test]
    fn contained_panic_does_not_poison_the_approval_mode_store() {
        let (outbox, _frames) = mpsc::channel::<ServerFrame>();
        let store = ToolApprovalModeStore::new(ToolApprovalMode::Manual);

        let store_for_handler = Arc::clone(&store);
        handle_request_isolated(9, &outbox, move || {
            let _guard = store_for_handler.begin_run("chat_panic", ToolApprovalMode::Yolo);
            panic!("worker died mid-run");
        });

        // The unwound run released its chat entry, and the store still works.
        assert_eq!(
            store.mode_for_chat(Some("chat_panic")),
            ToolApprovalMode::Manual
        );
        let _run = store.begin_run("chat_after", ToolApprovalMode::AutoSafe);
        assert_eq!(
            store.mode_for_chat(Some("chat_after")),
            ToolApprovalMode::AutoSafe
        );
    }

    #[test]
    fn parse_chat_approval_mode_defaults_to_manual() {
        assert_eq!(parse_chat_approval_mode(None), ToolApprovalMode::Manual);
        assert_eq!(
            parse_chat_approval_mode(Some("bogus")),
            ToolApprovalMode::Manual
        );
        assert_eq!(
            parse_chat_approval_mode(Some("auto_safe")),
            ToolApprovalMode::AutoSafe
        );
        assert_eq!(
            parse_chat_approval_mode(Some("yolo")),
            ToolApprovalMode::Yolo
        );
    }
}
