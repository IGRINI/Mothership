use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use mothership_core::{
    auth::FileCredentialVault, ChatConversation, ChatRunEvent, ChatRunEventSink, ChatRunService,
    ChatThreadSummary, DashboardSnapshot, SendChatMessageResult, SidecarStatus,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::{sidecar, state::AppState};

#[tauri::command]
pub fn get_dashboard_snapshot(state: State<'_, AppState>) -> Result<DashboardSnapshot, String> {
    state.database().snapshot().map_err(to_command_error)
}

#[tauri::command]
pub fn append_activity_event(
    state: State<'_, AppState>,
    message: String,
) -> Result<DashboardSnapshot, String> {
    state
        .database()
        .append_activity_event(&message)
        .map_err(to_command_error)?;

    state.database().snapshot().map_err(to_command_error)
}

#[tauri::command]
pub fn list_chats(
    state: State<'_, AppState>,
    limit: Option<i64>,
) -> Result<Vec<ChatThreadSummary>, String> {
    state
        .database()
        .list_chats(limit.unwrap_or(100))
        .map_err(to_command_error)
}

#[tauri::command]
pub fn create_chat(state: State<'_, AppState>) -> Result<ChatConversation, String> {
    state.database().create_chat().map_err(to_command_error)
}

#[tauri::command]
pub fn get_chat(
    state: State<'_, AppState>,
    chat_id: String,
    limit: Option<i64>,
) -> Result<ChatConversation, String> {
    state
        .database()
        .get_chat(&chat_id, limit.unwrap_or(200))
        .map_err(to_command_error)
}

#[tauri::command]
pub fn send_chat_message(
    app: AppHandle,
    state: State<'_, AppState>,
    chat_id: Option<String>,
    content: String,
) -> Result<SendChatMessageResult, String> {
    let result = state
        .database()
        .begin_chat_run(chat_id.as_deref(), &content)
        .map_err(to_command_error)?;

    let database = state.database().clone();
    let run = result.clone();
    std::thread::spawn(move || {
        let mut sink = TauriChatRunSink { app };
        ChatRunService::new(&database).run(&run, &mut sink);
    });

    Ok(result)
}

#[tauri::command]
pub fn get_connector_settings(
    state: State<'_, AppState>,
) -> Result<ConnectorSettingsSnapshot, String> {
    connector_settings_snapshot(state.database()).map_err(to_command_error)
}

#[tauri::command]
pub fn set_selected_model(
    state: State<'_, AppState>,
    provider_id: String,
    model_id: String,
) -> Result<ConnectorSettingsSnapshot, String> {
    let available_models = available_llm_models(state.database()).map_err(to_command_error)?;
    if !available_models
        .iter()
        .any(|model| model.provider_id == provider_id && model.id == model_id)
    {
        return Err(format!("unsupported model: {provider_id}/{model_id}"));
    }

    state
        .database()
        .set_selected_llm_model(&provider_id, &model_id)
        .map_err(to_command_error)?;

    connector_settings_snapshot(state.database()).map_err(to_command_error)
}

#[tauri::command]
pub fn save_adapter_settings(
    state: State<'_, AppState>,
    provider_id: String,
    values: BTreeMap<String, String>,
) -> Result<ConnectorSettingsSnapshot, String> {
    let database = state.database();
    let registry = mothership_adapter_host::AdapterRegistry::scan(&plugins_store_path(database));
    if registry.find(&provider_id).is_none() {
        return Err(format!("unknown adapter: {provider_id}"));
    }
    // Persist into the app's SHARED credential vault, keyed by provider — never
    // next to the adapter on disk. Merge so secrets the form doesn't carry (e.g.
    // an OAuth token the adapter stored itself) survive the save.
    let vault = FileCredentialVault::new(auth_store_path(database));
    vault
        .merge_adapter_settings(&provider_id, values)
        .map_err(to_command_error)?;
    connector_settings_snapshot(database).map_err(to_command_error)
}

/// Runs an adapter's own auth flow (e.g. Codex browser OAuth) on demand, driven
/// by the "Authorize" button. The adapter owns the flow end-to-end; the host
/// only spawns it, seeds any stored settings, wires the secret sink so the
/// resulting token lands in the shared vault, and waits for completion. Blocks
/// until the adapter acks (the user finishes the browser flow) or errors.
#[tauri::command]
pub fn authenticate_adapter(
    state: State<'_, AppState>,
    provider_id: String,
) -> Result<ConnectorSettingsSnapshot, String> {
    let database = state.database();
    let registry = mothership_adapter_host::AdapterRegistry::scan(&plugins_store_path(database));
    let entry = registry
        .find(&provider_id)
        .ok_or_else(|| format!("unknown adapter: {provider_id}"))?;

    let vault = FileCredentialVault::new(auth_store_path(database));
    let mut adapter =
        mothership_adapter_host::Adapter::spawn(&entry.program).map_err(|e| e.to_string())?;

    let sink_vault = vault.clone();
    let sink_provider = provider_id.clone();
    adapter.set_store_secret_handler(move |values| {
        if let Err(error) = sink_vault.merge_adapter_settings(&sink_provider, values) {
            eprintln!("failed to persist adapter secret for {sink_provider}: {error}");
        }
    });

    adapter.initialize().map_err(|e| e.to_string())?;
    let settings = vault
        .load_adapter_settings(&provider_id)
        .map_err(to_command_error)?;
    if !settings.is_empty() {
        adapter.set_settings(settings).map_err(|e| e.to_string())?;
    }

    // Register the process so `cancel_authenticate_adapter` (or leaving Settings)
    // can terminate this flow. `authenticate` blocks until the user finishes the
    // browser flow, cancels, or the adapter's own timeout fires.
    state.set_auth_process(&provider_id, adapter.process_id());
    let result = adapter.authenticate();
    // If our registration is already gone, a cancel took it and killed us — treat
    // that as a clean (not error) outcome.
    let cancelled = state.take_auth_process(&provider_id).is_none();
    drop(adapter);

    match result {
        Ok(()) => connector_settings_snapshot(database).map_err(to_command_error),
        Err(_) if cancelled => connector_settings_snapshot(database).map_err(to_command_error),
        Err(error) => Err(error.to_string()),
    }
}

/// Cancels an in-flight `authenticate` for `provider_id` by terminating the
/// adapter process (generic — works for any adapter's auth flow). Used by the
/// "Cancel" button and when the user leaves Settings mid-authorization.
#[tauri::command]
pub fn cancel_authenticate_adapter(
    state: State<'_, AppState>,
    provider_id: String,
) -> Result<ConnectorSettingsSnapshot, String> {
    if let Some(pid) = state.take_auth_process(&provider_id) {
        kill_process(pid);
    }
    connector_settings_snapshot(state.database()).map_err(to_command_error)
}

/// Best-effort terminate a child process by id (a spawned adapter). The host can
/// always kill an adapter — crash isolation is a core part of the contract.
fn kill_process(pid: u32) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
    #[cfg(unix)]
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output();
}

