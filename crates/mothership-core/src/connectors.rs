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

use serde::{Deserialize, Serialize};

use mothership_adapter_host::protocol::{AuthKind, SettingsFieldKind};
use mothership_adapter_host::{Adapter, AdapterEntry, AdapterRegistry};

use crate::adapter_pool::AdapterPool;
use crate::auth::FileCredentialVault;
use crate::llm::{
    ConnectorModelManagementKind, ConnectorModelManagementSchema, ConnectorSettingsSchema, LlmModel,
    SelectedLlmModel,
};
use crate::{Database, MothershipError, Result};

/// Tracks in-flight `authenticate` flows by provider id -> adapter process id, so
/// a [`cancel_authenticate`] (or the client leaving Settings) can terminate one.
/// Owned by whoever runs the auth flows (the sidecar); passed into
/// [`ConnectorService::authenticate`] / [`ConnectorService::cancel_authenticate`].
pub type AuthProcessRegistry = Mutex<HashMap<String, u32>>;

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
pub struct ConnectorProviderSummary {
    pub id: String,
    pub label: String,
    /// The adapter's own icon as a data URI, if it ships one (declared in its
    /// manifest). Core just renders whatever the plugin provides.
    pub icon: Option<String>,
    pub settings_schema: ConnectorSettingsSchema,
    pub models: Vec<LlmModel>,
    pub selected_model_id: Option<String>,
    /// The adapter's auth scheme: `none` / `api_key` / `oauth_internal` /
    /// `external_process`. Drives whether the UI shows an "Authorize" button.
    pub auth_kind: String,
    /// Whether the adapter currently has a stored credential — toggles the UI
    /// between "Authorize" and "Log out".
    pub authenticated: bool,
    /// Present for subprocess adapters: the settings fields they declare plus
    /// their current values, so the UI can render and save a config form.
    pub adapter_settings: Option<AdapterSettingsView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterSettingsView {
    pub fields: Vec<AdapterSettingsFieldView>,
    pub values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterSettingsFieldView {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub required: bool,
}

/// Per-adapter UI info gathered from one spawn: the settings form (declared
/// fields + current values from the vault), the adapter's auth scheme, and
/// whether it currently has a stored credential (drives Authorize vs Log out).
struct AdapterInfo {
    view: AdapterSettingsView,
    auth_kind: String,
    authenticated: bool,
}

/// Reads and mutates the connector configuration for one database (and its
/// sibling plugins/auth directories). Borrows the database like
/// [`crate::ChatRunService`]; cheap to construct per request.
pub struct ConnectorService<'a> {
    database: &'a Database,
    pool: Arc<AdapterPool>,
}

impl<'a> ConnectorService<'a> {
    pub fn new(database: &'a Database, pool: Arc<AdapterPool>) -> Self {
        Self { database, pool }
    }

    /// The full Connectors snapshot: every installed adapter's models, settings
    /// form, auth state, plus the active model selection.
    pub fn snapshot(&self) -> Result<ConnectorSettingsSnapshot> {
        let models = self.available_models();
        let selected_model = self.database.selected_llm_model()?;
        let adapter_info = self.adapter_infos();
        let provider_labels = self.provider_labels();
        let provider_icons = self.provider_icons();

        Ok(ConnectorSettingsSnapshot {
            providers: connector_providers(
                models,
                &selected_model,
                &adapter_info,
                &provider_labels,
                &provider_icons,
            ),
            selected_model,
        })
    }

    /// Validates that `provider_id`/`model_id` is one an installed adapter
    /// advertises, persists the selection, and returns the refreshed snapshot.
    pub fn set_selected_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<ConnectorSettingsSnapshot> {
        let available = self.available_models();
        if !available
            .iter()
            .any(|model| model.provider_id == provider_id && model.id == model_id)
        {
            return Err(MothershipError::InvalidRequest(format!(
                "unsupported model: {provider_id}/{model_id}"
            )));
        }
        self.database.set_selected_llm_model(provider_id, model_id)?;
        self.snapshot()
    }

    /// Persists adapter settings into the shared vault, merged so secrets the
    /// form doesn't carry (e.g. an OAuth token the adapter stored itself) survive.
    pub fn save_adapter_settings(
        &self,
        provider_id: &str,
        values: BTreeMap<String, String>,
    ) -> Result<ConnectorSettingsSnapshot> {
        let registry = AdapterRegistry::scan(&self.plugins_dir());
        if registry.find(provider_id).is_none() {
            return Err(MothershipError::InvalidRequest(format!(
                "unknown adapter: {provider_id}"
            )));
        }
        self.vault().merge_adapter_settings(provider_id, values)?;
        self.snapshot()
    }

