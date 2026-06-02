//! Host runtime for runtime-loadable provider adapters.
//!
//! Every adapter is a long-lived child process speaking newline-delimited
//! JSON-RPC over stdio (see [`protocol`]). The same contract covers a normal
//! HTTP provider (the adapter process *is* the adapter) and one that drives an
//! external CLI like Claude Code (the adapter process spawns and supervises that
//! CLI and bridges it into the same stream) — the core treats them identically.
//!
//! A crashing adapter cannot take down the app: it lives in its own process and
//! its failure surfaces as an error on the next read.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

// The protocol now lives in its own crate so the adapter SDK can depend on it
// without pulling in this host runtime. Re-exported as `protocol` so existing
// `mothership_adapter_host::protocol::*` paths keep working unchanged.
pub use mothership_adapter_protocol as protocol;

use protocol::{
    AuthKind, AuthStatus, ChatMessage, Model, ModelManagement, Outbound, PromptBundle, Request,
    RuntimeContext, SettingsField, ToolCallInvocation, ToolCallResponse, ToolDescriptor,
};

/// Handler the host registers to persist secrets an adapter pushes via the
/// `StoreSecret` side channel (e.g. a freshly minted/refreshed OAuth token).
type StoreSecretSink = Box<dyn FnMut(BTreeMap<String, String>) + Send>;

const CHAT_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct AdapterChatRound {
    pub text: String,
    pub state: Option<serde_json::Value>,
    pub tool_calls: Vec<ToolCallInvocation>,
}

/// A spawned adapter process and the stdio pipes to talk to it.
pub struct Adapter {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    reader: BufReader<ChildStdout>,
    next_id: u64,
    store_secret_sink: Option<StoreSecretSink>,
}

impl Adapter {
    /// Spawns `program` as an adapter process wired for stdio messaging.
    pub fn spawn(program: &Path) -> Result<Self> {
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn adapter {}", program.display()))?;
        let stdin = child.stdin.take().context("adapter has no stdin")?;
        let stdout = child.stdout.take().context("adapter has no stdout")?;
        Ok(Self {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            reader: BufReader::new(stdout),
            next_id: 1,
            store_secret_sink: None,
        })
    }

    /// Registers a handler invoked whenever the adapter pushes a `StoreSecret`
    /// side-channel message (e.g. an OAuth token it just obtained or refreshed).
    /// The host wires this to persist the values in the shared credential vault.
    /// It is handled transparently inside [`recv`](Self::recv), so it can fire at
    /// any point during a request/response exchange — including mid-chat.
    pub fn set_store_secret_handler(
        &mut self,
        handler: impl FnMut(BTreeMap<String, String>) + Send + 'static,
    ) {
        self.store_secret_sink = Some(Box::new(handler));
    }

    /// The OS process id of the spawned adapter. Lets the host supervise it
    /// externally — e.g. cancel a long-running `authenticate` flow by killing
    /// the process (generic across adapters; nothing provider-specific).
    pub fn process_id(&self) -> u32 {
        self.child.id()
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, request: &Request) -> Result<()> {
        write_request(&self.stdin, request)
    }

