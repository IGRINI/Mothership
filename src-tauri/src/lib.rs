mod commands;
mod job;
mod platform;
mod sidecar;
mod state;

use sidecar::Sidecar;
use state::AppState;
use tauri::Manager;

#[cfg(windows)]
const WINDOWS_WEBVIEW_BACKGROUND: &str = "FF07101A";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    configure_process_platform();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            // The host opens nothing: the sidecar owns the database (open,
            // migrate, recover interrupted runs) once we send it the path.
            let database_path = app.path().app_data_dir()?.join("mothership.sqlite3");
            let sidecar = Sidecar::start(app.handle(), database_path);
            app.manage(AppState::new(sidecar));
            platform::configure_webview_platform(app);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::append_activity_event,
            commands::approve_tool_execution,
            commands::authenticate_adapter,
            commands::branch_chat_from_message,
            commands::cancel_authenticate_adapter,
            commands::cancel_chat_run,
            commands::cancel_tool_execution,
            commands::continue_chat_message,
            commands::create_chat,
            commands::edit_chat_user_message,
            commands::get_change_file_diff,
            commands::get_chat,
            commands::get_chat_change_sets,
            commands::get_connector_settings,
            commands::get_dashboard_snapshot,
            commands::get_message_change_summary,
            commands::get_personalization,
            commands::get_tool_approval_mode,
            commands::get_tool_artifact_range,
            commands::get_tool_policy,
            commands::list_change_set_files,
            commands::list_chats,
            commands::list_projects,
            commands::logout_adapter,
            commands::open_project,
            commands::open_tool_path,
            commands::pick_project_directory,
            commands::retry_chat_message,
            commands::revert_change_set,
            commands::reveal_tool_path,
            commands::run_tool_command,
            commands::run_sidecar_status,
            commands::save_adapter_settings,
            commands::send_chat_message,
            commands::set_active_project,
            commands::set_chat_model,
            commands::set_chat_state,
            commands::set_personalization,
            commands::set_tool_approval_mode,
            commands::set_tool_policy,
            commands::set_selected_model,
            commands::set_provider_enabled,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Ready) {
                platform::configure_window_platform(app);
            }
        });
}

fn configure_process_platform() {
    #[cfg(windows)]
    std::env::set_var(
        "WEBVIEW2_DEFAULT_BACKGROUND_COLOR",
        WINDOWS_WEBVIEW_BACKGROUND,
    );
}
