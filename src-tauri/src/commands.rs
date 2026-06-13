//! Tauri command surface: a thin pass-through to the Core sidecar.
//!
//! Every command serializes a [`CoreRequest`], awaits the sidecar's single
//! terminal reply, and unwraps the matching [`CoreResponse`] variant. No
//! database, adapter, or vault logic lives here — that all moved into Core
//! (run in the sidecar process). Streaming chat output is not returned here; it
//! arrives as `chat-run-event`s the sidecar supervisor forwards to the webview.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use base64::Engine;
use mothership_core::ipc::{CoreRequest, CoreResponse};
use mothership_core::{
    ActiveRunSummary, AdapterSettingPatchValue, ChangeFileDiff, ChangeFileSummary,
    ChangeSetSummary, ChatConversation, ChatRunCancellationResult, ChatThreadSummary,
    ConnectorSettingsSnapshot,
    DashboardSnapshot, PersonalizationSettings, ProjectSnapshot, PromptPreview, ReasoningConfig,
    RevertOutcome, SendChatMessageResult, SidecarStatus, ToolApprovalAnswer, ToolArtifactRange,
    ToolExecutionAccepted, ToolExecutionCancellationResult, ToolExecutionRequest,
    ToolPolicySettings,
};
use serde_json::Value;
use tauri::{AppHandle, Manager, State, Window};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::sidecar::SidecarHealth;
use crate::state::AppState;

const MAX_IMAGE_PREVIEW_BYTES: u64 = 25 * 1024 * 1024;

/// Unwraps the expected [`CoreResponse`] variant, or turns an unexpected reply
/// into a command error (only possible on a protocol bug / version skew).
macro_rules! expect_variant {
    ($response:expr, $variant:path) => {
        match $response {
            $variant(value) => Ok(value),
            other => Err(format!("unexpected sidecar response: {other:?}")),
        }
    };
}

fn resolve_artifact_path(app: &AppHandle, path: &str) -> Result<PathBuf, String> {
    let raw = PathBuf::from(path);
    if !raw.is_absolute() {
        return Err("artifact path must be absolute".to_string());
    }

    let artifacts_root = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("artifacts");
    let artifacts_root = std::fs::canonicalize(&artifacts_root)
        .map_err(|error| format!("artifact root is unavailable: {error}"))?;
    let resolved = std::fs::canonicalize(&raw)
        .map_err(|error| format!("artifact path is unavailable: {error}"))?;

    if !resolved.starts_with(&artifacts_root) {
        return Err("artifact path is outside the Mothership artifacts directory".to_string());
    }
    Ok(resolved)
}

fn image_data_url(path: &Path) -> Result<String, String> {
    let content_type =
        image_content_type(path).ok_or_else(|| "file is not a supported image".to_string())?;
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("image path is not a file".to_string());
    }
    if metadata.len() > MAX_IMAGE_PREVIEW_BYTES {
        return Err(format!(
            "image is too large for inline preview ({} bytes)",
            metadata.len()
        ));
    }

    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{content_type};base64,{encoded}"))
}

fn image_content_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "apng" => Some("image/apng"),
        "avif" => Some("image/avif"),
        "bmp" => Some("image/bmp"),
        "gif" => Some("image/gif"),
        "ico" => Some("image/x-icon"),
        "jfif" | "jpeg" | "jpg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "svg" => Some("image/svg+xml"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[tauri::command]
pub async fn get_dashboard_snapshot(
    state: State<'_, AppState>,
) -> Result<DashboardSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::DashboardSnapshot)
        .await?;
    expect_variant!(response, CoreResponse::Dashboard)
}

#[tauri::command]
pub async fn append_activity_event(
    state: State<'_, AppState>,
    message: String,
) -> Result<DashboardSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::AppendActivityEvent { message })
        .await?;
    expect_variant!(response, CoreResponse::Dashboard)
}

#[tauri::command]
pub async fn list_chats(
    state: State<'_, AppState>,
    project_id: Option<String>,
    limit: Option<i64>,
) -> Result<Vec<ChatThreadSummary>, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ListChats { project_id, limit })
        .await?;
    expect_variant!(response, CoreResponse::ChatList)
}

#[tauri::command]
pub async fn create_chat(
    state: State<'_, AppState>,
    project_id: String,
    copy_from_chat_id: Option<String>,
) -> Result<ChatConversation, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::CreateChat {
            project_id,
            copy_from_chat_id,
        })
        .await?;
    expect_variant!(response, CoreResponse::Chat)
}

