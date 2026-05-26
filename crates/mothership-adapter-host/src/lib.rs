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

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub mod protocol;

use protocol::{AuthKind, ChatMessage, Model, ModelManagement, Outbound, Request, SettingsField};

/// A spawned adapter process and the stdio pipes to talk to it.
pub struct Adapter {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
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
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, request: &Request) -> Result<()> {
        let line = serde_json::to_string(request).context("encode request")?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Outbound> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!("adapter closed its output stream");
        }
        serde_json::from_str(line.trim()).context("decode adapter message")
    }

    /// Handshake: confirm the adapter is alive and speaks the protocol.
    pub fn initialize(&mut self) -> Result<()> {
        let id = self.next_id();
        self.send(&Request::Initialize { id })?;
        match self.recv()? {
            Outbound::Ack { id: got } if got == id => Ok(()),
            other => bail!("unexpected reply to initialize: {other:?}"),
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

    /// Runs a chat turn, invoking `on_delta` for each streamed chunk and
    /// returning the full concatenated text once the adapter signals `Done`.
    pub fn chat(
        &mut self,
        model: &str,
        messages: Vec<ChatMessage>,
        mut on_delta: impl FnMut(&str),
    ) -> Result<String> {
        let id = self.next_id();
        self.send(&Request::ChatStart {
            id,
            model: model.to_string(),
            messages,
        })?;

        let mut full = String::new();
        loop {
            match self.recv()? {
                Outbound::Delta { id: got, text } if got == id => {
                    full.push_str(&text);
                    on_delta(&text);
                }
                Outbound::Done { id: got } if got == id => return Ok(full),
                Outbound::Error { id: got, message } if got == id => {
                    bail!("adapter chat error: {message}")
                }
                other => bail!("unexpected chat event: {other:?}"),
            }
        }
    }
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
/// the adapter executable, resolved relative to the manifest's folder.
#[derive(Debug, Clone, Deserialize)]
struct ManifestFile {
    provider_id: String,
    provider_label: String,
    program: String,
}

/// A discovered adapter: its identity and the executable that implements it.
#[derive(Debug, Clone)]
pub struct AdapterEntry {
    pub provider_id: String,
    pub provider_label: String,
    pub program: PathBuf,
}

/// Adapters discovered under a plugins directory, keyed by provider id. Lets the
/// core map a provider to the executable to spawn, with one uniform contract.
#[derive(Debug, Default)]
pub struct AdapterRegistry {
    entries: Vec<AdapterEntry>,
}

impl AdapterRegistry {
    /// Scans `dir` for `<name>/adapter.json` manifests. A missing directory is an
    /// empty registry; malformed or unreadable manifests are skipped.
    pub fn scan(dir: &Path) -> Self {
        let mut entries = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(dir) {
            for entry in read_dir.flatten() {
                let folder = entry.path();
                let manifest_path = folder.join("adapter.json");
                let Ok(text) = std::fs::read_to_string(&manifest_path) else {
                    continue;
                };
                let Ok(manifest) = serde_json::from_str::<ManifestFile>(&text) else {
                    continue;
                };
                entries.push(AdapterEntry {
                    provider_id: manifest.provider_id,
                    provider_label: manifest.provider_label,
                    program: folder.join(manifest.program),
                });
            }
        }
        Self { entries }
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
