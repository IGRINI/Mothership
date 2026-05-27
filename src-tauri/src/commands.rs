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
    ChatConversation, ChatThreadSummary, ConnectorSettingsSnapshot, DashboardSnapshot,
    SendChatMessageResult, SidecarStatus,
};
use tauri::State;

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
    limit: Option<i64>,
) -> Result<Vec<ChatThreadSummary>, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::ListChats { limit })
        .await?;
    expect_variant!(response, CoreResponse::ChatList)
}

#[tauri::command]
pub async fn create_chat(state: State<'_, AppState>) -> Result<ChatConversation, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::CreateChat)
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
    content: String,
) -> Result<SendChatMessageResult, String> {
    let response = state
        .sidecar()
        .clone()
        .request(CoreRequest::SendChatMessage { chat_id, content })
        .await?;
    expect_variant!(response, CoreResponse::ChatMessageStarted)
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
pub async fn save_adapter_settings(
    state: State<'_, AppState>,
    provider_id: String,
    values: BTreeMap<String, String>,
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