    /// Reads the next protocol message. `StoreSecret` is consumed transparently
    /// (forwarded to the registered handler) and never surfaces to callers, so
    /// the adapter can persist credentials at any moment without disturbing the
    /// request/response flow.
    fn recv(&mut self) -> Result<Outbound> {
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                bail!("adapter closed its output stream");
            }
            let message: Outbound =
                serde_json::from_str(line.trim()).context("decode adapter message")?;
            if let Outbound::StoreSecret { values } = message {
                if let Some(sink) = self.store_secret_sink.as_mut() {
                    sink(values);
                }
                continue;
            }
            return Ok(message);
        }
    }

    /// Handshake: confirm the adapter is alive and speaks a compatible protocol
    /// version. Refuses (rather than mis-parsing) an adapter on a different
    /// version, including an older one that still replies with a bare `Ack`.
    pub fn initialize(&mut self) -> Result<()> {
        let id = self.next_id();
        self.send(&Request::Initialize {
            id,
            protocol_version: protocol::PROTOCOL_VERSION,
        })?;
        match self.recv()? {
            Outbound::Initialized {
                id: got,
                protocol_version,
            } if got == id => {
                if protocol_version == protocol::PROTOCOL_VERSION {
                    Ok(())
                } else {
                    bail!(
                        "adapter protocol version {protocol_version} is incompatible with host version {}",
                        protocol::PROTOCOL_VERSION
                    )
                }
            }
            other => bail!("unexpected reply to initialize (adapter may be too old): {other:?}"),
        }
    }

    /// Reads the adapter's identity (provider id + human label).
    pub fn identity(&mut self) -> Result<(String, String)> {
        let id = self.next_id();
        self.send(&Request::GetIdentity { id })?;
        match self.recv()? {
            Outbound::Identity {
                provider_id,
                provider_label,
                ..
            } => Ok((provider_id, provider_label)),
            other => bail!("unexpected reply to get_identity: {other:?}"),
        }
    }

    /// Reads the adapter's advertised models.
    /// Reads the adapter's models and how its list is managed.
    pub fn models(&mut self) -> Result<(Vec<Model>, ModelManagement)> {
        let id = self.next_id();
        self.send(&Request::GetModels { id })?;
        match self.recv()? {
            Outbound::Models {
                models, management, ..
            } => Ok((models, management)),
            other => bail!("unexpected reply to get_models: {other:?}"),
        }
    }

    /// Reads the settings fields the adapter wants the UI to render.
    pub fn settings_schema(&mut self) -> Result<Vec<SettingsField>> {
        let id = self.next_id();
        self.send(&Request::GetSettingsSchema { id })?;
        match self.recv()? {
            Outbound::SettingsSchema { fields, .. } => Ok(fields),
            other => bail!("unexpected reply to get_settings_schema: {other:?}"),
        }
    }

    /// Pushes the current settings values to the adapter (host resends them on
    /// each spawn since adapter processes are short-lived).
    pub fn set_settings(&mut self, values: BTreeMap<String, String>) -> Result<()> {
        let id = self.next_id();
        self.send(&Request::SetSettings { id, values })?;
        match self.recv()? {
            Outbound::Ack { id: got } if got == id => Ok(()),
            other => bail!("unexpected reply to set_settings: {other:?}"),
        }
    }

    /// Reads the adapter's auth scheme (the host stores API-key secrets in its
    /// vault; oauth / external-process flows are owned by the adapter).
    pub fn auth_schema(&mut self) -> Result<AuthKind> {
        let id = self.next_id();
        self.send(&Request::GetAuthSchema { id })?;
        match self.recv()? {
            Outbound::AuthSchema { auth, .. } => Ok(auth),
            other => bail!("unexpected reply to get_auth_schema: {other:?}"),
        }
    }

    pub fn auth_status(&mut self) -> Result<AuthStatus> {
        let id = self.next_id();
        self.send(&Request::GetAuthStatus { id })?;
        match self.recv()? {
            Outbound::AuthStatus { status, .. } => Ok(status),
            other => bail!("unexpected reply to get_auth_status: {other:?}"),
        }
    }

    /// Asks the adapter to run its own auth flow now (e.g. browser OAuth) and
    /// returns once it acks completion. The adapter persists any resulting
    /// credential via `StoreSecret`, which `recv` forwards to the registered
    /// handler transparently — so wire `set_store_secret_handler` first.
    pub fn authenticate(&mut self) -> Result<()> {
        let id = self.next_id();
        self.send(&Request::Authenticate { id })?;
        match self.recv()? {
            Outbound::Ack { id: got } if got == id => Ok(()),
            Outbound::Error { id: got, message } if got == id => {
                bail!("adapter authenticate failed: {message}")
            }
            other => bail!("unexpected reply to authenticate: {other:?}"),
        }
    }

    /// Asks the adapter to revoke / clean up its current auth before the host
    /// forgets the credential. Best-effort on the adapter side; the host still
    /// proceeds with local logout regardless.
    pub fn logout(&mut self) -> Result<()> {
        let id = self.next_id();
        self.send(&Request::Logout { id })?;
        match self.recv()? {
            Outbound::Ack { id: got } if got == id => Ok(()),
            other => bail!("unexpected reply to logout: {other:?}"),
        }
    }

    /// Runs a single provider model round. The adapter may stream visible text
    /// and then returns an opaque continuation state plus any model-requested
    /// tool calls. Core owns whether and how to execute those tools.
    #[allow(clippy::too_many_arguments)]
    pub fn chat_round_cancellable(
        &mut self,
        model: &str,
        reasoning: Option<protocol::ReasoningConfig>,
        prompt: PromptBundle,
        runtime_context: RuntimeContext,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDescriptor>,
        state: Option<serde_json::Value>,
        tool_results: Vec<ToolCallResponse>,
        extra_messages: Vec<ChatMessage>,
        is_cancelled: impl Fn() -> bool + Send + 'static,
        mut on_delta: impl FnMut(&str),
    ) -> Result<AdapterChatRound> {
        let id = self.next_id();
        self.send(&Request::ChatStart {
            id,
            model: model.to_string(),
            reasoning,
            prompt,
            runtime_context,
            messages,
            tools,
            state,
            tool_results,
            extra_messages,
        })?;

        let stdin = Arc::clone(&self.stdin);
        let finished = Arc::new(AtomicBool::new(false));
        let watcher_finished = Arc::clone(&finished);
        let watcher = thread::spawn(move || {
            while !watcher_finished.load(Ordering::SeqCst) {
                if is_cancelled() {
                    let _ = write_request(&stdin, &Request::ChatCancel { id });
                    return;
                }
                thread::sleep(CHAT_CANCEL_POLL_INTERVAL);
            }
        });

        let mut full = String::new();
        let result = loop {
            match self.recv() {
                Ok(Outbound::Delta { id: got, text }) if got == id => {
                    full.push_str(&text);
                    on_delta(&text);
                }
                Ok(Outbound::ChatRoundComplete {
                    id: got,
                    state,
                    tool_calls,
                }) if got == id => {
                    break Ok(AdapterChatRound {
                        text: full,
                        state,
                        tool_calls,
                    })
                }
                Ok(Outbound::Error { id: got, message }) if got == id => {
                    break Err(anyhow::anyhow!("adapter chat error: {message}"))
                }
                Ok(other) => break Err(anyhow::anyhow!("unexpected chat event: {other:?}")),
                Err(error) => break Err(error),
            }
        };
        finished.store(true, Ordering::SeqCst);
        let _ = watcher.join();
        result
    }
}

