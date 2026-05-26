use crate::{MothershipError, Result};

use super::{
    domain::now_timestamp, AdapterStartAuthInput, AuthSession, AuthSessionId, AuthSessionStatus,
    CompleteAuthAdapterInput, CompleteAuthRequest, ConnectionStatus, CredentialRecord,
    CredentialRecordId, CredentialRef, CredentialStatus, CredentialVault,
    ProviderAuthAdapterRegistry, ProviderAuthRepository, ProviderConnection, ProviderConnectionId,
    ProviderDescriptor, RevokeAuthAdapterInput, StartAuthRequest, StoreCredentialRequest,
    VaultHandle,
};
use crate::id::generate_id;

pub struct ProviderAuthService<'a> {
    repository: &'a dyn ProviderAuthRepository,
    vault: &'a dyn CredentialVault,
    registry: &'a dyn ProviderAuthAdapterRegistry,
}

impl<'a> ProviderAuthService<'a> {
    pub fn new(
        repository: &'a dyn ProviderAuthRepository,
        vault: &'a dyn CredentialVault,
        registry: &'a dyn ProviderAuthAdapterRegistry,
    ) -> Self {
        Self {
            repository,
            vault,
            registry,
        }
    }

    pub fn list_providers(&self) -> Vec<ProviderDescriptor> {
        self.registry.providers()
    }

    pub fn start_auth(&self, request: StartAuthRequest) -> Result<AuthSession> {
        validate_id("provider_id", request.provider_id.as_str())?;
        validate_id("auth_method_id", request.auth_method_id.as_str())?;

        let adapter = self
            .registry
            .adapter_for(&request.provider_id, &request.auth_method_id)
            .ok_or_else(|| {
                super::ports::unsupported_adapter(&request.provider_id, &request.auth_method_id)
            })?;

        let auth_session_id = AuthSessionId::new(generate_id("auth_session")?);
        let adapter_result = adapter.start_auth(AdapterStartAuthInput {
            auth_method_id: request.auth_method_id.clone(),
            auth_session_id: auth_session_id.clone(),
        })?;

        let now = now_timestamp();
        let session = AuthSession {
            id: auth_session_id,
            provider_id: request.provider_id,
            auth_method_id: request.auth_method_id,
            mode: adapter_result.mode,
            status: AuthSessionStatus::Pending,
            next_action: adapter_result.next_action,
            expires_at: adapter_result.expires_at,
            provider_metadata: adapter_result.provider_metadata,
            created_at: now.clone(),
            updated_at: now,
        };

        self.repository.save_auth_session(&session)?;
        Ok(session)
    }

    pub fn complete_auth(&self, request: CompleteAuthRequest) -> Result<ProviderConnection> {
        let session = self.repository.get_auth_session(&request.session_id)?;
        if session.status != AuthSessionStatus::Pending {
            return Err(MothershipError::InvalidRequest(format!(
                "auth session is not pending: {}",
                session.id
            )));
        }

        let adapter = self
            .registry
            .adapter_for(&session.provider_id, &session.auth_method_id)
            .ok_or_else(|| {
                super::ports::unsupported_adapter(&session.provider_id, &session.auth_method_id)
            })?;

        let completion = adapter.complete_auth(CompleteAuthAdapterInput {
            session: session.clone(),
            payload: request.payload,
        })?;

        let vault_handle = self.vault.store(StoreCredentialRequest {
            provider_id: session.provider_id.clone(),
            credential: completion.secret.clone(),
        })?;

        let now = now_timestamp();
        let connection_id = ProviderConnectionId::new(generate_id("provider_connection")?);
        let credential_record_id = CredentialRecordId::new(generate_id("credential_record")?);
        let credential_ref = CredentialRef {
            record_id: credential_record_id.clone(),
            vault_handle: VaultHandle::new(vault_handle.as_str().to_string()),
        };

        let connection = ProviderConnection {
            id: connection_id.clone(),
            provider_id: session.provider_id.clone(),
            auth_method_id: session.auth_method_id.clone(),
            status: ConnectionStatus::Active,
            account_label: completion.account_label,
            account_email: completion.account_email,
            scopes: completion.scopes,
            capabilities: completion.capabilities,
            credential_ref,
            expires_at: completion.secret.expires_at.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
        };

        let record = CredentialRecord {
            id: credential_record_id,
            connection_id,
            credential_kind: completion.secret.credential_kind,
            vault_handle,
            expires_at: completion.secret.expires_at,
            fingerprint_hash: completion.secret.fingerprint_hash,
            status: CredentialStatus::Active,
            created_at: now.clone(),
            updated_at: now.clone(),
        };

        let mut completed_session = session;
        completed_session.status = AuthSessionStatus::Completed;
        completed_session.updated_at = now;

        self.repository.save_provider_connection(&connection)?;
        self.repository.save_credential_record(&record)?;
        self.repository.save_auth_session(&completed_session)?;

        Ok(connection)
    }

    pub fn list_connections(&self) -> Result<Vec<ProviderConnection>> {
        self.repository.list_provider_connections()
    }

    pub fn disconnect(&self, id: &ProviderConnectionId) -> Result<()> {
        let connection = self.repository.get_provider_connection(id)?;
        if let Ok(secret) = self.vault.load(&connection.credential_ref.vault_handle) {
            if let Some(adapter) = self
                .registry
                .adapter_for(&connection.provider_id, &connection.auth_method_id)
            {
                let _ = adapter.revoke_auth(RevokeAuthAdapterInput {
                    connection: connection.clone(),
                    secret,
                });
            }
        }

        self.vault.delete(&connection.credential_ref.vault_handle)?;
        self.repository.mark_provider_connection_disconnected(id)
    }
}