#[tauri::command]
pub async fn get_chat(
    state: State<'_, AppState>,
    chat_id: String,
    limit: Option<i64>,
) -> Result<ChatConversation, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::GetChat { chat_id, limit })
        .await?;
    expect_variant!(response, CoreResponse::Chat)
}

#[tauri::command]
pub async fn get_prompt_preview(
    state: State<'_, AppState>,
    chat_id: String,
) -> Result<PromptPreview, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::GetPromptPreview { chat_id })
        .await?;
    expect_variant!(response, CoreResponse::PromptPreview)
}

#[tauri::command]
pub async fn send_chat_message(
    state: State<'_, AppState>,
    chat_id: Option<String>,
    project_id: Option<String>,
    content: String,
    reasoning: Option<ReasoningConfig>,
    fast_mode: Option<bool>,
) -> Result<SendChatMessageResult, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SendChatMessage {
            chat_id,
            project_id,
            content,
            reasoning,
            fast_mode: fast_mode.unwrap_or(false),
        })
        .await?;
    expect_variant!(response, CoreResponse::ChatMessageStarted)
}

#[tauri::command]
pub async fn list_projects(state: State<'_, AppState>) -> Result<ProjectSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ListProjects)
        .await?;
    expect_variant!(response, CoreResponse::ProjectSnapshot)
}

#[tauri::command]
pub async fn pick_project_directory(window: Window) -> Result<Option<String>, String> {
    #[cfg(not(desktop))]
    {
        let _ = window;
        return Ok(None);
    }

    #[cfg(desktop)]
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        window
            .dialog()
            .file()
            .set_parent(&window)
            .set_title("Open project folder")
            .pick_folder(move |folder| {
                let selected_path = folder
                    .map(|path| {
                        path.into_path()
                            .map(|path| path.to_string_lossy().into_owned())
                            .map_err(|error| error.to_string())
                    })
                    .transpose();

                let _ = sender.send(selected_path);
            });

        receiver
            .await
            .map_err(|_| "project directory picker was interrupted".to_string())?
    }
}

#[tauri::command]
pub async fn open_project(
    state: State<'_, AppState>,
    path: String,
) -> Result<ProjectSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::OpenProject { path })
        .await?;
    expect_variant!(response, CoreResponse::ProjectSnapshot)
}

#[tauri::command]
pub async fn set_active_project(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<ProjectSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SetActiveProject { project_id })
        .await?;
    expect_variant!(response, CoreResponse::ProjectSnapshot)
}

#[tauri::command]
pub async fn edit_chat_user_message(
    state: State<'_, AppState>,
    chat_id: String,
    message_id: String,
    content: String,
) -> Result<SendChatMessageResult, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::EditChatUserMessage {
            chat_id,
            message_id,
            content,
        })
        .await?;
    expect_variant!(response, CoreResponse::ChatMessageStarted)
}

#[tauri::command]
pub async fn branch_chat_from_message(
    state: State<'_, AppState>,
    chat_id: String,
    message_id: String,
) -> Result<ChatConversation, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::BranchChatFromMessage {
            chat_id,
            message_id,
        })
        .await?;
    expect_variant!(response, CoreResponse::Chat)
}

#[tauri::command]
pub async fn retry_chat_message(
    state: State<'_, AppState>,
    chat_id: String,
) -> Result<SendChatMessageResult, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::RetryChatMessage { chat_id })
        .await?;
    expect_variant!(response, CoreResponse::ChatMessageStarted)
}

#[tauri::command]
pub async fn continue_chat_message(
    state: State<'_, AppState>,
    chat_id: String,
) -> Result<SendChatMessageResult, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ContinueChatMessage { chat_id })
        .await?;
    expect_variant!(response, CoreResponse::ChatMessageStarted)
}

#[tauri::command]
pub async fn cancel_chat_run(
    state: State<'_, AppState>,
    run_id: String,
) -> Result<ChatRunCancellationResult, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::CancelChatRun { run_id })
        .await?;
    expect_variant!(response, CoreResponse::ChatRunCancellation)
}

#[tauri::command]
pub async fn list_active_runs(
    state: State<'_, AppState>,
) -> Result<Vec<ActiveRunSummary>, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ListActiveRuns)
        .await?;
    expect_variant!(response, CoreResponse::ActiveRuns)
}

#[tauri::command]
pub async fn rename_chat(
    state: State<'_, AppState>,
    chat_id: String,
    title: String,
) -> Result<ChatThreadSummary, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::RenameChat { chat_id, title })
        .await?;
    expect_variant!(response, CoreResponse::ChatSummary)
}

