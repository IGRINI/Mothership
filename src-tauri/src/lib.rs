mod commands;
mod platform;
mod sidecar;
mod state;

use mothership_core::Database;
use state::AppState;
use tauri::Manager;

#[cfg(windows)]
const WINDOWS_WEBVIEW_BACKGROUND: &str = "FF07101A";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    configure_process_platform();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let database_path = app.path().app_data_dir()?.join("mothership.sqlite3");
            let database = Database::open(database_path)?;
            database.recover_interrupted_chat_runs()?;
            app.manage(AppState::new(database));
            platform::configure_webview_platform(app);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::append_activity_event,
            commands::complete_provider_auth,
            commands::create_chat,
            commands::disconnect_provider_connection,
            commands::get_chat,
            commands::get_connector_settings,
            commands::get_dashboard_snapshot,
            commands::list_chats,
            commands::run_sidecar_status,
            commands::save_adapter_settings,
            commands::send_chat_message,
            commands::set_selected_model,
            commands::start_provider_auth,
            commands::start_provider_oauth_login,
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
