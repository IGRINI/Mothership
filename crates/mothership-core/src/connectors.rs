//! Connector (provider-adapter) application logic.
//!
//! Everything the UI needs to render and drive the Connectors settings —
//! enumerating installed adapters, the models they advertise, their settings
//! form, auth scheme/status, plus the save / select-model / authorize / cancel /
//! logout actions — lives here in Core. The host (and any future client) just
//! asks Core for a [`ConnectorSettingsSnapshot`] and issues commands; it owns no
//! adapter-spawning or vault logic of its own.
//!
//! Adapters are spawned through the `mothership-adapter-host` port. Secrets and
//! settings live in the app's shared [`FileCredentialVault`], keyed by provider —
//! never next to the adapter on disk.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use mothership_adapter_host::protocol::{
    AuthKind, AuthStatus, AuthStatusKind, ModelManagement, SettingsField, SettingsFieldKind,
};
use mothership_adapter_host::{AdapterEntry, AdapterRegistry};

use crate::adapter_pool::{
    spawn_ready_adapter, spawn_ready_adapter_without_secret_sink, AdapterPool,
};
use crate::auth::FileCredentialVault;
use crate::llm::{
    ConnectorModelManagementKind, ConnectorModelManagementSchema, ConnectorSettingsSchema,
    LlmModel, ProviderRuntimeKind, SelectedLlmModel,
};
use crate::provider_runtime::{ProviderRuntimeManager, ProviderRuntimeStatus};
use crate::{Database, MothershipError, Result};

const CODEX_ADAPTER_SHA256: Option<&str> = option_env!("MOTHERSHIP_BUILTIN_CODEX_ADAPTER_SHA256");
const OPENROUTER_ADAPTER_SHA256: Option<&str> =
    option_env!("MOTHERSHIP_BUILTIN_OPENROUTER_ADAPTER_SHA256");
const CLAUDE_AGENT_ADAPTER_SHA256: Option<&str> =
    option_env!("MOTHERSHIP_BUILTIN_CLAUDE_AGENT_ADAPTER_SHA256");

pub(crate) const CAPABILITY_LLM_MODELS: &str = "llm.models";
pub(crate) const CAPABILITY_LLM_CHAT: &str = "llm.chat";
pub(crate) const CAPABILITY_AGENT_RUNTIME: &str = "agent.runtime";
const CAPABILITY_SETTINGS_READ: &str = "settings.read";
const CAPABILITY_SETTINGS_WRITE: &str = "settings.write";
const CAPABILITY_AUTH_INTERACTIVE: &str = "auth.interactive";
const CAPABILITY_AUTH_LOGOUT: &str = "auth.logout";
const MODEL_CATALOG_CACHE_TTL: Duration = Duration::from_secs(300);

/// Tracks in-flight `authenticate` flows by provider id -> adapter process id, so
/// a [`cancel_authenticate`] (or the client leaving Settings) can terminate one.
/// Owned by whoever runs the auth flows (the sidecar); passed into
/// [`ConnectorManager::authenticate`] / [`ConnectorManager::cancel_authenticate`].
pub type AuthProcessRegistry = Mutex<HashMap<String, u32>>;

pub fn trusted_built_in_adapter_sha256(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "codex" => CODEX_ADAPTER_SHA256,
        "openrouter" => OPENROUTER_ADAPTER_SHA256,
        "claude-agent" => CLAUDE_AGENT_ADAPTER_SHA256,
        _ => None,
    }
}