#[tauri::command]
pub async fn delete_chat(state: State<'_, AppState>, chat_id: String) -> Result<(), String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::DeleteChat { chat_id })
        .await?;
    match response {
        CoreResponse::Ack => Ok(()),
        other => Err(format!("unexpected sidecar response: {other:?}")),
    }
}

#[tauri::command]
pub async fn rename_project(
    state: State<'_, AppState>,
    project_id: String,
    name: String,
) -> Result<ProjectSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::RenameProject { project_id, name })
        .await?;
    expect_variant!(response, CoreResponse::ProjectSnapshot)
}

#[tauri::command]
pub async fn delete_project(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<ProjectSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::DeleteProject { project_id })
        .await?;
    expect_variant!(response, CoreResponse::ProjectSnapshot)
}

#[tauri::command]
pub async fn set_project_appearance(
    state: State<'_, AppState>,
    project_id: String,
    icon: Option<String>,
    icon_color: Option<String>,
) -> Result<ProjectSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SetProjectAppearance {
            project_id,
            icon,
            icon_color,
        })
        .await?;
    expect_variant!(response, CoreResponse::ProjectSnapshot)
}

#[tauri::command]
pub async fn run_tool_command(
    state: State<'_, AppState>,
    request: ToolExecutionRequest,
) -> Result<ToolExecutionAccepted, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::RunToolCommand { request })
        .await?;
    expect_variant!(response, CoreResponse::ToolExecutionAccepted)
}

#[tauri::command]
pub async fn approve_tool_execution(
    state: State<'_, AppState>,
    tool_call_id: String,
    approved: bool,
    reason: Option<String>,
) -> Result<ToolApprovalAnswer, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ApproveToolExecution {
            tool_call_id,
            approved,
            reason,
        })
        .await?;
    expect_variant!(response, CoreResponse::ToolApproval)
}

#[tauri::command]
pub async fn cancel_tool_execution(
    state: State<'_, AppState>,
    tool_call_id: String,
) -> Result<ToolExecutionCancellationResult, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::CancelToolExecution { tool_call_id })
        .await?;
    expect_variant!(response, CoreResponse::ToolExecutionCancellation)
}

#[tauri::command]
pub async fn get_personalization(
    state: State<'_, AppState>,
) -> Result<PersonalizationSettings, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::GetPersonalization)
        .await?;
    expect_variant!(response, CoreResponse::Personalization)
}

#[tauri::command]
pub async fn set_personalization(
    state: State<'_, AppState>,
    provider_id: Option<String>,
    model_id: Option<String>,
    content: String,
) -> Result<PersonalizationSettings, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SetPersonalization {
            provider_id,
            model_id,
            content,
        })
        .await?;
    expect_variant!(response, CoreResponse::Personalization)
}

#[tauri::command]
pub async fn get_tool_policy(state: State<'_, AppState>) -> Result<ToolPolicySettings, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::GetToolPolicy)
        .await?;
    expect_variant!(response, CoreResponse::ToolPolicy)
}

#[tauri::command]
pub async fn set_tool_policy(
    state: State<'_, AppState>,
    settings: ToolPolicySettings,
) -> Result<ToolPolicySettings, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SetToolPolicy { settings })
        .await?;
    expect_variant!(response, CoreResponse::ToolPolicy)
}

#[tauri::command]
pub async fn get_tool_artifact_range(
    state: State<'_, AppState>,
    tool_call_id: String,
    log_ref: String,
    offset: u64,
    limit: u64,
) -> Result<ToolArtifactRange, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::GetToolArtifactRange {
            tool_call_id,
            log_ref,
            offset,
            limit,
        })
        .await?;
    expect_variant!(response, CoreResponse::ToolArtifactRange)
}

/// Open a workspace path externally. Core resolves + contains the path against
/// the owning project's root first; we never hand a raw UI string to the opener.
#[tauri::command]
pub async fn open_tool_path(
    app: AppHandle,
    state: State<'_, AppState>,
    project_id: Option<String>,
    path: String,
) -> Result<(), String> {
    let Some(project_id) = project_id else {
        return Err("cannot open a path without a project".to_string());
    };
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ResolveWorkspacePath { project_id, path })
        .await?;
    let resolved = expect_variant!(response, CoreResponse::ResolvedPath)?;
    app.opener()
        .open_path(resolved, None::<&str>)
        .map_err(|error| error.to_string())
}

