use std::path::Path;

use mothership_core::SidecarStatus;
use tauri::AppHandle;
use tauri_plugin_shell::ShellExt;

const SIDECAR_NAME: &str = "mothership-sidecar";

pub async fn status(app: &AppHandle, database_path: &Path) -> Result<SidecarStatus, String> {
    let database_path = database_path
        .to_str()
        .ok_or_else(|| "database path is not valid UTF-8".to_string())?;

    let output = app
        .shell()
        .sidecar(SIDECAR_NAME)
        .map_err(|error| error.to_string())?
        .args(["status", "--database", database_path])
        .output()
        .await
        .map_err(|error| error.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            "sidecar exited with a non-zero status".to_string()
        } else {
            stderr
        };
        return Err(message);
    }

    serde_json::from_slice::<SidecarStatus>(&output.stdout).map_err(|error| error.to_string())
}