/// The full Connectors view plus the active model selection. Serialized straight
/// to the UI, so the field casing is the UI contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorSettingsSnapshot {
    pub providers: Vec<ConnectorProviderSummary>,
    pub selected_model: SelectedLlmModel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorSettingsEvent {
    pub kind: ConnectorSettingsEventKind,
    pub snapshot: ConnectorSettingsSnapshot,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorSettingsEventKind {
    RefreshStarted,
    ProviderUpdated,
    SelectedModelChanged,
    AdapterSettingsSaved,
    AuthenticationFinished,
    AuthenticationCancelled,
    LoggedOut,
    ProviderEnabledChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorProviderSummary {
    pub id: String,
    pub label: String,
    /// User-controlled on/off switch. Disabled providers are persisted (default
    /// on) and hidden from model selection in chat, while still showing in the
    /// Connectors screen so they can be re-enabled or configured.
    pub enabled: bool,
    pub runtime_kind: ProviderRuntimeKind,
    /// The adapter's own icon as a data URI, if it ships one (declared in its
    /// manifest). Core just renders whatever the plugin provides.
    pub icon: Option<String>,
    pub settings_schema: ConnectorSettingsSchema,
    pub models: Vec<LlmModel>,
    pub model_error: Option<String>,
    pub refresh_status: ConnectorRefreshStatus,
    /// Whether the adapter has a resident process already initialized in the
    /// pool. `false` means the next provider-backed operation may pay a warm-up
    /// cost even if the model catalog is already ready.
    pub runtime_ready: bool,
    pub runtime_status: ProviderRuntimeStatus,
    pub selected_model_id: Option<String>,
    /// The adapter's auth scheme: `none` / `api_key` / `oauth_internal` /
    /// `external_process`. Drives whether the UI shows an "Authorize" button.
    pub auth_kind: String,
    /// Provider-agnostic auth state reported by the adapter after Core has
    /// pushed the current vault settings into it.
    pub auth_status: AuthStatus,
    /// Convenience boolean for UI actions derived from [`auth_status`].
    pub authenticated: bool,
    /// Present for subprocess adapters: the settings fields they declare plus
    /// their current values, so the UI can render and save a config form.
    pub adapter_settings: Option<AdapterSettingsView>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorRefreshStatus {
    Pending,
    Refreshing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterSettingsView {
    pub fields: Vec<AdapterSettingsFieldView>,
    /// Non-secret current values only. Secret values are write-only and exposed
    /// via `secrets` as redacted status metadata.
    pub values: BTreeMap<String, String>,
    pub secrets: BTreeMap<String, SecretSettingState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretSettingState {
    pub has_value: bool,
    pub fingerprint: Option<String>,
    pub last4: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterSettingsFieldView {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub required: bool,
    pub options: Vec<AdapterSettingsFieldOptionView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterSettingsFieldOptionView {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// Per-adapter UI info gathered from one spawn: the settings form (declared
/// fields + current values from the vault), the adapter's auth scheme, and
/// whether it currently has a stored credential (drives Authorize vs Log out).
#[derive(Clone)]
struct AdapterInfo {
    view: AdapterSettingsView,
    auth_kind: String,
    auth_status: AuthStatus,
    authenticated: bool,
}

#[derive(Clone)]
struct AdapterModelCatalog {
    models: Vec<LlmModel>,
    management: ModelManagement,
    error: Option<String>,
}

#[derive(Default)]
struct ConnectorManagerState {
    catalogs: BTreeMap<String, AdapterModelCatalog>,
    catalog_fetched_at: BTreeMap<String, Instant>,
    adapter_info: BTreeMap<String, AdapterInfo>,
    refreshing: BTreeSet<String>,
}

/// Owns the live connector catalog for the sidecar process.
///
/// Read operations return the last known state immediately. Provider calls
/// (models/settings/auth schema) are refreshed in background jobs and published
/// as connector events, so UI clients never have to wait for a slow provider just
/// to render the current screen.
pub struct ConnectorManager {
    database: Database,
    pool: Arc<AdapterPool>,
    runtime: Option<Arc<ProviderRuntimeManager>>,
    state: Mutex<ConnectorManagerState>,
}

impl ConnectorManager {
    pub fn new(database: Database, pool: Arc<AdapterPool>) -> Self {
        Self {
            database,
            pool,
            runtime: None,
            state: Mutex::new(ConnectorManagerState::default()),
        }
    }

    pub fn with_runtime(
        database: Database,
        pool: Arc<AdapterPool>,
        runtime: Arc<ProviderRuntimeManager>,
    ) -> Self {
        Self {
            database,
            pool,
            runtime: Some(runtime),
            state: Mutex::new(ConnectorManagerState::default()),
        }
    }

    pub fn snapshot(&self) -> Result<ConnectorSettingsSnapshot> {
        let selected_model = self.database.selected_llm_model()?;
        let disabled = self.database.disabled_provider_ids()?;
        let provider_labels = self.provider_labels();
        let provider_icons = self.provider_icons();
        let provider_runtime_kinds = self.provider_runtime_kinds();
        let provider_errors = self.provider_errors();
        let state = self.state.lock().unwrap();

        Ok(ConnectorSettingsSnapshot {
            providers: connector_providers(
                &state.catalogs,
                &selected_model,
                &disabled,
                &state.adapter_info,
                &provider_labels,
                &provider_icons,
                &provider_runtime_kinds,
                &state.refreshing,
                &provider_errors,
                &self.pool,
                self.runtime.as_deref(),
            ),
            selected_model,
        })
    }

    /// Toggle a provider on or off. Validates the id exists among installed
    /// adapters (so the UI can't disable a phantom provider), persists the flag,
    /// and returns the refreshed snapshot. No catalog refresh is needed — the
    /// adapter's models and auth are unchanged, only its availability.
    pub fn set_provider_enabled(
        &self,
        provider_id: &str,
        enabled: bool,
    ) -> Result<ConnectorSettingsSnapshot> {
        find_trusted_adapter_entry(&self.plugins_dir(), provider_id)?;
        self.database.set_provider_enabled(provider_id, enabled)?;
        // Switching a provider off frees its resident process; the next run after
        // a re-enable just pays a one-time warm-up.
        if !enabled {
            self.pool.force_evict(provider_id);
        }
        self.snapshot()
    }

    /// Validates that a provider/model exists among the installed adapters,
    /// WITHOUT mutating any persisted selection. Used by `set_selected_model`
    /// (global default) and by per-chat model changes (which must NOT touch the
    /// global default).
    pub fn ensure_model_supported(&self, provider_id: &str, model_id: &str) -> Result<()> {
        let support = {
            let state = self.state.lock().unwrap();
            state.catalogs.get(provider_id).map(|catalog| {
                let catalog_match = catalog.models.iter().any(|model| model.id == model_id);
                let custom_allowed = model_management_accepts_custom_ids(catalog.management);
                (catalog_match, custom_allowed)
            })
        };

        match support {
            Some((true, _)) => Ok(()),
            Some((false, true)) if is_valid_custom_model_id(model_id) => Ok(()),
            Some((false, true)) => Err(MothershipError::InvalidRequest(format!(
                "invalid custom model id: {model_id}"
            ))),
            _ => Err(MothershipError::InvalidRequest(format!(
                "unsupported model: {provider_id}/{model_id}"
            ))),
        }
    }

    pub fn set_selected_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<ConnectorSettingsSnapshot> {
        self.ensure_model_supported(provider_id, model_id)?;
        self.database
            .set_selected_llm_model(provider_id, model_id)?;
        self.snapshot()
    }

    pub fn save_adapter_settings(
        &self,
        provider_id: &str,
        patch: BTreeMap<String, AdapterSettingPatchValue>,
    ) -> Result<ConnectorSettingsSnapshot> {
        self.save_adapter_settings_only(provider_id, patch)?;
        self.invalidate_provider(provider_id);
        self.snapshot()
    }

    pub fn authenticate(
        &self,
        provider_id: &str,
        registry: &AuthProcessRegistry,
    ) -> Result<ConnectorSettingsSnapshot> {
        self.authenticate_only(provider_id, registry)?;
        self.invalidate_provider(provider_id);
        self.snapshot()
    }

    pub fn cancel_authenticate(
        &self,
        provider_id: &str,
        registry: &AuthProcessRegistry,
    ) -> Result<ConnectorSettingsSnapshot> {
        self.cancel_authenticate_only(provider_id, registry)?;
        self.snapshot()
    }

    pub fn logout(&self, provider_id: &str) -> Result<ConnectorSettingsSnapshot> {
        self.logout_only(provider_id)?;
        self.invalidate_provider(provider_id);
        self.snapshot()
    }

    fn save_adapter_settings_only(
        &self,
        provider_id: &str,
        patch: BTreeMap<String, AdapterSettingPatchValue>,
    ) -> Result<()> {
        let entry = find_trusted_adapter_entry(&self.plugins_dir(), provider_id)?;
        ensure_adapter_capability(&entry, CAPABILITY_SETTINGS_WRITE, "save settings")
            .map_err(MothershipError::InvalidRequest)?;

        let vault = self.vault();
        let fields = adapter_settings_fields(&self.pool, &entry, &vault).map_err(|error| {
            MothershipError::InvalidRequest(format!(
                "failed to read settings schema for {provider_id}: {error}"
            ))
        })?;
        let field_kinds = fields
            .iter()
            .map(|field| (field.key.clone(), field.kind))
            .collect::<BTreeMap<_, _>>();
        for key in patch.keys() {
            if !field_kinds.contains_key(key) {
                return Err(MothershipError::InvalidRequest(format!(
                    "unsupported adapter setting for {provider_id}: {key}"
                )));
            }
        }

        let mut values = vault.load_adapter_settings(provider_id)?;
        let mut secret_changed = false;
        for (key, change) in patch {
            let is_secret = matches!(field_kinds.get(&key), Some(SettingsFieldKind::Secret));
            match change {
                AdapterSettingPatchValue::Set { value } => {
                    if is_secret {
                        secret_changed = true;
                    }
                    values.insert(key, value);
                }
                AdapterSettingPatchValue::Clear => {
                    if is_secret {
                        secret_changed = true;
                    }
                    values.remove(&key);
                }
                AdapterSettingPatchValue::Unchanged => {}
            }
        }

        vault.save_adapter_settings(provider_id, &values)?;
        if secret_changed {
            self.pool.force_evict(provider_id);
        }
        Ok(())
    }

    /// Replaces the stored settings map for tests and adapter-owned internals.
    /// UI code should use [`save_adapter_settings`](Self::save_adapter_settings)
    /// so keys are validated against the adapter schema.
    pub fn save_adapter_settings_snapshot(
        &self,
        provider_id: &str,
        values: &BTreeMap<String, String>,
    ) -> Result<()> {
        let entry = find_trusted_adapter_entry(&self.plugins_dir(), provider_id)?;
        ensure_adapter_capability(&entry, CAPABILITY_SETTINGS_WRITE, "save settings")
            .map_err(MothershipError::InvalidRequest)?;
        self.vault().save_adapter_settings(provider_id, values)
    }

    fn logout_only(&self, provider_id: &str) -> Result<()> {
        let vault = self.vault();
        if let Ok(entry) = find_trusted_adapter_entry(&self.plugins_dir(), provider_id) {
            let can_logout =
                ensure_adapter_capability(&entry, CAPABILITY_AUTH_LOGOUT, "log out").is_ok();
            let stored_settings = vault.load_adapter_settings(provider_id)?;
            // First kill any resident instance so a busy adapter cannot keep using
            // the old credential after logout. Then remove the vault entry before
            // best-effort revoke so concurrent spawns cannot read the old secret.
            self.pool.force_evict(provider_id);
            vault.delete_adapter_settings(provider_id)?;
            if can_logout {
                let _ = adapter_logout_once(&entry, stored_settings);
            }
        } else {
            vault.delete_adapter_settings(provider_id)?;
        }
        Ok(())
    }

    fn authenticate_only(&self, provider_id: &str, registry: &AuthProcessRegistry) -> Result<()> {
        let entry = find_trusted_adapter_entry(&self.plugins_dir(), provider_id)?;
        ensure_adapter_capability(&entry, CAPABILITY_AUTH_INTERACTIVE, "authenticate")
            .map_err(MothershipError::InvalidRequest)?;

        let vault = self.vault();
        let mut session = spawn_ready_adapter(&entry, &vault)
            .map_err(|error| MothershipError::InvalidRequest(error.to_string()))?;

        if let Ok(mut map) = registry.lock() {
            map.insert(provider_id.to_string(), session.adapter.process_id());
        }
        let result = session.adapter.authenticate();
        // If our registration is already gone, a cancel took it and killed us —
        // treat that as a clean (not error) outcome.
        let cancelled = registry
            .lock()
            .ok()
            .and_then(|mut map| map.remove(provider_id))
            .is_none();
        drop(session);

        match result {
            Ok(()) => Ok(()),
            Err(_) if cancelled => Ok(()),
            Err(error) => Err(MothershipError::InvalidRequest(error.to_string())),
        }
    }

    fn cancel_authenticate_only(
        &self,
        provider_id: &str,
        registry: &AuthProcessRegistry,
    ) -> Result<()> {
        let pid = registry
            .lock()
            .ok()
            .and_then(|mut map| map.remove(provider_id));
        if let Some(pid) = pid {
            kill_process(pid);
        }
        Ok(())
    }

    pub fn refresh_all(&self, mut emit: impl FnMut(ConnectorSettingsEvent)) {
        let entries = AdapterRegistry::scan(&self.plugins_dir())
            .entries()
            .to_vec();
        self.refresh_entries(entries, &mut emit);
    }

    pub fn refresh_provider(
        &self,
        provider_id: &str,
        mut emit: impl FnMut(ConnectorSettingsEvent),
    ) {
        let entries = AdapterRegistry::scan(&self.plugins_dir())
            .entries()
            .iter()
            .filter(|entry| entry.provider_id == provider_id)
            .cloned()
            .collect::<Vec<_>>();
        self.refresh_entries(entries, &mut emit);
    }

    fn refresh_entries(
        &self,
        entries: Vec<AdapterEntry>,
        emit: &mut impl FnMut(ConnectorSettingsEvent),
    ) {
        if entries.is_empty() {
            return;
        }
        let provider_ids = entries
            .iter()
            .map(|entry| entry.provider_id.clone())
            .collect::<Vec<_>>();
        if let Some(event) = self.begin_refresh(&provider_ids) {
            emit(event);
        }

        for entry in entries {
            self.refresh_entry(&entry, emit);
        }
    }

    fn begin_refresh(&self, provider_ids: &[String]) -> Option<ConnectorSettingsEvent> {
        let mut started = false;
        {
            let mut state = self.state.lock().unwrap();
            for provider_id in provider_ids {
                started |= state.refreshing.insert(provider_id.clone());
            }
        }
        if started {
            self.event(ConnectorSettingsEventKind::RefreshStarted)
        } else {
            None
        }
    }

    fn refresh_entry(&self, entry: &AdapterEntry, emit: &mut impl FnMut(ConnectorSettingsEvent)) {
        if let Err(error) = verified_adapter_entry(entry) {
            let mut state = self.state.lock().unwrap();
            let previous = state.catalogs.get(&entry.provider_id).cloned();
            state.catalogs.insert(
                entry.provider_id.clone(),
                AdapterModelCatalog {
                    models: previous
                        .as_ref()
                        .map(|catalog| catalog.models.clone())
                        .unwrap_or_default(),
                    management: previous
                        .as_ref()
                        .map(|catalog| catalog.management)
                        .unwrap_or(ModelManagement::Fixed),
                    error: Some(error),
                },
            );
            state.catalog_fetched_at.remove(&entry.provider_id);
            state.adapter_info.remove(&entry.provider_id);
            state.refreshing.remove(&entry.provider_id);
            if let Some(event) = self.event(ConnectorSettingsEventKind::ProviderUpdated) {
                emit(event);
            }
            return;
        }

        let vault = self.vault();
        let info_result = adapter_info_result(&self.pool, entry, &vault);
        if let Ok(info) = info_result {
            {
                let mut state = self.state.lock().unwrap();
                state.adapter_info.insert(entry.provider_id.clone(), info);
            }
            if let Some(event) = self.event(ConnectorSettingsEventKind::ProviderUpdated) {
                emit(event);
            }
        }

        let catalog_result = self
            .cached_model_catalog(&entry.provider_id)
            .map(|catalog| Ok((catalog, false)))
            .unwrap_or_else(|| {
                adapter_model_catalog(&self.pool, entry, &vault).map(|catalog| (catalog, true))
            });

        {
            let mut state = self.state.lock().unwrap();
            match catalog_result {
                Ok((catalog, fetched)) => {
                    state.catalogs.insert(entry.provider_id.clone(), catalog);
                    if fetched {
                        state
                            .catalog_fetched_at
                            .insert(entry.provider_id.clone(), Instant::now());
                    }
                }
                Err(error) => {
                    let previous = state.catalogs.get(&entry.provider_id).cloned();
                    state.catalogs.insert(
                        entry.provider_id.clone(),
                        AdapterModelCatalog {
                            models: previous
                                .as_ref()
                                .map(|catalog| catalog.models.clone())
                                .unwrap_or_default(),
                            management: previous
                                .as_ref()
                                .map(|catalog| catalog.management)
                                .unwrap_or(ModelManagement::Fixed),
                            error: Some(error),
                        },
                    );
                    state.catalog_fetched_at.remove(&entry.provider_id);
                }
            }

            state.refreshing.remove(&entry.provider_id);
        }

        if let Some(event) = self.event(ConnectorSettingsEventKind::ProviderUpdated) {
            emit(event);
        }
    }

    fn event(&self, kind: ConnectorSettingsEventKind) -> Option<ConnectorSettingsEvent> {
        match self.snapshot() {
            Ok(snapshot) => Some(ConnectorSettingsEvent { kind, snapshot }),
            Err(error) => {
                eprintln!("failed to build connector event: {error}");
                None
            }
        }
    }

    fn invalidate_provider(&self, provider_id: &str) {
        let mut state = self.state.lock().unwrap();
        state.catalogs.remove(provider_id);
        state.catalog_fetched_at.remove(provider_id);
        state.adapter_info.remove(provider_id);
    }

    fn cached_model_catalog(&self, provider_id: &str) -> Option<AdapterModelCatalog> {
        let state = self.state.lock().unwrap();
        let fetched_at = state.catalog_fetched_at.get(provider_id)?;
        if fetched_at.elapsed() > MODEL_CATALOG_CACHE_TTL {
            return None;
        }
        let catalog = state.catalogs.get(provider_id)?;
        if catalog.error.is_some() {
            return None;
        }
        Some(catalog.clone())
    }

    fn vault(&self) -> FileCredentialVault {
        FileCredentialVault::new(self.auth_dir())
    }

    fn plugins_dir(&self) -> PathBuf {
        sibling_dir(&self.database, "plugins")
    }

    fn auth_dir(&self) -> PathBuf {
        sibling_dir(&self.database, "auth")
    }

    fn provider_labels(&self) -> BTreeMap<String, String> {
        adapter_provider_labels(&self.plugins_dir())
    }

    fn provider_icons(&self) -> BTreeMap<String, String> {
        AdapterRegistry::scan(&self.plugins_dir())
            .entries()
            .iter()
            .filter_map(|entry| {
                let uri = icon_data_uri(entry.icon.as_deref()?)?;
                Some((entry.provider_id.clone(), uri))
            })
            .collect()
    }

    fn provider_runtime_kinds(&self) -> BTreeMap<String, ProviderRuntimeKind> {
        AdapterRegistry::scan(&self.plugins_dir())
            .entries()
            .iter()
            .map(|entry| (entry.provider_id.clone(), adapter_runtime_kind(entry)))
            .collect()
    }

    fn provider_errors(&self) -> BTreeMap<String, String> {
        adapter_diagnostics(&self.plugins_dir())
    }
}

/// Patch value accepted from UI settings forms. Secret fields are write-only:
/// `unchanged` keeps the existing value, `set` replaces it, and `clear` removes
/// it. Unknown/internal keys are rejected against the adapter's declared schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AdapterSettingPatchValue {
    Set { value: String },
    Clear,
    Unchanged,
}

/// Best-effort terminate a child process by id (a spawned adapter). Core can
/// always kill an adapter — crash isolation is part of the contract.
/// Fire-and-forget (`spawn`, not `output`) so it never blocks the caller.
pub fn kill_process(pid: u32) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    #[cfg(unix)]
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn verified_adapter_entry(entry: &AdapterEntry) -> std::result::Result<(), String> {
    let actual_sha256 = entry
        .verify_program_integrity()
        .map_err(|error| error.to_string())?;
    if let Some(expected_sha256) = trusted_built_in_adapter_sha256(&entry.provider_id) {
        if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
            return Err(format!(
                "built-in adapter {} was modified: expected {}, got {}",
                entry.provider_id, expected_sha256, actual_sha256
            ));
        }
    } else if is_built_in_adapter_id(&entry.provider_id) {
        return Err(format!(
            "built-in adapter {} has no trusted build hash",
            entry.provider_id
        ));
    }
    Ok(())
}

fn adapter_provider_labels(plugins_dir: &Path) -> BTreeMap<String, String> {
    let registry = AdapterRegistry::scan(plugins_dir);
    let mut labels = registry
        .entries()
        .iter()
        .map(|entry| (entry.provider_id.clone(), entry.provider_label.clone()))
        .collect::<BTreeMap<_, _>>();
    for diagnostic in registry.diagnostics() {
        if let (Some(provider_id), Some(provider_label)) = (
            diagnostic.provider_id.as_ref(),
            diagnostic.provider_label.as_ref(),
        ) {
            if !provider_label.trim().is_empty() {
                labels.insert(provider_id.clone(), provider_label.clone());
            }
        }
    }
    labels
}

fn adapter_diagnostics(plugins_dir: &Path) -> BTreeMap<String, String> {
    let registry = AdapterRegistry::scan(plugins_dir);
    let mut errors = registry
        .diagnostics()
        .iter()
        .filter_map(|diagnostic| {
            let provider_id = diagnostic.provider_id.as_ref()?;
            Some((provider_id.clone(), diagnostic.message.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    for entry in registry.entries() {
        if let Err(error) = verified_adapter_entry(entry) {
            errors.insert(entry.provider_id.clone(), error);
        }
    }
    errors
}

pub(crate) fn find_trusted_adapter_entry(
    plugins_dir: &Path,
    provider_id: &str,
) -> std::result::Result<AdapterEntry, MothershipError> {
    let registry = AdapterRegistry::scan(plugins_dir);
    let entry = registry
        .find(provider_id)
        .ok_or_else(|| MothershipError::InvalidRequest(format!("unknown adapter: {provider_id}")))?
        .clone();
    verified_adapter_entry(&entry).map_err(|error| {
        MothershipError::InvalidRequest(format!("adapter {provider_id} failed validation: {error}"))
    })?;
    Ok(entry)
}

pub(crate) fn ensure_adapter_capability(
    entry: &AdapterEntry,
    capability: &str,
    action: &str,
) -> std::result::Result<(), String> {
    if entry.has_capability(capability) {
        Ok(())
    } else {
        Err(format!(
            "adapter {} is missing capability `{}` required to {}",
            entry.provider_id, capability, action
        ))
    }
}

pub(crate) fn adapter_runtime_kind(entry: &AdapterEntry) -> ProviderRuntimeKind {
    if entry.has_capability(CAPABILITY_AGENT_RUNTIME) {
        ProviderRuntimeKind::SelfManaged
    } else {
        ProviderRuntimeKind::CoreManaged
    }
}

pub(crate) fn ensure_adapter_can_run_chat(
    entry: &AdapterEntry,
) -> std::result::Result<ProviderRuntimeKind, String> {
    let runtime_kind = adapter_runtime_kind(entry);
    match runtime_kind {
        ProviderRuntimeKind::CoreManaged => {
            ensure_adapter_capability(entry, CAPABILITY_LLM_CHAT, "run chat")?;
        }
        ProviderRuntimeKind::SelfManaged => {
            ensure_adapter_capability(entry, CAPABILITY_AGENT_RUNTIME, "run self-managed agent")?;
        }
    }
    Ok(runtime_kind)
}

fn is_built_in_adapter_id(provider_id: &str) -> bool {
    matches!(provider_id, "codex" | "openrouter" | "claude-agent")
}

/// Spawns an adapter just long enough to read its advertised models. Each call
/// starts and drops a child process; model listing is infrequent so this is fine
/// for now (a resident registry can come later). Settings (which can drive the
/// model list, e.g. OpenRouter's user-defined list) come from the shared vault.
fn adapter_model_catalog(
    pool: &AdapterPool,
    entry: &AdapterEntry,
    vault: &FileCredentialVault,
) -> std::result::Result<AdapterModelCatalog, String> {
    ensure_adapter_capability(entry, CAPABILITY_LLM_MODELS, "list models")?;
    let (models, management) = pool
        .with(entry, vault, |adapter| adapter.models())
        .map_err(|error| error.to_string())?;
    let models = models
        .into_iter()
        .map(|model| LlmModel {
            provider_id: entry.provider_id.clone(),
            provider_label: entry.provider_label.clone(),
            id: model.id,
            label: model.label,
            family: "Adapter".to_string(),
            description: String::new(),
            capabilities: vec!["text".to_string()],
            reasoning: model.reasoning,
            fast_mode: model.fast_mode,
            recommended: model.recommended,
        })
        .collect();
    Ok(AdapterModelCatalog {
        models,
        management,
        error: None,
    })
}

fn adapter_settings_fields(
    pool: &AdapterPool,
    entry: &AdapterEntry,
    vault: &FileCredentialVault,
) -> std::result::Result<Vec<SettingsField>, String> {
    ensure_adapter_capability(entry, CAPABILITY_SETTINGS_READ, "read settings schema")?;
    pool.with(entry, vault, |adapter| adapter.settings_schema())
        .map_err(|error| error.to_string())
}

fn adapter_info_result(
    pool: &AdapterPool,
    entry: &AdapterEntry,
    vault: &FileCredentialVault,
) -> std::result::Result<AdapterInfo, String> {
    ensure_adapter_capability(entry, CAPABILITY_SETTINGS_READ, "read adapter info")?;
    let (fields, auth, auth_status) = pool
        .with(entry, vault, |adapter| {
            let fields = adapter.settings_schema()?;
            let auth = adapter.auth_schema()?;
            let auth_status = adapter.auth_status()?;
            Ok((fields, auth, auth_status))
        })
        .map_err(|error| error.to_string())?;
    let auth_kind = match auth {
        AuthKind::None => "none",
        AuthKind::ApiKey { .. } => "api_key",
        AuthKind::OauthInternal => "oauth_internal",
        AuthKind::ExternalProcess => "external_process",
    }
    .to_string();

    let values = vault
        .load_adapter_settings(&entry.provider_id)
        .unwrap_or_default();
    let authenticated = is_authenticated(&auth_status);

    let view = sanitized_adapter_settings(fields, &values);

    Ok(AdapterInfo {
        view,
        auth_kind,
        auth_status,
        authenticated,
    })
}

fn is_authenticated(status: &AuthStatus) -> bool {
    matches!(
        status.kind,
        AuthStatusKind::Authenticated | AuthStatusKind::Configured | AuthStatusKind::NotRequired
    )
}

fn connector_providers(
    catalogs: &BTreeMap<String, AdapterModelCatalog>,
    selected_model: &SelectedLlmModel,
    disabled: &BTreeSet<String>,
    adapter_info: &BTreeMap<String, AdapterInfo>,
    provider_labels: &BTreeMap<String, String>,
    provider_icons: &BTreeMap<String, String>,
    provider_runtime_kinds: &BTreeMap<String, ProviderRuntimeKind>,
    refreshing: &BTreeSet<String>,
    provider_errors: &BTreeMap<String, String>,
    pool: &AdapterPool,
    runtime: Option<&ProviderRuntimeManager>,
) -> Vec<ConnectorProviderSummary> {
    let models = catalogs
        .values()
        .flat_map(|catalog| catalog.models.clone())
        .collect::<Vec<_>>();
    // Providers are exactly the installed adapters: those advertising models and
    // those exposing a settings form. (A freshly installed adapter shows up via
    // its settings form before it has any usable models.)
    let mut provider_ids = BTreeSet::new();
    for model in &models {
        provider_ids.insert(model.provider_id.clone());
    }
    for provider_id in adapter_info.keys() {
        provider_ids.insert(provider_id.clone());
    }
    for provider_id in catalogs.keys() {
        provider_ids.insert(provider_id.clone());
    }
    for provider_id in provider_labels.keys() {
        provider_ids.insert(provider_id.clone());
    }
    for provider_id in provider_errors.keys() {
        provider_ids.insert(provider_id.clone());
    }

    provider_ids
        .into_iter()
        .map(|provider_id| {
            let provider_models = models
                .iter()
                .filter(|model| model.provider_id == provider_id)
                .cloned()
                .collect::<Vec<_>>();
            // Prefer the adapter's declared label (available even before any
            // models load), then a model's label, then the bare id.
            let label = provider_labels
                .get(&provider_id)
                .cloned()
                .or_else(|| {
                    provider_models
                        .first()
                        .map(|model| model.provider_label.clone())
                })
                .unwrap_or_else(|| provider_id.clone());
            let selected_model_id = (selected_model.provider_id == provider_id)
                .then(|| selected_model.model_id.clone());
            let info = adapter_info.get(&provider_id);
            let auth_kind = info
                .map(|info| info.auth_kind.clone())
                .unwrap_or_else(|| "none".to_string());
            let authenticated = info.map(|info| info.authenticated).unwrap_or(false);
            let auth_status = info
                .map(|info| info.auth_status.clone())
                .unwrap_or_else(|| AuthStatus::missing("connector has not reported auth status"));
            let adapter_settings = info.map(|info| info.view.clone());
            let icon = provider_icons.get(&provider_id).cloned();
            let runtime_kind = provider_runtime_kinds
                .get(&provider_id)
                .copied()
                .unwrap_or(ProviderRuntimeKind::CoreManaged);
            let catalog = catalogs.get(&provider_id);
            let model_error = catalog
                .and_then(|catalog| catalog.error.clone())
                .or_else(|| provider_errors.get(&provider_id).cloned());
            let refresh_status = if refreshing.contains(&provider_id) {
                ConnectorRefreshStatus::Refreshing
            } else if model_error.is_some() {
                ConnectorRefreshStatus::Failed
            } else if catalog.is_some() || info.is_some() {
                ConnectorRefreshStatus::Ready
            } else {
                ConnectorRefreshStatus::Pending
            };
            let runtime_ready = pool.is_resident_ready(&provider_id);
            let runtime_status = runtime
                .map(|runtime| runtime.status(&provider_id))
                .unwrap_or_else(ProviderRuntimeStatus::idle);

            let enabled = !disabled.contains(&provider_id);

            ConnectorProviderSummary {
                id: provider_id,
                label,
                enabled,
                runtime_kind,
                icon,
                settings_schema: connector_settings_schema(
                    catalog.map(|catalog| catalog.management),
                ),
                models: provider_models,
                model_error,
                refresh_status,
                runtime_ready,
                runtime_status,
                selected_model_id,
                auth_kind,
                auth_status,
                authenticated,
                adapter_settings,
            }
        })
        .collect()
}

fn connector_settings_schema(management: Option<ModelManagement>) -> ConnectorSettingsSchema {
    let management = management.unwrap_or(ModelManagement::Fixed);
    let (kind, description, add_model_label, accepts_custom_model_ids) = match management {
        ModelManagement::Fixed => (
            ConnectorModelManagementKind::FixedCatalog,
            "This connector exposes a fixed model catalog.".to_string(),
            None,
            false,
        ),
        ModelManagement::Server => (
            ConnectorModelManagementKind::RemoteCatalog,
            "Models are fetched from the provider adapter.".to_string(),
            None,
            false,
        ),
        ModelManagement::ServerWithCustom => (
            ConnectorModelManagementKind::RemoteCatalog,
            "Models are fetched from the provider adapter; custom model ids can also be used."
                .to_string(),
            Some("Use custom model".to_string()),
            true,
        ),
        ModelManagement::UserDefined => (
            ConnectorModelManagementKind::EditableList,
            "Add the model ids this connector should expose.".to_string(),
            Some("Add model".to_string()),
            true,
        ),
    };
    ConnectorSettingsSchema {
        model_management: ConnectorModelManagementSchema {
            kind,
            title: "Models".to_string(),
            description,
            add_model_label,
            accepts_custom_model_ids,
        },
    }
}

fn model_management_accepts_custom_ids(management: ModelManagement) -> bool {
    matches!(
        management,
        ModelManagement::ServerWithCustom | ModelManagement::UserDefined
    )
}

fn is_valid_custom_model_id(model_id: &str) -> bool {
    let trimmed = model_id.trim();
    !trimmed.is_empty()
        && trimmed == model_id
        && trimmed.len() <= 256
        && !trimmed
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
}

fn sanitized_adapter_settings(
    fields: Vec<SettingsField>,
    stored_values: &BTreeMap<String, String>,
) -> AdapterSettingsView {
    let mut values = BTreeMap::new();
    let mut secrets = BTreeMap::new();
    let fields = fields
        .into_iter()
        .map(|field| {
            if matches!(field.kind, SettingsFieldKind::Secret) {
                secrets.insert(
                    field.key.clone(),
                    stored_values
                        .get(&field.key)
                        .map(|value| secret_state(value))
                        .unwrap_or_else(empty_secret_state),
                );
            } else if let Some(value) = stored_values.get(&field.key) {
                values.insert(field.key.clone(), value.clone());
            }
            AdapterSettingsFieldView {
                key: field.key,
                label: field.label,
                kind: match field.kind {
                    SettingsFieldKind::Text => "text",
                    SettingsFieldKind::Secret => "secret",
                    SettingsFieldKind::Bool => "bool",
                    SettingsFieldKind::StringList => "string_list",
                    SettingsFieldKind::ModelVisibilityList => "model_visibility_list",
                }
                .to_string(),
                required: field.required,
                options: field
                    .options
                    .into_iter()
                    .map(|option| AdapterSettingsFieldOptionView {
                        value: option.value,
                        label: option.label,
                        description: option.description,
                    })
                    .collect(),
            }
        })
        .collect();

    AdapterSettingsView {
        fields,
        values,
        secrets,
    }
}

fn empty_secret_state() -> SecretSettingState {
    SecretSettingState {
        has_value: false,
        fingerprint: None,
        last4: None,
    }
}

fn secret_state(value: &str) -> SecretSettingState {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return empty_secret_state();
    }
    let digest = Sha256::digest(trimmed.as_bytes());
    let fingerprint = digest
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut tail = trimmed.chars().rev().take(4).collect::<Vec<_>>();
    tail.reverse();
    SecretSettingState {
        has_value: true,
        fingerprint: Some(fingerprint),
        last4: Some(tail.into_iter().collect()),
    }
}

fn adapter_logout_once(
    entry: &AdapterEntry,
    settings: BTreeMap<String, String>,
) -> std::result::Result<(), String> {
    let mut session = spawn_ready_adapter_without_secret_sink(entry, settings)
        .map_err(|error| error.to_string())?;
    session.adapter.logout().map_err(|error| error.to_string())
}

/// Reads an icon file and encodes it as a `data:` URI (base64). Returns `None`
/// if the file is missing or the extension isn't a known image type.
fn icon_data_uri(path: &Path) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    let mime = match path
        .extension()
        .and_then(|ext| ext.to_str())?
        .to_lowercase()
        .as_str()
    {
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    };
    let bytes = std::fs::read(path).ok()?;
    Some(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

/// A directory next to the database file (the app data dir holds the db, the
/// `plugins/` registry, and the `auth/` vault side by side).
fn sibling_dir(database: &Database, name: &str) -> PathBuf {
    database
        .path()
        .parent()
        .map(|path| path.join(name))
        .unwrap_or_else(|| PathBuf::from(name))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_app_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("mothership_connectors_{name}_{stamp}"))
    }

    fn echo_adapter_path() -> PathBuf {
        let exe = if cfg!(windows) {
            "echo_adapter.exe"
        } else {
            "echo_adapter"
        };
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug")
            .join(exe)
    }

    fn install_echo_adapter(app_dir: &std::path::Path, adapter_path: &std::path::Path) {
        let adapter_dir = app_dir.join("plugins").join("echo");
        fs::create_dir_all(&adapter_dir).expect("create adapter dir");
        let manifest = serde_json::json!({
            "provider_id": "echo",
            "provider_label": "Echo",
            "program": adapter_path,
            "capabilities": ["llm.models", "llm.chat", "settings.read", "settings.write"],
        });
        fs::write(
            adapter_dir.join("adapter.json"),
            serde_json::to_string(&manifest).expect("manifest json"),
        )
        .expect("write manifest");
    }

    #[test]
    fn model_management_schema_marks_custom_model_support() {
        let remote = connector_settings_schema(Some(ModelManagement::Server));
        assert!(!remote.model_management.accepts_custom_model_ids);

        let remote_custom = connector_settings_schema(Some(ModelManagement::ServerWithCustom));
        assert_eq!(
            remote_custom.model_management.kind,
            ConnectorModelManagementKind::RemoteCatalog
        );
        assert!(remote_custom.model_management.accepts_custom_model_ids);

        let user_defined = connector_settings_schema(Some(ModelManagement::UserDefined));
        assert_eq!(
            user_defined.model_management.kind,
            ConnectorModelManagementKind::EditableList
        );
        assert!(user_defined.model_management.accepts_custom_model_ids);
    }

    #[test]
    fn custom_model_id_validation_accepts_provider_aliases_without_whitespace() {
        assert!(is_valid_custom_model_id("opus[1m]"));
        assert!(is_valid_custom_model_id("claude-opus-4-7"));
        assert!(is_valid_custom_model_id("anthropic/claude-sonnet-4.6"));
        assert!(!is_valid_custom_model_id(""));
        assert!(!is_valid_custom_model_id(" claude-opus-4-7"));
        assert!(!is_valid_custom_model_id("claude opus"));
    }

    #[test]
    fn snapshot_redacts_secret_adapter_settings() {
        let adapter_path = echo_adapter_path();
        if !adapter_path.exists() {
            eprintln!(
                "skipping: echo adapter not built at {}",
                adapter_path.display()
            );
            return;
        }

        let app_dir = temp_app_dir("redacts_secret");
        install_echo_adapter(&app_dir, &adapter_path);
        let database = Database::open(app_dir.join("mothership.sqlite3")).expect("open database");
        FileCredentialVault::new(app_dir.join("auth"))
            .save_adapter_settings(
                "echo",
                &BTreeMap::from([
                    ("api_key".to_string(), "secret-value-1234".to_string()),
                    ("endpoint".to_string(), "https://example.test".to_string()),
                ]),
            )
            .expect("seed settings");

        let manager = ConnectorManager::new(database, Arc::new(AdapterPool::new()));
        manager.refresh_all(|_| {});
        let snapshot = manager.snapshot().expect("snapshot");
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.id == "echo")
            .expect("echo provider");
        let settings = provider
            .adapter_settings
            .as_ref()
            .expect("adapter settings");

        assert_eq!(
            settings.values.get("endpoint"),
            Some(&"https://example.test".to_string())
        );
        assert!(!settings.values.contains_key("api_key"));
        assert_eq!(
            settings
                .secrets
                .get("api_key")
                .map(|secret| secret.has_value),
            Some(true)
        );
        assert_eq!(
            settings
                .secrets
                .get("api_key")
                .and_then(|secret| secret.last4.as_deref()),
            Some("1234")
        );

        let _ = fs::remove_dir_all(app_dir);
    }

    #[test]
    fn save_adapter_settings_rejects_unknown_or_internal_keys() {
        let adapter_path = echo_adapter_path();
        if !adapter_path.exists() {
            eprintln!(
                "skipping: echo adapter not built at {}",
                adapter_path.display()
            );
            return;
        }

        let app_dir = temp_app_dir("rejects_unknown_key");
        install_echo_adapter(&app_dir, &adapter_path);
        let database = Database::open(app_dir.join("mothership.sqlite3")).expect("open database");

        let error = ConnectorManager::new(database, Arc::new(AdapterPool::new()))
            .save_adapter_settings(
                "echo",
                BTreeMap::from([(
                    "credential".to_string(),
                    AdapterSettingPatchValue::Set {
                        value: "must-not-write".to_string(),
                    },
                )]),
            )
            .expect_err("unknown key rejected");

        assert!(matches!(error, MothershipError::InvalidRequest(_)));

        let _ = fs::remove_dir_all(app_dir);
    }

    #[test]
    fn manager_snapshot_reports_adapter_integrity_mismatch() {
        let app_dir = temp_app_dir("integrity_mismatch");
        let adapter_dir = app_dir.join("plugins").join("broken");
        fs::create_dir_all(&adapter_dir).expect("create adapter dir");
        let program = adapter_dir.join(if cfg!(windows) {
            "broken.exe"
        } else {
            "broken"
        });
        fs::write(&program, b"not a real adapter").expect("write fake adapter");
        let manifest = serde_json::json!({
            "provider_id": "broken",
            "provider_label": "Broken",
            "program": program,
            "capabilities": ["llm.models", "llm.chat", "settings.read", "settings.write"],
            "integrity": {
                "algorithm": "sha256",
                "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
            },
        });
        fs::write(
            adapter_dir.join("adapter.json"),
            serde_json::to_string(&manifest).expect("manifest json"),
        )
        .expect("write manifest");

        let database = Database::open(app_dir.join("mothership.sqlite3")).expect("open database");
        let snapshot = ConnectorManager::new(database, Arc::new(AdapterPool::new()))
            .snapshot()
            .expect("snapshot");
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.id == "broken")
            .expect("broken provider");

        assert_eq!(provider.refresh_status, ConnectorRefreshStatus::Failed);
        assert!(provider
            .model_error
            .as_deref()
            .unwrap_or_default()
            .contains("integrity mismatch"));

        let _ = fs::remove_dir_all(app_dir);
    }
}
