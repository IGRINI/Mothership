use std::{
    collections::HashMap,
    fmt,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{MothershipError, Result};

use super::{
    CredentialKind, CredentialVault, ProviderId, SecretMaterial, SecretPayload,
    StoreCredentialRequest, VaultHandle,
};

static NEXT_VAULT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub struct InMemoryCredentialVault {
    records: Mutex<HashMap<VaultHandle, SecretMaterial>>,
}

impl fmt::Debug for InMemoryCredentialVault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let record_count = self
            .records
            .lock()
            .map(|records| records.len())
            .unwrap_or_default();
        formatter
            .debug_struct("InMemoryCredentialVault")
            .field("record_count", &record_count)
            .finish()
    }
}

impl InMemoryCredentialVault {
    pub fn contains(&self, handle: &VaultHandle) -> Result<bool> {
        let records = self.lock_records()?;
        Ok(records.contains_key(handle))
    }

    fn lock_records(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<VaultHandle, SecretMaterial>>> {
        self.records.lock().map_err(|_| {
            MothershipError::InvalidRequest("credential vault lock poisoned".to_string())
        })
    }
}

impl CredentialVault for InMemoryCredentialVault {
    fn store(&self, request: StoreCredentialRequest) -> Result<VaultHandle> {
        let sequence = NEXT_VAULT_ID.fetch_add(1, Ordering::Relaxed);
        let handle = VaultHandle::new(format!(
            "memory://{}/credential/{sequence:016x}",
            request.provider_id
        ));

        let mut records = self.lock_records()?;
        records.insert(
            VaultHandle::new(handle.as_str().to_string()),
            request.credential,
        );

        Ok(handle)
    }

    fn load(&self, handle: &VaultHandle) -> Result<SecretMaterial> {
        let records = self.lock_records()?;
        let secret = records
            .get(handle)
            .ok_or_else(|| {
                MothershipError::CredentialVault(format!("credential not found: {handle}"))
            })?
            .clone();

        Ok(secret)
    }

    fn replace(&self, handle: &VaultHandle, request: StoreCredentialRequest) -> Result<()> {
        let mut records = self.lock_records()?;
        records.insert(
            VaultHandle::new(handle.as_str().to_string()),
            request.credential,
        );
        Ok(())
    }

    fn delete(&self, handle: &VaultHandle) -> Result<()> {
        let mut records = self.lock_records()?;
        records.remove(handle);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileCredentialVault {
    root_dir: PathBuf,
}

impl FileCredentialVault {
    const HANDLE_PREFIX: &'static str = "file://credentials/";
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

    fn credential_path(&self, file_name: &str) -> Result<PathBuf> {
        validate_credential_file_name(file_name)?;
        Ok(self.credential_dir().join(file_name))
    }

    fn file_name_for_handle(handle: &VaultHandle) -> Result<&str> {
        handle
            .as_str()
            .strip_prefix(Self::HANDLE_PREFIX)
            .ok_or_else(|| {
                MothershipError::CredentialVault(format!("unsupported credential handle: {handle}"))
            })
    }
}

impl CredentialVault for FileCredentialVault {
    fn store(&self, request: StoreCredentialRequest) -> Result<VaultHandle> {
        fs::create_dir_all(self.credential_dir())?;

        let file_name = generate_credential_file_name(request.provider_id.as_str());
        let path = self.credential_path(&file_name)?;
        let temp_path = path.with_extension(format!("json.tmp-{}", std::process::id()));
        let stored = StoredCredentialFile {
            schema_version: Self::SCHEMA_VERSION,
            provider_id: request.provider_id,
            credential_kind: request.credential.credential_kind,
            payload: request.credential.payload.expose_for_vault().to_string(),
            expires_at: request.credential.expires_at,
            fingerprint_hash: request.credential.fingerprint_hash,
        };

        write_json_atomically(&temp_path, &path, &stored)?;
        Ok(VaultHandle::new(format!(
            "{}{}",
            Self::HANDLE_PREFIX,
            file_name
        )))
    }

    fn load(&self, handle: &VaultHandle) -> Result<SecretMaterial> {
        let file_name = Self::file_name_for_handle(handle)?;
        let path = self.credential_path(file_name)?;
        let bytes = fs::read(&path).map_err(|error| {
            MothershipError::CredentialVault(format!("failed to read credential {handle}: {error}"))
        })?;
        let stored: StoredCredentialFile = serde_json::from_slice(&bytes).map_err(|error| {
            MothershipError::CredentialVault(format!(
                "credential file is corrupted for {handle}: {error}"
            ))
        })?;

        if stored.schema_version != Self::SCHEMA_VERSION {
            return Err(MothershipError::CredentialVault(format!(
                "unsupported credential schema version {} for {handle}",
                stored.schema_version
            )));
        }

        Ok(SecretMaterial {
            credential_kind: stored.credential_kind,
            payload: SecretPayload::new(stored.payload),
            expires_at: stored.expires_at,
            fingerprint_hash: stored.fingerprint_hash,
        })
    }

    fn replace(&self, handle: &VaultHandle, request: StoreCredentialRequest) -> Result<()> {
        let file_name = Self::file_name_for_handle(handle)?;
        let path = self.credential_path(file_name)?;
        let temp_path = path.with_extension(format!("json.tmp-{}", std::process::id()));
        let stored = StoredCredentialFile {
            schema_version: Self::SCHEMA_VERSION,
            provider_id: request.provider_id,
            credential_kind: request.credential.credential_kind,
            payload: request.credential.payload.expose_for_vault().to_string(),
            expires_at: request.credential.expires_at,
            fingerprint_hash: request.credential.fingerprint_hash,
        };

        fs::create_dir_all(self.credential_dir())?;
        replace_json_atomically(&temp_path, &path, &stored)
    }

    fn delete(&self, handle: &VaultHandle) -> Result<()> {
        let file_name = Self::file_name_for_handle(handle)?;
        let path = self.credential_path(file_name)?;

        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCredentialFile {
    schema_version: u32,
    provider_id: ProviderId,
    credential_kind: CredentialKind,
    payload: String,
    expires_at: Option<String>,
    fingerprint_hash: Option<String>,
}

fn write_json_atomically<T: Serialize>(
    temp_path: &Path,
    final_path: &Path,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);

    if let Err(error) = fs::rename(temp_path, final_path) {
        let _ = fs::remove_file(temp_path);
        return Err(error.into());
    }

    Ok(())
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

fn generate_credential_file_name(provider_id: &str) -> String {
    let sequence = NEXT_VAULT_ID.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let provider_id = sanitize_file_name(provider_id);
    format!("{provider_id}_{stamp}_{sequence:016x}.json")
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

fn validate_credential_file_name(file_name: &str) -> Result<()> {
    let valid = !file_name.is_empty()
        && file_name.ends_with(".json")
        && file_name.chars().all(|character| {
            matches!(
                character,
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.'
            )
        });

    if valid {
        Ok(())
    } else {
        Err(MothershipError::CredentialVault(format!(
            "invalid credential file name: {file_name}"
        )))
    }
}