fn write_request(stdin: &Arc<Mutex<ChildStdin>>, request: &Request) -> Result<()> {
    let line = serde_json::to_string(request).context("encode request")?;
    let mut stdin = stdin.lock().unwrap();
    stdin.write_all(line.as_bytes())?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

impl Drop for Adapter {
    fn drop(&mut self) {
        // Best-effort: terminate the child so a dropped adapter doesn't leak a
        // process. A real supervisor would send a graceful shutdown first.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// On-disk adapter manifest: `<plugins-dir>/<name>/adapter.json`. `program` is
/// the adapter executable and `icon` an optional image file, both resolved
/// relative to the manifest's folder.
#[derive(Debug, Clone, Deserialize)]
struct ManifestFile {
    provider_id: String,
    provider_label: String,
    program: String,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    integrity: Option<AdapterIntegrity>,
    #[serde(default)]
    capabilities: BTreeSet<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterIntegrity {
    pub algorithm: String,
    pub sha256: String,
}

/// A discovered adapter: its identity, the executable that implements it, and an
/// optional icon file (the adapter ships its own branding). Settings (including
/// secrets) live in the app's shared credential vault, keyed by `provider_id`,
/// not next to the adapter on disk.
#[derive(Debug, Clone)]
pub struct AdapterEntry {
    pub provider_id: String,
    pub provider_label: String,
    pub program: PathBuf,
    pub icon: Option<PathBuf>,
    pub integrity: Option<AdapterIntegrity>,
    pub capabilities: BTreeSet<String>,
}

impl AdapterEntry {
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }

    pub fn capabilities(&self) -> &BTreeSet<String> {
        &self.capabilities
    }

    /// Computes the executable SHA-256 and verifies the manifest-declared hash
    /// when one is present. Core calls this immediately before spawning.
    pub fn verify_program_integrity(&self) -> Result<String> {
        let actual = file_sha256_hex(&self.program)?;
        if let Some(integrity) = &self.integrity {
            if !integrity.algorithm.eq_ignore_ascii_case("sha256") {
                bail!(
                    "unsupported integrity algorithm for {}: {}",
                    self.provider_id,
                    integrity.algorithm
                );
            }
            if !actual.eq_ignore_ascii_case(integrity.sha256.trim()) {
                bail!(
                    "adapter integrity mismatch for {}: expected {}, got {}",
                    self.provider_id,
                    integrity.sha256,
                    actual
                );
            }
        }
        Ok(actual)
    }
}

#[derive(Debug, Clone)]
pub struct AdapterDiagnostic {
    pub provider_id: Option<String>,
    pub provider_label: Option<String>,
    pub manifest_path: PathBuf,
    pub message: String,
}

/// Adapters discovered under a plugins directory, keyed by provider id. Lets the
/// core map a provider to the executable to spawn, with one uniform contract.
#[derive(Debug, Default)]
pub struct AdapterRegistry {
    entries: Vec<AdapterEntry>,
    diagnostics: Vec<AdapterDiagnostic>,
}

impl AdapterRegistry {
    /// Scans `dir` for `<name>/adapter.json` manifests. A missing directory is an
    /// empty registry; malformed or unreadable manifests are skipped.
    pub fn scan(dir: &Path) -> Self {
        let mut entries = Vec::new();
        let mut diagnostics = Vec::new();
        let mut seen_provider_ids = BTreeSet::new();
        if let Ok(read_dir) = std::fs::read_dir(dir) {
            let mut folders = read_dir
                .flatten()
                .map(|entry| entry.path())
                .collect::<Vec<_>>();
            folders.sort();

            for folder in folders {
                if !folder.is_dir() {
                    continue;
                }
                let manifest_path = folder.join("adapter.json");
                let Ok(text) = std::fs::read_to_string(&manifest_path) else {
                    continue;
                };
                let Ok(manifest) = serde_json::from_str::<ManifestFile>(&text) else {
                    push_diagnostic(
                        &mut diagnostics,
                        &manifest_path,
                        None,
                        None,
                        "malformed adapter manifest",
                    );
                    continue;
                };
                if !valid_provider_id(&manifest.provider_id) {
                    push_diagnostic(
                        &mut diagnostics,
                        &manifest_path,
                        Some(manifest.provider_id),
                        Some(manifest.provider_label),
                        "invalid provider id",
                    );
                    continue;
                }
                if folder.file_name().and_then(|name| name.to_str())
                    != Some(manifest.provider_id.as_str())
                {
                    push_diagnostic(
                        &mut diagnostics,
                        &manifest_path,
                        Some(manifest.provider_id),
                        Some(manifest.provider_label),
                        "adapter folder does not match provider id",
                    );
                    continue;
                }
                if !seen_provider_ids.insert(manifest.provider_id.to_ascii_lowercase()) {
                    push_diagnostic(
                        &mut diagnostics,
                        &manifest_path,
                        Some(manifest.provider_id),
                        Some(manifest.provider_label),
                        "duplicate adapter provider id",
                    );
                    continue;
                }
                if manifest.provider_label.trim().is_empty() {
                    push_diagnostic(
                        &mut diagnostics,
                        &manifest_path,
                        Some(manifest.provider_id),
                        Some(manifest.provider_label),
                        "empty provider label",
                    );
                    continue;
                }

                let program = resolve_program_path(&folder, &manifest.program);
                if !program.is_file() {
                    push_diagnostic(
                        &mut diagnostics,
                        &manifest_path,
                        Some(manifest.provider_id),
                        Some(manifest.provider_label),
                        "adapter executable is missing",
                    );
                    continue;
                }
                let icon = manifest
                    .icon
                    .as_deref()
                    .and_then(|icon| resolve_icon_path(&folder, icon));

                entries.push(AdapterEntry {
                    provider_id: manifest.provider_id,
                    provider_label: manifest.provider_label,
                    program,
                    icon,
                    integrity: manifest.integrity,
                    capabilities: manifest.capabilities,
                });
            }
        }
        Self {
            entries,
            diagnostics,
        }
    }

    pub fn diagnostics(&self) -> &[AdapterDiagnostic] {
        &self.diagnostics
    }

    pub fn find(&self, provider_id: &str) -> Option<&AdapterEntry> {
        self.entries
            .iter()
            .find(|entry| entry.provider_id == provider_id)
    }

    pub fn entries(&self) -> &[AdapterEntry] {
        &self.entries
    }
}

fn push_diagnostic(
    diagnostics: &mut Vec<AdapterDiagnostic>,
    manifest_path: &Path,
    provider_id: Option<String>,
    provider_label: Option<String>,
    message: impl Into<String>,
) {
    let diagnostic = AdapterDiagnostic {
        provider_id,
        provider_label,
        manifest_path: manifest_path.to_path_buf(),
        message: message.into(),
    };
    if let Some(provider_id) = diagnostic.provider_id.as_deref() {
        eprintln!(
            "skipping adapter `{}`: {} ({})",
            provider_id,
            diagnostic.message,
            diagnostic.manifest_path.display()
        );
    } else {
        eprintln!(
            "skipping adapter manifest: {} ({})",
            diagnostic.message,
            diagnostic.manifest_path.display()
        );
    }
    diagnostics.push(diagnostic);
}

fn valid_provider_id(provider_id: &str) -> bool {
    !provider_id.is_empty()
        && provider_id.chars().all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_' | '.')
        })
}

fn resolve_program_path(folder: &Path, program: &str) -> PathBuf {
    let program = PathBuf::from(program);
    if program.is_absolute() {
        program
    } else {
        folder.join(program)
    }
}

fn resolve_icon_path(folder: &Path, icon: &str) -> Option<PathBuf> {
    let icon_path = folder.join(icon);
    let Ok(folder) = folder.canonicalize() else {
        return None;
    };
    let Ok(icon_path) = icon_path.canonicalize() else {
        return None;
    };
    icon_path.starts_with(folder).then_some(icon_path)
}

fn file_sha256_hex(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open adapter executable {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read adapter executable {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("mothership_adapter_host_{name}_{stamp}"))
    }

    fn write_manifest(root: &Path, provider_id: &str, program: &str, extra: serde_json::Value) {
        let dir = root.join(provider_id);
        fs::create_dir_all(&dir).expect("create provider dir");
        let mut manifest = serde_json::json!({
            "provider_id": provider_id,
            "provider_label": "Test",
            "program": program,
        });
        let object = manifest.as_object_mut().expect("manifest object");
        for (key, value) in extra.as_object().expect("extra object") {
            object.insert(key.clone(), value.clone());
        }
        fs::write(dir.join("adapter.json"), manifest.to_string()).expect("write manifest");
    }

    #[test]
    fn registry_keeps_capabilities_and_verifies_integrity() {
        let root = temp_dir("valid");
        let program = root.join("adapter.exe");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&program, b"adapter bytes").expect("write program");
        let sha256 = file_sha256_hex(&program).expect("hash");
        write_manifest(
            &root,
            "test",
            program.to_str().expect("program path"),
            serde_json::json!({
                "capabilities": ["llm.chat", "llm.models"],
                "integrity": { "algorithm": "sha256", "sha256": sha256 },
            }),
        );

        let registry = AdapterRegistry::scan(&root);
        assert!(registry.diagnostics().is_empty());
        let entry = registry.find("test").expect("entry");
        assert!(entry.has_capability("llm.chat"));
        assert!(entry.verify_program_integrity().is_ok());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_reports_bad_manifest_and_duplicate_provider() {
        let root = temp_dir("diagnostics");
        fs::create_dir_all(root.join("bad")).expect("create bad");
        fs::write(root.join("bad").join("adapter.json"), "{not-json").expect("write bad");

        let program = root.join("adapter.exe");
        fs::write(&program, b"adapter bytes").expect("write program");
        write_manifest(
            &root,
            "first",
            program.to_str().expect("program path"),
            serde_json::json!({}),
        );
        fs::create_dir_all(root.join("second")).expect("create second");
        fs::write(
            root.join("second").join("adapter.json"),
            serde_json::json!({
                "provider_id": "first",
                "provider_label": "Duplicate",
                "program": program,
            })
            .to_string(),
        )
        .expect("write duplicate");

        let registry = AdapterRegistry::scan(&root);
        assert_eq!(registry.entries().len(), 1);
        assert!(registry.diagnostics().len() >= 2);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn integrity_mismatch_is_rejected_before_spawn() {
        let root = temp_dir("mismatch");
        let program = root.join("adapter.exe");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&program, b"adapter bytes").expect("write program");
        write_manifest(
            &root,
            "test",
            program.to_str().expect("program path"),
            serde_json::json!({
                "integrity": {
                    "algorithm": "sha256",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                },
            }),
        );

        let registry = AdapterRegistry::scan(&root);
        let entry = registry.find("test").expect("entry");
        assert!(entry.verify_program_integrity().is_err());

        let _ = fs::remove_dir_all(root);
    }
}