/// Resolve a workspace path without opening it. Used by the UI to render safe
/// local previews after Core has enforced project containment.
#[tauri::command]
pub async fn resolve_tool_path(
    state: State<'_, AppState>,
    project_id: Option<String>,
    path: String,
) -> Result<String, String> {
    let Some(project_id) = project_id else {
        return Err("cannot resolve a path without a project".to_string());
    };
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ResolveWorkspacePath { project_id, path })
        .await?;
    expect_variant!(response, CoreResponse::ResolvedPath)
}

#[tauri::command]
pub async fn read_image_data_url(
    state: State<'_, AppState>,
    project_id: Option<String>,
    path: String,
) -> Result<String, String> {
    // Closed, single-user desktop app with a trusted agent: any absolute local
    // image path may be previewed. The UI only ever hands us local file paths
    // (internet refs are filtered out before they reach a preview), and
    // `image_data_url` still enforces "is an image" + the 25 MB size cap.
    // Relative paths are resolved against the owning project's workspace.
    let candidate = PathBuf::from(&path);
    if candidate.is_absolute() {
        return image_data_url(&candidate);
    }

    let Some(project_id) = project_id else {
        return Err("cannot resolve a relative image path without a project".to_string());
    };
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ResolveWorkspacePath { project_id, path })
        .await?;
    let resolved = PathBuf::from(expect_variant!(response, CoreResponse::ResolvedPath)?);
    image_data_url(&resolved)
}

#[tauri::command]
pub async fn open_artifact_path(app: AppHandle, path: String) -> Result<(), String> {
    let resolved = resolve_artifact_path(&app, &path)?;
    app.opener()
        .open_path(resolved.display().to_string(), None::<&str>)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn reveal_artifact_path(app: AppHandle, path: String) -> Result<(), String> {
    let resolved = resolve_artifact_path(&app, &path)?;
    app.opener()
        .reveal_item_in_dir(resolved.display().to_string())
        .map_err(|error| error.to_string())
}

/// Reveal a workspace path in the OS file manager (Explorer / Finder). Like
/// [`open_tool_path`], Core resolves + contains the path against the owning
/// project's root before it reaches the opener.
#[tauri::command]
pub async fn reveal_tool_path(
    app: AppHandle,
    state: State<'_, AppState>,
    project_id: Option<String>,
    path: String,
) -> Result<(), String> {
    let Some(project_id) = project_id else {
        return Err("cannot reveal a path without a project".to_string());
    };
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ResolveWorkspacePath { project_id, path })
        .await?;
    let resolved = expect_variant!(response, CoreResponse::ResolvedPath)?;
    app.opener()
        .reveal_item_in_dir(resolved)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_connector_settings(
    state: State<'_, AppState>,
) -> Result<ConnectorSettingsSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ConnectorSettings)
        .await?;
    expect_variant!(response, CoreResponse::ConnectorSettings)
}

#[tauri::command]
pub async fn set_selected_model(
    state: State<'_, AppState>,
    provider_id: String,
    model_id: String,
) -> Result<ConnectorSettingsSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SetSelectedModel {
            provider_id,
            model_id,
        })
        .await?;
    expect_variant!(response, CoreResponse::ConnectorSettings)
}

#[tauri::command]
pub async fn set_provider_enabled(
    state: State<'_, AppState>,
    provider_id: String,
    enabled: bool,
) -> Result<ConnectorSettingsSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SetProviderEnabled {
            provider_id,
            enabled,
        })
        .await?;
    expect_variant!(response, CoreResponse::ConnectorSettings)
}

#[tauri::command]
pub async fn set_feature_route(
    state: State<'_, AppState>,
    feature: String,
    provider_id: String,
    model_id: String,
    options: Value,
) -> Result<ConnectorSettingsSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SetFeatureRoute {
            feature,
            provider_id,
            model_id,
            options,
        })
        .await?;
    expect_variant!(response, CoreResponse::ConnectorSettings)
}

#[tauri::command]
pub async fn set_chat_model(
    state: State<'_, AppState>,
    chat_id: String,
    provider_id: String,
    model_id: String,
) -> Result<ChatThreadSummary, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SetChatModel {
            chat_id,
            provider_id,
            model_id,
        })
        .await?;
    expect_variant!(response, CoreResponse::ChatSummary)
}

#[tauri::command]
pub async fn set_chat_state(
    state: State<'_, AppState>,
    chat_id: String,
    approval_mode: Option<String>,
    reasoning: Option<String>,
    fast_mode: Option<bool>,
    draft: Option<String>,
) -> Result<ChatThreadSummary, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SetChatState {
            chat_id,
            approval_mode,
            reasoning,
            fast_mode,
            draft,
        })
        .await?;
    expect_variant!(response, CoreResponse::ChatSummary)
}

