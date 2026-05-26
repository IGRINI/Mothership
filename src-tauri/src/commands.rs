use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use mothership_core::{
    auth::{
        AuthMethod, AuthSession, CompleteAuthRequest, FileCredentialVault, OpenAiCodexOAuthAdapter,
        ProviderAuthService, ProviderConnection, ProviderConnectionId, ProviderDescriptor,
        StartAuthRequest, StaticProviderAuthAdapterRegistry,
    },
    ChatConversation, ChatRunEvent, ChatRunEventSink, ChatRunService, ChatThreadSummary,
    DashboardSnapshot, SendChatMessageResult, SidecarStatus,
};
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, State};

use crate::{sidecar, state::AppState};

const OAUTH_LISTENER_CANCELLED: &str = "OAuth listener cancelled";

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
pub fn start_provider_auth(
    state: State<'_, AppState>,
    provider_id: String,
    auth_method_id: String,
) -> Result<AuthSession, String> {
    with_auth_service(state.database(), |auth| {
        auth.start_auth(StartAuthRequest {
            provider_id: provider_id.into(),
            auth_method_id: auth_method_id.into(),
        })
    })
    .map_err(to_command_error)
}

#[tauri::command]
pub fn complete_provider_auth(
    state: State<'_, AppState>,
    session_id: String,
    callback_url: String,
) -> Result<ConnectorSettingsSnapshot, String> {
    with_auth_service(state.database(), |auth| {
        auth.complete_auth(CompleteAuthRequest {
            session_id: session_id.into(),
            payload: json!({ "callbackUrl": callback_url }),
        })
    })
    .map_err(to_command_error)?;

    state.finish_oauth_listener();
    connector_settings_snapshot(state.database()).map_err(to_command_error)
}

#[tauri::command]
pub fn disconnect_provider_connection(
    state: State<'_, AppState>,
    connection_id: String,
) -> Result<ConnectorSettingsSnapshot, String> {
    with_auth_service(state.database(), |auth| {
        auth.disconnect(&ProviderConnectionId::from(connection_id))
    })
    .map_err(to_command_error)?;

    connector_settings_snapshot(state.database()).map_err(to_command_error)
}

