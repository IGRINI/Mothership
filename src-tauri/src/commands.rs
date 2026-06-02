//! Tauri command surface: a thin pass-through to the Core sidecar.
//!
//! Every command serializes a [`CoreRequest`], awaits the sidecar's single
//! terminal reply, and unwraps the matching [`CoreResponse`] variant. No
//! database, adapter, or vault logic lives here — that all moved into Core
//! (run in the sidecar process). Streaming chat output is not returned here; it
//! arrives as `chat-run-event`s the sidecar supervisor forwards to the webview.

use std::collections::BTreeMap;

use mothership_core::ipc::{CoreRequest, CoreResponse};
use mothership_core::{
    AdapterSettingPatchValue, ChatConversation, ChatRunCancellationResult, ChatThreadSummary,
    ConnectorSettingsSnapshot, DashboardSnapshot, ProjectSnapshot, ReasoningConfig,
    SendChatMessageResult, SidecarStatus, ToolApprovalAnswer, ToolExecutionAccepted,
    ToolExecutionCancellationResult, ToolExecutionRequest,
};
use tauri::{State, Window};
use tauri_plugin_dialog::DialogExt;

use crate::state::AppState;

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
) -> Result<ChatConversation, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::CreateChat { project_id })
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
pub async fn send_chat_message(
    state: State<'_, AppState>,
    chat_id: Option<String>,
    project_id: Option<String>,
    content: String,
    reasoning: Option<ReasoningConfig>,
) -> Result<SendChatMessageResult, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SendChatMessage {
            chat_id,
            project_id,
            content,
            reasoning,
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