#[tauri::command]
pub async fn save_adapter_settings(
    state: State<'_, AppState>,
    provider_id: String,
    values: BTreeMap<String, AdapterSettingPatchValue>,
) -> Result<ConnectorSettingsSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SaveAdapterSettings {
            provider_id,
            values,
        })
        .await?;
    expect_variant!(response, CoreResponse::ConnectorSettings)
}

#[tauri::command]
pub async fn authenticate_adapter(
    state: State<'_, AppState>,
    provider_id: String,
) -> Result<ConnectorSettingsSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::Authenticate { provider_id })
        .await?;
    expect_variant!(response, CoreResponse::ConnectorSettings)
}

#[tauri::command]
pub async fn cancel_authenticate_adapter(
    state: State<'_, AppState>,
    provider_id: String,
) -> Result<ConnectorSettingsSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::CancelAuthenticate { provider_id })
        .await?;
    expect_variant!(response, CoreResponse::ConnectorSettings)
}

#[tauri::command]
pub async fn logout_adapter(
    state: State<'_, AppState>,
    provider_id: String,
) -> Result<ConnectorSettingsSnapshot, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::Logout { provider_id })
        .await?;
    expect_variant!(response, CoreResponse::ConnectorSettings)
}

#[tauri::command]
pub async fn run_sidecar_status(state: State<'_, AppState>) -> Result<SidecarStatus, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SidecarStatus)
        .await?;
    expect_variant!(response, CoreResponse::SidecarStatus)
}

/// Host-level supervisor health, answered without talking to the sidecar
/// (unlike `run_sidecar_status`, which needs it ready). Safe in any state —
/// lets the UI seed its `sidecar-status` listener with the current value.
#[tauri::command]
pub fn get_sidecar_health(state: State<'_, AppState>) -> SidecarHealth {
    state.sidecar().health()
}

/// Manually restarts the Core sidecar after the supervisor gave up
/// (`sidecar-status` = `failed`). If the sidecar is alive or already
/// restarting this kills nothing and returns the current health; otherwise it
/// resets the restart budget, wakes the supervisor, and resolves once the new
/// sidecar is ready (or errors after a bounded wait).
#[tauri::command]
pub async fn restart_sidecar(state: State<'_, AppState>) -> Result<SidecarHealth, String> {
    state.sidecar().clone().restart().await
}

#[tauri::command]
pub async fn get_chat_change_sets(
    state: State<'_, AppState>,
    chat_id: String,
) -> Result<Vec<ChangeSetSummary>, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::GetChatChangeSets { chat_id })
        .await?;
    expect_variant!(response, CoreResponse::ChangeSets)
}

#[tauri::command]
pub async fn get_message_change_summary(
    state: State<'_, AppState>,
    message_id: String,
) -> Result<Vec<ChangeSetSummary>, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::GetMessageChangeSummary { message_id })
        .await?;
    expect_variant!(response, CoreResponse::ChangeSets)
}

#[tauri::command]
pub async fn get_change_file_diff(
    state: State<'_, AppState>,
    change_file_id: String,
    offset: Option<u64>,
    limit: Option<u64>,
    full: Option<bool>,
) -> Result<ChangeFileDiff, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::GetChangeFileDiff {
            change_file_id,
            offset,
            limit,
            full,
        })
        .await?;
    expect_variant!(response, CoreResponse::ChangeFileDiff)
}

#[tauri::command]
pub async fn revert_change_set(
    state: State<'_, AppState>,
    change_set_id: String,
) -> Result<RevertOutcome, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::RevertChangeSet { change_set_id })
        .await?;
    expect_variant!(response, CoreResponse::ChangeSetReverted)
}

#[tauri::command]
pub async fn get_change_journal_retention(state: State<'_, AppState>) -> Result<u32, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::GetChangeJournalRetention)
        .await?;
    expect_variant!(response, CoreResponse::ChangeJournalRetention)
}

#[tauri::command]
pub async fn set_change_journal_retention(
    state: State<'_, AppState>,
    value: u32,
) -> Result<u32, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SetChangeJournalRetention { value })
        .await?;
    expect_variant!(response, CoreResponse::ChangeJournalRetention)
}

#[tauri::command]
pub async fn list_change_set_files(
    state: State<'_, AppState>,
    change_set_id: String,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<Vec<ChangeFileSummary>, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ListChangeSetFiles {
            change_set_id,
            offset,
            limit,
        })
        .await?;
    expect_variant!(response, CoreResponse::ChangeFiles)
}
