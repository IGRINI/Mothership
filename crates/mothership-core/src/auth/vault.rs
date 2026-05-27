use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{MothershipError, Result};

/// The app's shared credential store: a directory holding one file per provider
/// adapter with that adapter's settings map (including its secrets — API keys,
/// OAuth tokens). The host loads these and pushes them to the adapter via
/// `set_settings` on spawn, and persists anything the adapter hands back (a
/// freshly minted/refreshed token) here too. Nothing secret is stored next to
/// the adapter on disk.
///
/// Plaintext JSON for now (YOLO/full-trust). The store can later move behind an
/// OS keychain without changing callers.
#[derive(Debug, Clone)]
pub struct FileCredentialVault {
    root_dir: PathBuf,
}

impl FileCredentialVault {
    const SCHEMA_VERSION: u32 = 1;

    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
        }
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    fn credential_dir(&self) -> PathBuf {
        self.root_dir.join("credentials")
    }

    /// Path of the file holding one adapter's settings, keyed by `provider_id`.
    fn adapter_settings_path(&self, provider_id: &str) -> PathBuf {
        let file_name = format!("adapter_{}.json", sanitize_file_name(provider_id));
        self.credential_dir().join(file_name)
    }

    /// Loads the stored settings map for `provider_id` (empty if nothing is
    /// stored yet).
    pub fn load_adapter_settings(&self, provider_id: &str) -> Result<BTreeMap<String, String>> {
        let path = self.adapter_settings_path(provider_id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(error) => {
                return Err(MothershipError::CredentialVault(format!(
                    "failed to read adapter settings for {provider_id}: {error}"
                )))
            }
        };

        let stored: AdapterSettingsFile = serde_json::from_slice(&bytes).map_err(|error| {
            MothershipError::CredentialVault(format!(
                "adapter settings file is corrupted for {provider_id}: {error}"
            ))
        })?;

        if stored.schema_version != Self::SCHEMA_VERSION {
            return Err(MothershipError::CredentialVault(format!(
                "unsupported adapter settings schema version {} for {provider_id}",
                stored.schema_version
            )));
        }

        Ok(stored.values)
    }

    /// Replaces the stored settings map for `provider_id` atomically.
    pub fn save_adapter_settings(
        &self,
        provider_id: &str,
        values: &BTreeMap<String, String>,
    ) -> Result<()> {
        fs::create_dir_all(self.credential_dir())?;
        let path = self.adapter_settings_path(provider_id);
        let temp_path = path.with_extension(format!("json.tmp-{}", std::process::id()));
        let stored = AdapterSettingsFile {
            schema_version: Self::SCHEMA_VERSION,
            provider_id: provider_id.to_string(),
            values: values.clone(),
        };
        replace_json_atomically(&temp_path, &path, &stored)
    }

    /// Merges `updates` over the stored settings for `provider_id`, keeping any
    /// keys the caller didn't supply. This is the safe path for both the UI
    /// "Save" (which only knows the declared schema fields) and the adapter's
    /// `StoreSecret` side channel (which carries only the changed keys, e.g. a
    /// freshly minted OAuth token) — neither clobbers the other's values.
    pub fn merge_adapter_settings(
        &self,
        provider_id: &str,
        updates: BTreeMap<String, String>,
    ) -> Result<()> {
        let mut values = self.load_adapter_settings(provider_id)?;
        values.extend(updates);
        self.save_adapter_settings(provider_id, &values)
    }

    /// Forgets everything stored for `provider_id` — the "log out" path: the
    /// app drops the adapter's credential from the shared store. A missing file
    /// is already-logged-out, so it's success.
    pub fn delete_adapter_settings(&self, provider_id: &str) -> Result<()> {
        match fs::remove_file(self.adapter_settings_path(provider_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(MothershipError::CredentialVault(format!(
                "failed to delete adapter settings for {provider_id}: {error}"
            ))),
        }
    }
}

/// On-disk shape of one adapter's stored settings map (its full `set_settings`
/// map, including any secret fields).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdapterSettingsFile {
    schema_version: u32,
    provider_id: String,
    values: BTreeMap<String, String>,
}

fn replace_json_atomically<T: Serialize>(
    temp_path: &Path,
    final_path: &Path,
    value: &T,
) -> Result<()> {
    let backup_path = final_path.with_extension(format!("json.bak-{}", std::process::id()));
    write_json_file(temp_path, value)?;

    if final_path.exists() {
        if let Err(error) = fs::rename(final_path, &backup_path) {
            let _ = fs::remove_file(temp_path);
            return Err(error.into());
        }
    }

    if let Err(error) = fs::rename(temp_path, final_path) {
        let _ = fs::remove_file(temp_path);
        if backup_path.exists() {
            let _ = fs::rename(&backup_path, final_path);
        }
        return Err(error.into());
    }

    if backup_path.exists() {
        let _ = fs::remove_file(backup_path);
    }

    Ok(())
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn sanitize_file_name(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => character,
            _ => '_',
        })
        .collect();

    if sanitized.is_empty() {
        "provider".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_vault(name: &str) -> FileCredentialVault {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        FileCredentialVault::new(std::env::temp_dir().join(format!("ms_vault_{name}_{stamp}")))
    }

    #[test]
    fn adapter_settings_round_trip() {
        let vault = temp_vault("round_trip");
        assert!(vault
            .load_adapter_settings("openrouter")
            .expect("load missing")
            .is_empty());

        let values = BTreeMap::from([
            ("api_key".to_string(), "sk-secret".to_string()),
            ("base_url".to_string(), "https://example".to_string()),
        ]);
        vault
            .save_adapter_settings("openrouter", &values)
            .expect("save");

        assert_eq!(vault.load_adapter_settings("openrouter").expect("load"), values);

        let _ = fs::remove_dir_all(vault.root_dir());
    }

    #[test]
    fn merge_keeps_keys_not_in_update() {
        let vault = temp_vault("merge");
        vault
            .save_adapter_settings(
                "codex",
                &BTreeMap::from([("credential".to_string(), "old-token".to_string())]),
            )
            .expect("seed");

        // The UI saves an empty form (Codex declares no fields); the stored
        // OAuth credential must survive.
        vault
            .merge_adapter_settings("codex", BTreeMap::new())
            .expect("merge empty");
        assert_eq!(
            vault.load_adapter_settings("codex").expect("load").get("credential"),
            Some(&"old-token".to_string())
        );

        // The adapter pushes a refreshed token; it replaces only that key.
        vault
            .merge_adapter_settings(
                "codex",
                BTreeMap::from([("credential".to_string(), "new-token".to_string())]),
            )
            .expect("merge update");
        assert_eq!(
            vault.load_adapter_settings("codex").expect("load").get("credential"),
            Some(&"new-token".to_string())
        );

        let _ = fs::remove_dir_all(vault.root_dir());
    }
}