#[tauri::command]
pub fn start_provider_oauth_login(
    app: AppHandle,
    state: State<'_, AppState>,
    provider_id: String,
    auth_method_id: String,
) -> Result<AuthSession, String> {
    if provider_id != OpenAiCodexOAuthAdapter::PROVIDER_ID
        || auth_method_id != OpenAiCodexOAuthAdapter::AUTH_METHOD_ID
    {
        return Err("browser OAuth listener is only implemented for OpenAI Codex".to_string());
    }

    if !state.try_begin_oauth_listener() {
        return Err(
            "OAuth authorization is already in progress. Finish the current browser flow first."
                .to_string(),
        );
    }

    let listener = match TcpListener::bind("127.0.0.1:1455") {
        Ok(listener) => listener,
        Err(error) => {
            state.finish_oauth_listener();
            return Err(format!("failed to bind OAuth callback listener: {error}"));
        }
    };

    if let Err(error) = listener.set_nonblocking(true) {
        state.finish_oauth_listener();
        return Err(format!(
            "failed to configure OAuth callback listener: {error}"
        ));
    }

    let session = match with_auth_service(state.database(), |auth| {
        auth.start_auth(StartAuthRequest {
            provider_id: provider_id.into(),
            auth_method_id: auth_method_id.into(),
        })
    }) {
        Ok(session) => session,
        Err(error) => {
            state.finish_oauth_listener();
            return Err(to_command_error(error));
        }
    };

    let database = state.database().clone();
    let session_id = session.id.clone();
    let app_handle = app.clone();
    let listener_active = state.oauth_listener_active();
    let wait_active = state.oauth_listener_active();
    std::thread::spawn(move || {
        let result = wait_for_oauth_callback(listener, &wait_active).and_then(|callback_url| {
            with_auth_service(&database, |auth| {
                auth.complete_auth(CompleteAuthRequest {
                    session_id,
                    payload: json!({ "callbackUrl": callback_url }),
                })
            })
            .map_err(|error| error.to_string())
        });

        match result {
            Ok(connection) => {
                let _ =
                    app_handle.emit("connector-auth-completed", connection_summary(&connection));
            }
            Err(error) if error == OAUTH_LISTENER_CANCELLED => {}
            Err(error) => {
                let _ = app_handle.emit("connector-auth-failed", error);
            }
        }

        listener_active.store(false, Ordering::SeqCst);
    });

    Ok(session)
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
    status: String,
    settings_schema: mothership_core::ConnectorSettingsSchema,
    auth_methods: Vec<ConnectorAuthMethodSummary>,
    connections: Vec<ConnectorConnectionSummary>,
    models: Vec<mothership_core::LlmModel>,
    selected_model_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorAuthMethodSummary {
    id: String,
    kind: String,
    label: String,
    description: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorConnectionSummary {
    id: String,
    provider_id: String,
    auth_method_id: String,
    status: String,
    account_label: Option<String>,
    account_email: Option<String>,
    expires_at: Option<String>,
    updated_at: String,
}

fn connector_settings_snapshot(
    database: &mothership_core::Database,
) -> mothership_core::Result<ConnectorSettingsSnapshot> {
    let providers = with_auth_service(database, |auth| {
        let providers = auth.list_providers();
        let connections = auth.list_connections()?;
        Ok((providers, connections))
    })?;
    let models = available_llm_models_for_connections(database, &providers.1)?;
    let selected_model = database.selected_llm_model()?;

    Ok(ConnectorSettingsSnapshot {
        providers: connector_providers(providers.0, providers.1, models, &selected_model),
        selected_model,
    })
}

fn available_llm_models(
    database: &mothership_core::Database,
) -> mothership_core::Result<Vec<mothership_core::LlmModel>> {
    let connections = with_auth_service(database, |auth| auth.list_connections())?;
    available_llm_models_for_connections(database, &connections)
}

fn available_llm_models_for_connections(
    database: &mothership_core::Database,
    connections: &[ProviderConnection],
) -> mothership_core::Result<Vec<mothership_core::LlmModel>> {
    let vault = FileCredentialVault::new(auth_store_path(database));
    let llm_registry = mothership_core::default_llm_registry();
    let model_catalog =
        mothership_core::LlmModelCatalogService::new(database, &vault, &llm_registry);
    let mut models = model_catalog.list_models(connections)?;
    models.extend(plugin_models(database));
    Ok(models)
}

/// Loads provider-adapter plugins from the app's plugins directory and returns
/// the models they advertise, so a dropped-in plugin shows up in the picker
/// without rebuilding the app. Plugin failures are logged and skipped — a bad
/// plugin must never break the built-in model list.
fn plugin_models(database: &mothership_core::Database) -> Vec<mothership_core::LlmModel> {
    let host = match mothership_plugin_host::PluginHost::new() {
        Ok(host) => host,
        Err(error) => {
            eprintln!("plugin host unavailable: {error}");
            return Vec::new();
        }
    };

    let mut models = Vec::new();
    for (path, manifest) in host.scan(&plugins_store_path(database)) {
        match manifest {
            Ok(manifest) => {
                for model in manifest.models {
                    models.push(mothership_core::LlmModel {
                        provider_id: manifest.provider_id.clone(),
                        provider_label: manifest.provider_label.clone(),
                        id: model.id,
                        label: model.label,
                        family: model.family,
                        description: model.description,
                        capabilities: vec!["text".to_string()],
                        recommended: model.recommended,
                    });
                }
            }
            Err(error) => eprintln!("skipping plugin {}: {error}", path.display()),
        }
    }
    models
}

fn plugins_store_path(database: &mothership_core::Database) -> PathBuf {
    database
        .path()
        .parent()
        .map(|path| path.join("plugins"))
        .unwrap_or_else(|| PathBuf::from("plugins"))
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
    auth_providers: Vec<ProviderDescriptor>,
    connections: Vec<ProviderConnection>,
    models: Vec<mothership_core::LlmModel>,
    selected_model: &mothership_core::SelectedLlmModel,
) -> Vec<ConnectorProviderSummary> {
    let llm_registry = mothership_core::default_llm_registry();
    let mut provider_ids = BTreeSet::new();
    for provider in &auth_providers {
        provider_ids.insert(provider.id.as_str().to_string());
    }
    for model in &models {
        provider_ids.insert(model.provider_id.clone());
    }
    for connection in &connections {
        provider_ids.insert(connection.provider_id.as_str().to_string());
    }

    let auth_by_provider: BTreeMap<String, ProviderDescriptor> = auth_providers
        .into_iter()
        .map(|provider| (provider.id.as_str().to_string(), provider))
        .collect();

    provider_ids
        .into_iter()
        .map(|provider_id| {
            let auth_provider = auth_by_provider.get(&provider_id);
            let settings_schema = llm_registry
                .settings_schema(&provider_id)
                .unwrap_or_else(default_connector_settings_schema);
            let provider_models = models
                .iter()
                .filter(|model| model.provider_id == provider_id)
                .cloned()
                .collect::<Vec<_>>();
            let provider_connections = connections
                .iter()
                .filter(|connection| connection.provider_id.as_str() == provider_id)
                .map(connection_summary)
                .collect::<Vec<_>>();
            let has_active_connection = provider_connections
                .iter()
                .any(|connection| connection.status == "active");
            let selected_model_id = (selected_model.provider_id == provider_id)
                .then(|| selected_model.model_id.clone());

            ConnectorProviderSummary {
                id: provider_id.clone(),
                label: auth_provider
                    .map(|provider| provider.label.clone())
                    .or_else(|| {
                        llm_registry
                            .provider_label(&provider_id)
                            .map(ToOwned::to_owned)
                    })
                    .or_else(|| {
                        provider_models
                            .first()
                            .map(|model| model.provider_label.clone())
                    })
                    .unwrap_or(provider_id),
                settings_schema,
                status: if has_active_connection {
                    "connected".to_string()
                } else if auth_provider
                    .map(|provider| !provider.methods.is_empty())
                    .unwrap_or(false)
                {
                    "not_connected".to_string()
                } else {
                    "not_available".to_string()
                },
                auth_methods: auth_provider
                    .map(|provider| {
                        provider
                            .methods
                            .iter()
                            .map(auth_method_summary)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                connections: provider_connections,
                models: provider_models,
                selected_model_id,
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

fn auth_method_summary(method: &AuthMethod) -> ConnectorAuthMethodSummary {
    ConnectorAuthMethodSummary {
        id: method.id.as_str().to_string(),
        kind: serde_json::to_value(&method.kind)
            .ok()
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "unknown".to_string()),
        label: method.label.clone(),
        description: method.description.clone(),
    }
}

fn connection_summary(connection: &ProviderConnection) -> ConnectorConnectionSummary {
    ConnectorConnectionSummary {
        id: connection.id.as_str().to_string(),
        provider_id: connection.provider_id.as_str().to_string(),
        auth_method_id: connection.auth_method_id.as_str().to_string(),
        status: serde_json::to_value(&connection.status)
            .ok()
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "unknown".to_string()),
        account_label: connection.account_label.clone(),
        account_email: connection.account_email.clone(),
        expires_at: connection.expires_at.clone(),
        updated_at: connection.updated_at.clone(),
    }
}

fn with_auth_service<T>(
    database: &mothership_core::Database,
    run: impl FnOnce(&ProviderAuthService<'_>) -> mothership_core::Result<T>,
) -> mothership_core::Result<T> {
    let vault = FileCredentialVault::new(auth_store_path(database));
    let registry = StaticProviderAuthAdapterRegistry::with_openai_codex();
    let service = ProviderAuthService::new(database, &vault, &registry);
    run(&service)
}

fn auth_store_path(database: &mothership_core::Database) -> PathBuf {
    database
        .path()
        .parent()
        .map(|path| path.join("auth"))
        .unwrap_or_else(|| PathBuf::from("auth"))
}

fn wait_for_oauth_callback(listener: TcpListener, active: &AtomicBool) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(300);

    loop {
        if !active.load(Ordering::SeqCst) {
            return Err(OAUTH_LISTENER_CANCELLED.to_string());
        }

        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut reader =
                    BufReader::new(stream.try_clone().map_err(|error| error.to_string())?);
                let mut request_line = String::new();
                reader
                    .read_line(&mut request_line)
                    .map_err(|error| error.to_string())?;
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .ok_or_else(|| "invalid OAuth callback request".to_string())?;
                let callback_url = format!(
                    "{}{}",
                    OpenAiCodexOAuthAdapter::default_redirect_uri(),
                    path.strip_prefix("/auth/callback").unwrap_or_default()
                );
                let body = oauth_callback_success_page();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-security-policy: default-src 'none'; style-src 'unsafe-inline'\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .map_err(|error| error.to_string())?;
                return Ok(callback_url);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err("OAuth callback timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn oauth_callback_success_page() -> &'static str {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Mothership Authorization</title>
  <style>
    :root {
      color-scheme: dark;
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #050a10;
      color: #eaf1f8;
    }

    * {
      box-sizing: border-box;
    }

    html,
    body {
      width: 100%;
      min-height: 100%;
      margin: 0;
    }

    body {
      display: grid;
      min-height: 100vh;
      place-items: center;
      padding: 24px;
      background:
        radial-gradient(circle at 50% -20%, rgb(33 108 227 / 22%), transparent 42%),
        linear-gradient(180deg, #07101a 0%, #050a10 100%);
    }

    main {
      width: min(520px, 100%);
      border: 1px solid #213143;
      border-radius: 12px;
      padding: 28px;
      background: #09131f;
      box-shadow:
        0 24px 80px rgb(0 0 0 / 42%),
        0 0 0 1px rgb(35 124 255 / 10%);
    }

    .brand {
      display: flex;
      align-items: center;
      gap: 12px;
      margin-bottom: 26px;
    }

    .mark {
      position: relative;
      display: grid;
      width: 42px;
      height: 42px;
      place-items: center;
      border: 1px solid #285080;
      border-radius: 10px;
      background: linear-gradient(145deg, #0b2540, #0e66ff);
      box-shadow: 0 12px 34px rgb(13 99 255 / 28%);
    }

    .mark::before,
    .mark::after {
      position: absolute;
      border: 2px solid rgb(255 255 255 / 86%);
      content: "";
    }

    .mark::before {
      width: 20px;
      height: 20px;
      border-radius: 999px;
    }

    .mark::after {
      width: 7px;
      height: 7px;
      border-top: 0;
      border-left: 0;
      transform: translate(10px, 10px);
    }

    .brand strong {
      color: #f5f8fc;
      font-size: 18px;
      font-weight: 800;
      letter-spacing: 0;
    }

    .brand span {
      display: block;
      margin-top: 2px;
      color: #7f8d9d;
      font-size: 13px;
      font-weight: 650;
    }

    .status {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      border: 1px solid #1d5a39;
      border-radius: 999px;
      padding: 7px 11px;
      background: #0c2118;
      color: #7df2a4;
      font-size: 13px;
      font-weight: 800;
    }

    .status::before {
      width: 8px;
      height: 8px;
      border-radius: 999px;
      background: #41d981;
      box-shadow: 0 0 0 4px rgb(65 217 129 / 14%);
      content: "";
    }

    h1 {
      margin: 18px 0 10px;
      color: #f3f7fb;
      font-size: clamp(28px, 7vw, 42px);
      line-height: 1.06;
      letter-spacing: 0;
    }

    p {
      margin: 0;
      color: #9caaba;
      font-size: 15px;
      line-height: 1.6;
    }

    .footer {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      margin-top: 28px;
      border-top: 1px solid #1b2a3a;
      padding-top: 16px;
      color: #708092;
      font-size: 12px;
      font-weight: 700;
    }

    .pill {
      border: 1px solid #27394b;
      border-radius: 999px;
      padding: 5px 9px;
      background: #101b27;
      color: #b9c7d6;
    }
  </style>
</head>
<body>
  <main>
    <div class="brand">
      <div class="mark" aria-hidden="true"></div>
      <div>
        <strong>Mothership</strong>
        <span>Local provider authorization</span>
      </div>
    </div>

    <div class="status">Authorization received</div>
    <h1>You are connected.</h1>
    <p>Mothership has received the OAuth callback and is finishing the secure local connection. You can close this browser window and return to the desktop app.</p>

    <div class="footer">
      <span>Credentials stay on this device</span>
      <span class="pill">OAuth callback complete</span>
    </div>
  </main>
</body>
</html>"#
}