fn validate_id(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(MothershipError::InvalidRequest(format!(
            "{name} cannot be empty"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::json;

    use crate::Database;

    use super::*;
    use crate::auth::{
        CompleteAuthRequest, FileCredentialVault, InMemoryCredentialVault, MockProviderAuthAdapter,
        SecretPayload, StaticProviderAuthAdapterRegistry,
    };

    #[test]
    fn mock_flow_creates_connection_without_serializing_raw_secret() {
        let database_path = temp_database_path("mock_flow_creates_connection");
        let database = Database::open(&database_path).expect("open database");
        let vault = InMemoryCredentialVault::default();
        let registry = StaticProviderAuthAdapterRegistry::with_mock_adapter();
        let service = ProviderAuthService::new(&database, &vault, &registry);

        let session = service
            .start_auth(StartAuthRequest {
                provider_id: MockProviderAuthAdapter::PROVIDER_ID.into(),
                auth_method_id: MockProviderAuthAdapter::AUTH_METHOD_ID.into(),
            })
            .expect("start auth");

        let connection = service
            .complete_auth(CompleteAuthRequest {
                session_id: session.id,
                payload: json!({ "accountLabel": "Console Test" }),
            })
            .expect("complete auth");

        let serialized = serde_json::to_string(&connection).expect("serialize connection");
        assert!(serialized.contains("vaultHandle"));
        assert!(!serialized.contains("mock-access-token"));
        assert!(!serialized.contains("mock-refresh-token"));
        assert!(vault
            .contains(&connection.credential_ref.vault_handle)
            .expect("vault lookup"));

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn file_vault_keeps_credentials_independent_when_one_file_is_corrupt() {
        let database_path = temp_database_path("file_vault_corrupt_single_file");
        let auth_path = database_path.with_extension("auth");
        let database = Database::open(&database_path).expect("open database");
        let vault = FileCredentialVault::new(&auth_path);
        let registry = StaticProviderAuthAdapterRegistry::with_mock_adapter();
        let service = ProviderAuthService::new(&database, &vault, &registry);

        let first = connect_mock(&service, "First");
        let second = connect_mock(&service, "Second");

        let first_file = credential_file_path(&auth_path, &first.credential_ref.vault_handle);
        fs::write(first_file, b"{ this is not valid json").expect("corrupt first credential");

        assert!(vault.load(&first.credential_ref.vault_handle).is_err());
        assert!(vault.load(&second.credential_ref.vault_handle).is_ok());
        assert_eq!(service.list_connections().expect("list").len(), 2);

        let _ = fs::remove_file(database_path);
        let _ = fs::remove_dir_all(auth_path);
    }

    #[test]
    fn disconnect_marks_connection_deleted_and_removes_vault_entry() {
        let database_path = temp_database_path("disconnect_removes_vault_entry");
        let database = Database::open(&database_path).expect("open database");
        let vault = InMemoryCredentialVault::default();
        let registry = StaticProviderAuthAdapterRegistry::with_mock_adapter();
        let service = ProviderAuthService::new(&database, &vault, &registry);

        let session = service
            .start_auth(StartAuthRequest {
                provider_id: MockProviderAuthAdapter::PROVIDER_ID.into(),
                auth_method_id: MockProviderAuthAdapter::AUTH_METHOD_ID.into(),
            })
            .expect("start auth");
        let connection = service
            .complete_auth(CompleteAuthRequest {
                session_id: session.id,
                payload: json!({}),
            })
            .expect("complete auth");

        let vault_handle = connection.credential_ref.vault_handle.clone();
        service.disconnect(&connection.id).expect("disconnect");

        assert!(!vault.contains(&vault_handle).expect("vault lookup"));
        assert!(service.list_connections().expect("list").is_empty());

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn secret_payload_debug_is_redacted() {
        let secret = SecretPayload::new("raw-secret-value");
        let rendered = format!("{secret:?}");
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains("raw-secret-value"));
    }

    #[test]
    fn generated_ids_are_not_process_local_counters() {
        let first = generate_id("auth_session").expect("first id");
        let second = generate_id("auth_session").expect("second id");

        assert_ne!(first, second);
        assert!(first.starts_with("auth_session_"));
        assert!(!first.ends_with("0000000000000001"));
    }

    fn connect_mock(service: &ProviderAuthService<'_>, label: &str) -> ProviderConnection {
        let session = service
            .start_auth(StartAuthRequest {
                provider_id: MockProviderAuthAdapter::PROVIDER_ID.into(),
                auth_method_id: MockProviderAuthAdapter::AUTH_METHOD_ID.into(),
            })
            .expect("start auth");

        service
            .complete_auth(CompleteAuthRequest {
                session_id: session.id,
                payload: json!({ "accountLabel": label }),
            })
            .expect("complete auth")
    }

    fn credential_file_path(auth_path: &PathBuf, handle: &VaultHandle) -> PathBuf {
        let file_name = handle
            .as_str()
            .strip_prefix("file://credentials/")
            .expect("file credential handle");
        auth_path.join("credentials").join(file_name)
    }

    fn temp_database_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("mothership_{name}_{stamp}.sqlite"))
    }
}
