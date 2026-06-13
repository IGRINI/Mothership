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

    let builder = tauri::Builder::default();

    // Single instance must be the FIRST registered plugin so a second launch
    // is caught before anything else initializes. Two instances would mean two
    // sidecars on one SQLite database — the second one's startup recovery
    // marks the first one's live runs as failed.
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        focus_main_window(app);
    }));

    builder
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
            commands::delete_chat,
            commands::delete_project,
            commands::edit_chat_user_message,
            commands::get_change_file_diff,
            commands::get_change_journal_retention,
            commands::get_chat,
            commands::get_chat_change_sets,
            commands::get_connector_settings,
            commands::get_dashboard_snapshot,
            commands::get_message_change_summary,
            commands::get_personalization,
            commands::get_prompt_preview,
            commands::get_sidecar_health,
            commands::get_tool_artifact_range,
            commands::get_tool_policy,
            commands::list_active_runs,
            commands::list_change_set_files,
            commands::list_chats,
            commands::list_projects,
            commands::logout_adapter,
            commands::open_project,
            commands::open_artifact_path,
            commands::open_tool_path,
            commands::pick_project_directory,
            commands::read_image_data_url,
            commands::rename_chat,
            commands::rename_project,
            commands::restart_sidecar,
            commands::retry_chat_message,
            commands::revert_change_set,
            commands::resolve_tool_path,
            commands::reveal_artifact_path,
            commands::reveal_tool_path,
            commands::run_tool_command,
            commands::run_sidecar_status,
            commands::save_adapter_settings,
            commands::send_chat_message,
            commands::set_active_project,
            commands::set_change_journal_retention,
            commands::set_chat_model,
            commands::set_chat_state,
            commands::set_personalization,
            commands::set_project_appearance,
            commands::set_feature_route,
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

/// Brings the existing main window to the front when a second app instance
/// launches (Discord-style single instance).
#[cfg(desktop)]
fn focus_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let Some(window) = app.get_webview_window("main") else {
        eprintln!("main window was not found while focusing the running instance");
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}