    /// Logs an adapter out. Best-effort: first asks the adapter to revoke its
    /// credential server-side (e.g. Codex OAuth token revoke), then forgets it in
    /// the shared vault. A revoke failure never blocks local logout.
    pub fn logout(&self, provider_id: &str) -> Result<ConnectorSettingsSnapshot> {
        let vault = self.vault();
        let registry = AdapterRegistry::scan(&self.plugins_dir());
        if let Some(entry) = registry.find(provider_id) {
            // Best-effort server-side revoke via the (resident) adapter — the pool
            // seeds it with the stored credential — then drop the resident so it no
            // longer holds the revoked token.
            let _ = self.pool.with(entry, &vault, |adapter| adapter.logout());
            self.pool.evict(provider_id);
        }
        vault.delete_adapter_settings(provider_id)?;
        self.snapshot()
    }

    /// Runs an adapter's own auth flow (e.g. Codex browser OAuth) on demand. The
    /// adapter owns the flow end-to-end; Core spawns it, seeds stored settings,
    /// wires the secret sink so a resulting token lands in the shared vault,
    /// registers the process so it can be cancelled, and waits for completion.
    ///
    /// Blocks for as long as the flow takes (the user finishing the browser
    /// step), so the caller must run it off any UI/reader thread. If a concurrent
    /// [`cancel_authenticate`](Self::cancel_authenticate) already removed this
    /// flow's registration, the adapter was killed — that's a clean (cancelled),
    /// not failed, outcome.
    pub fn authenticate(
        &self,
        provider_id: &str,
        registry: &AuthProcessRegistry,
    ) -> Result<ConnectorSettingsSnapshot> {
        let adapters = AdapterRegistry::scan(&self.plugins_dir());
        let entry = adapters.find(provider_id).ok_or_else(|| {
            MothershipError::InvalidRequest(format!("unknown adapter: {provider_id}"))
        })?;

        let vault = self.vault();
        let mut adapter = Adapter::spawn(&entry.program)
            .map_err(|error| MothershipError::InvalidRequest(error.to_string()))?;

        let sink_vault = vault.clone();
        let sink_provider = provider_id.to_string();
        adapter.set_store_secret_handler(move |values| {
            if let Err(error) = sink_vault.merge_adapter_settings(&sink_provider, values) {
                eprintln!("failed to persist adapter secret for {sink_provider}: {error}");
            }
        });

        adapter
            .initialize()
            .map_err(|error| MothershipError::InvalidRequest(error.to_string()))?;
        let settings = vault.load_adapter_settings(provider_id)?;
        if !settings.is_empty() {
            adapter
                .set_settings(settings)
                .map_err(|error| MothershipError::InvalidRequest(error.to_string()))?;
        }

        if let Ok(mut map) = registry.lock() {
            map.insert(provider_id.to_string(), adapter.process_id());
        }
        let result = adapter.authenticate();
        // If our registration is already gone, a cancel took it and killed us —
        // treat that as a clean (not error) outcome.
        let cancelled = registry
            .lock()
            .ok()
            .and_then(|mut map| map.remove(provider_id))
            .is_none();
        drop(adapter);

        match result {
            Ok(()) => self.snapshot(),
            Err(_) if cancelled => self.snapshot(),
            Err(error) => Err(MothershipError::InvalidRequest(error.to_string())),
        }
    }

    /// Cancels an in-flight `authenticate` for `provider_id` by terminating the
    /// adapter process (generic — works for any adapter's auth flow).
    pub fn cancel_authenticate(
        &self,
        provider_id: &str,
        registry: &AuthProcessRegistry,
    ) -> Result<ConnectorSettingsSnapshot> {
        let pid = registry
            .lock()
            .ok()
            .and_then(|mut map| map.remove(provider_id));
        if let Some(pid) = pid {
            kill_process(pid);
        }
        self.snapshot()
    }

    /// Models available to the UI: exactly what the installed adapters advertise.
    fn available_models(&self) -> Vec<LlmModel> {
        let registry = AdapterRegistry::scan(&self.plugins_dir());
        let vault = self.vault();
        let mut models = Vec::new();
        for entry in registry.entries() {
            match adapter_models(&self.pool, entry, &vault) {
                Ok(list) => models.extend(list),
                Err(error) => eprintln!("skipping adapter {}: {error}", entry.provider_id),
            }
        }
        models
    }

    /// Spawns each installed adapter once to read its settings form + auth scheme.
    fn adapter_infos(&self) -> BTreeMap<String, AdapterInfo> {
        let registry = AdapterRegistry::scan(&self.plugins_dir());
        let vault = self.vault();
        let mut infos = BTreeMap::new();
        for entry in registry.entries() {
            if let Some(info) = adapter_info(&self.pool, entry, &vault) {
                infos.insert(entry.provider_id.clone(), info);
            }
        }
        infos
    }