/// Logs an adapter out by forgetting its stored credential in the shared vault.
/// The host owns the credential store, so this is a host-side action — the
/// adapter re-authenticates from scratch next time. (Remote token revocation,
/// where a provider supports it, would later be an adapter-driven step.)
#[tauri::command]
pub fn logout_adapter(
    state: State<'_, AppState>,
    provider_id: String,
) -> Result<ConnectorSettingsSnapshot, String> {
    let database = state.database();
    let vault = FileCredentialVault::new(auth_store_path(database));
    vault
        .delete_adapter_settings(&provider_id)
        .map_err(to_command_error)?;
    connector_settings_snapshot(database).map_err(to_command_error)
}

#[tauri::command]
pub async fn run_sidecar_status(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SidecarStatus, String> {
    let database_path = state.database().path().to_path_buf();
    sidecar::status(&app, &database_path).await
}

fn to_command_error(error: mothership_core::MothershipError) -> String {
    error.to_string()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorSettingsSnapshot {
    providers: Vec<ConnectorProviderSummary>,
    selected_model: mothership_core::SelectedLlmModel,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorProviderSummary {
    id: String,
    label: String,
    /// The adapter's own icon as a data URI, if it ships one (declared in its
    /// manifest). The core just renders whatever the plugin provides.
    icon: Option<String>,
    settings_schema: mothership_core::ConnectorSettingsSchema,
    models: Vec<mothership_core::LlmModel>,
    selected_model_id: Option<String>,
    /// The adapter's auth scheme: `none` / `api_key` / `oauth_internal` /
    /// `external_process`. Drives whether the UI shows an "Authorize" button.
    auth_kind: String,
    /// Whether the adapter currently has a stored credential — toggles the UI
    /// between "Authorize" and "Log out".
    authenticated: bool,
    /// Present for subprocess adapters: the settings fields they declare plus
    /// their current values, so the UI can render and save a config form.
    adapter_settings: Option<AdapterSettingsView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdapterSettingsView {
    fields: Vec<AdapterSettingsFieldView>,
    values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdapterSettingsFieldView {
    key: String,
    label: String,
    kind: String,
    required: bool,
}

fn connector_settings_snapshot(
    database: &mothership_core::Database,
) -> mothership_core::Result<ConnectorSettingsSnapshot> {
    let models = available_llm_models(database)?;
    let selected_model = database.selected_llm_model()?;
    let adapter_settings = adapter_settings_views(database);
    let provider_labels = adapter_provider_labels(database);
    let provider_icons = adapter_provider_icons(database);

    Ok(ConnectorSettingsSnapshot {
        providers: connector_providers(
            models,
            &selected_model,
            &adapter_settings,
            &provider_labels,
            &provider_icons,
        ),
        selected_model,
    })
}

/// Models available to the UI. Every provider is a runtime-loaded adapter, so
/// the list is exactly what the installed adapters advertise.
fn available_llm_models(
    database: &mothership_core::Database,
) -> mothership_core::Result<Vec<mothership_core::LlmModel>> {
    Ok(plugin_models(database))
}

/// Loads provider-adapter plugins from the app's plugins directory and returns
/// the models they advertise, so a dropped-in plugin shows up in the picker
/// without rebuilding the app. Plugin failures are logged and skipped — a bad
/// plugin must never break the built-in model list.
fn plugin_models(database: &mothership_core::Database) -> Vec<mothership_core::LlmModel> {
    let registry = mothership_adapter_host::AdapterRegistry::scan(&plugins_store_path(database));
    let vault = FileCredentialVault::new(auth_store_path(database));
    let mut models = Vec::new();
    for entry in registry.entries() {
        match adapter_models(entry, &vault) {
            Ok(list) => models.extend(list),
            Err(error) => eprintln!("skipping adapter {}: {error}", entry.provider_id),
        }
    }
    models
}

/// Spawns an adapter just long enough to read its advertised models. Each call
/// starts and drops a child process; model listing is infrequent so this is
/// fine for now (a resident registry can come later). Settings (which can drive
/// the model list, e.g. OpenRouter's user-defined list) come from the shared
/// vault, keyed by provider.
fn adapter_models(
    entry: &mothership_adapter_host::AdapterEntry,
    vault: &FileCredentialVault,
) -> std::result::Result<Vec<mothership_core::LlmModel>, String> {
    let mut adapter = mothership_adapter_host::Adapter::spawn(&entry.program)
        .map_err(|error| error.to_string())?;
    adapter.initialize().map_err(|error| error.to_string())?;
    let settings = vault
        .load_adapter_settings(&entry.provider_id)
        .map_err(|error| error.to_string())?;
    if !settings.is_empty() {
        adapter
            .set_settings(settings)
            .map_err(|error| error.to_string())?;
    }
    let (models, _management) = adapter.models().map_err(|error| error.to_string())?;
    Ok(models
        .into_iter()
        .map(|model| mothership_core::LlmModel {
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

/// Per-adapter UI info gathered from one spawn: the settings form (declared
/// fields + current values from the vault), the adapter's auth scheme, and
/// whether it currently has a stored credential (drives Authorize vs Log out).
struct AdapterInfo {
    view: AdapterSettingsView,
    auth_kind: String,
    authenticated: bool,
}

/// Spawns each installed adapter once to read the settings fields it declares
/// (paired with current vault values) and its auth scheme. Used by the UI to
/// render the per-adapter config form and decide whether to show "Authorize".
fn adapter_settings_views(
    database: &mothership_core::Database,
) -> BTreeMap<String, AdapterInfo> {
    let registry = mothership_adapter_host::AdapterRegistry::scan(&plugins_store_path(database));
    let vault = FileCredentialVault::new(auth_store_path(database));
    let mut views = BTreeMap::new();
    for entry in registry.entries() {
        if let Some(info) = adapter_settings_view(entry, &vault) {
            views.insert(entry.provider_id.clone(), info);
        }
    }
    views
}

fn adapter_settings_view(
    entry: &mothership_adapter_host::AdapterEntry,
    vault: &FileCredentialVault,
) -> Option<AdapterInfo> {
    use mothership_adapter_host::protocol::{AuthKind, SettingsFieldKind};

    let mut adapter = mothership_adapter_host::Adapter::spawn(&entry.program).ok()?;
    adapter.initialize().ok()?;
    let fields = adapter.settings_schema().ok()?;
    let auth_kind = match adapter.auth_schema().ok()? {
        AuthKind::None => "none",
        AuthKind::ApiKey { .. } => "api_key",
        AuthKind::OauthInternal => "oauth_internal",
        AuthKind::ExternalProcess => "external_process",
    }
    .to_string();

    let values = vault
        .load_adapter_settings(&entry.provider_id)
        .unwrap_or_default();
    // The host owns the credential store, so it knows the auth STATUS: an
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

fn plugins_store_path(database: &mothership_core::Database) -> PathBuf {
    database
        .path()
        .parent()
        .map(|path| path.join("plugins"))
        .unwrap_or_else(|| PathBuf::from("plugins"))
}

/// Maps each installed adapter's `provider_id` to its human label from the
/// manifest — cheap (no process spawn), so the UI can name a connector even
/// before any of its models have loaded.
fn adapter_provider_labels(
    database: &mothership_core::Database,
) -> BTreeMap<String, String> {
    mothership_adapter_host::AdapterRegistry::scan(&plugins_store_path(database))
        .entries()
        .iter()
        .map(|entry| (entry.provider_id.clone(), entry.provider_label.clone()))
        .collect()
}

/// Maps each installed adapter's `provider_id` to its icon as a data URI, if it
/// ships one. The adapter declares the icon file in its manifest; the host just
/// reads + inlines it so the webview can render it without disk access.
fn adapter_provider_icons(
    database: &mothership_core::Database,
) -> BTreeMap<String, String> {
    mothership_adapter_host::AdapterRegistry::scan(&plugins_store_path(database))
        .entries()
        .iter()
        .filter_map(|entry| {
            let uri = icon_data_uri(entry.icon.as_deref()?)?;
            Some((entry.provider_id.clone(), uri))
        })
        .collect()
}

/// Reads an icon file and encodes it as a `data:` URI (base64). Returns `None`
/// if the file is missing or the extension isn't a known image type.
fn icon_data_uri(path: &std::path::Path) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    let mime = match path.extension().and_then(|ext| ext.to_str())?.to_lowercase().as_str() {
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

/// Thin event adapter: forwards Core's chat-run events to the desktop UI.
///
/// All run orchestration and database writes live in
/// [`mothership_core::ChatRunService`]; the host only plumbs the resulting
/// events to the Tauri front end via the `chat-run-event` channel.
struct TauriChatRunSink {
    app: AppHandle,
}

impl ChatRunEventSink for TauriChatRunSink {
    fn emit(&mut self, event: ChatRunEvent) {
        let _ = self.app.emit("chat-run-event", event);
    }
}

fn connector_providers(
    models: Vec<mothership_core::LlmModel>,
    selected_model: &mothership_core::SelectedLlmModel,
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
                .or_else(|| provider_models.first().map(|model| model.provider_label.clone()))
                .unwrap_or_else(|| provider_id.clone());
            let selected_model_id = (selected_model.provider_id == provider_id)
                .then(|| selected_model.model_id.clone());
            let info = adapter_info.get(&provider_id);
            let auth_kind = info
                .map(|info| info.auth_kind.clone())
                .unwrap_or_else(|| "none".to_string());
            let authenticated = info.map(|info| info.authenticated).unwrap_or(false);
            let adapter_settings_view = info.map(|info| info.view.clone());
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
                adapter_settings: adapter_settings_view,
            }
        })
        .collect()
}

fn default_connector_settings_schema() -> mothership_core::ConnectorSettingsSchema {
    mothership_core::ConnectorSettingsSchema {
        model_management: mothership_core::ConnectorModelManagementSchema {
            kind: mothership_core::ConnectorModelManagementKind::FixedCatalog,
            title: "Models".to_string(),
            description: "This connector does not expose model management settings yet."
                .to_string(),
            add_model_label: None,
        },
    }
}

fn auth_store_path(database: &mothership_core::Database) -> PathBuf {
    database
        .path()
        .parent()
        .map(|path| path.join("auth"))
        .unwrap_or_else(|| PathBuf::from("auth"))
}