    /// Maps each installed adapter's provider id to its human label (cheap; no
    /// spawn), so the UI can name a connector before its models load.
    fn provider_labels(&self) -> BTreeMap<String, String> {
        AdapterRegistry::scan(&self.plugins_dir())
            .entries()
            .iter()
            .map(|entry| (entry.provider_id.clone(), entry.provider_label.clone()))
            .collect()
    }

    /// Maps each installed adapter's provider id to its icon as a data URI, if it
    /// ships one — read + inlined so the webview needs no disk access.
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

    fn vault(&self) -> FileCredentialVault {
        FileCredentialVault::new(self.auth_dir())
    }

    fn plugins_dir(&self) -> PathBuf {
        sibling_dir(self.database, "plugins")
    }

    fn auth_dir(&self) -> PathBuf {
        sibling_dir(self.database, "auth")
    }
}

/// Best-effort terminate a child process by id (a spawned adapter). Core can
/// always kill an adapter — crash isolation is part of the contract.
/// Fire-and-forget (`spawn`, not `output`) so it never blocks the caller.
pub fn kill_process(pid: u32) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .spawn();
    #[cfg(unix)]
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .spawn();
}

/// Spawns an adapter just long enough to read its advertised models. Each call
/// starts and drops a child process; model listing is infrequent so this is fine
/// for now (a resident registry can come later). Settings (which can drive the
/// model list, e.g. OpenRouter's user-defined list) come from the shared vault.
fn adapter_models(
    pool: &AdapterPool,
    entry: &AdapterEntry,
    vault: &FileCredentialVault,
) -> std::result::Result<Vec<LlmModel>, String> {
    let (models, _management) = pool
        .with(entry, vault, |adapter| adapter.models())
        .map_err(|error| error.to_string())?;
    Ok(models
        .into_iter()
        .map(|model| LlmModel {
            provider_id: entry.provider_id.clone(),
            provider_label: entry.provider_label.clone(),
            id: model.id,
            label: model.label,
            family: "Adapter".to_string(),
            description: String::new(),
            capabilities: vec!["text".to_string()],
            recommended: model.recommended,
        })
        .collect())
}

fn adapter_info(
    pool: &AdapterPool,
    entry: &AdapterEntry,
    vault: &FileCredentialVault,
) -> Option<AdapterInfo> {
    // One round-trip over the (resident) adapter reads both its settings form
    // and its auth scheme.
    let (fields, auth) = pool
        .with(entry, vault, |adapter| {
            let fields = adapter.settings_schema()?;
            let auth = adapter.auth_schema()?;
            Ok((fields, auth))
        })
        .ok()?;
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
    // Core owns the credential store, so it knows the auth STATUS: an
    // oauth/external adapter is "logged in" iff something is stored for it.
    // (Api-key adapters don't show an Authorize/Log-out button, so it's moot.)
    let authenticated =
        matches!(auth_kind.as_str(), "oauth_internal" | "external_process") && !values.is_empty();

    let view = AdapterSettingsView {
        fields: fields
            .into_iter()
            .map(|field| AdapterSettingsFieldView {
                key: field.key,
                label: field.label,
                kind: match field.kind {
                    SettingsFieldKind::Text => "text",
                    SettingsFieldKind::Secret => "secret",
                    SettingsFieldKind::Bool => "bool",
                    SettingsFieldKind::StringList => "string_list",
                }
                .to_string(),
                required: field.required,
            })
            .collect(),
        values,
    };

    Some(AdapterInfo {
        view,
        auth_kind,
        authenticated,
    })
}

fn connector_providers(
    models: Vec<LlmModel>,
    selected_model: &SelectedLlmModel,
    adapter_info: &BTreeMap<String, AdapterInfo>,
    provider_labels: &BTreeMap<String, String>,
    provider_icons: &BTreeMap<String, String>,
) -> Vec<ConnectorProviderSummary> {
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
            let adapter_settings = info.map(|info| info.view.clone());
            let icon = provider_icons.get(&provider_id).cloned();

            ConnectorProviderSummary {
                id: provider_id,
                label,
                icon,
                settings_schema: default_connector_settings_schema(),
                models: provider_models,
                selected_model_id,
                auth_kind,
                authenticated,
                adapter_settings,
            }
        })
        .collect()
}

fn default_connector_settings_schema() -> ConnectorSettingsSchema {
    ConnectorSettingsSchema {
        model_management: ConnectorModelManagementSchema {
            kind: ConnectorModelManagementKind::FixedCatalog,
            title: "Models".to_string(),
            description: "This connector does not expose model management settings yet."
                .to_string(),
            add_model_label: None,
        },
    }
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
