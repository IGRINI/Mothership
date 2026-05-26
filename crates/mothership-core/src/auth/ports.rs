use crate::{MothershipError, Result};

use super::{
    AuthMethod, AuthMethodId, AuthMode, AuthNextAction, AuthSession, AuthSessionId,
    CredentialRecord, ProviderConnection, ProviderConnectionId, ProviderDescriptor, ProviderId,
    SecretMaterial, VaultHandle,
};

pub trait ProviderAuthAdapter: Send + Sync {
    fn provider_id(&self) -> ProviderId;

    fn provider_label(&self) -> String;

    fn auth_methods(&self) -> Vec<AuthMethod>;

    fn start_auth(&self, input: AdapterStartAuthInput) -> Result<AdapterStartAuthResult>;

    fn complete_auth(&self, input: CompleteAuthAdapterInput) -> Result<AdapterAuthCompletion>;

    fn revoke_auth(&self, _input: RevokeAuthAdapterInput) -> Result<AdapterRevokeAuthResult> {
        Ok(AdapterRevokeAuthResult::unsupported())
    }
}

#[derive(Debug, Clone)]
pub struct AdapterStartAuthInput {
    pub auth_method_id: AuthMethodId,
    pub auth_session_id: AuthSessionId,
}

#[derive(Debug, Clone)]
pub struct AdapterStartAuthResult {
    pub mode: AuthMode,
    pub next_action: AuthNextAction,
    pub expires_at: Option<String>,
    pub provider_metadata: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CompleteAuthAdapterInput {
    pub session: AuthSession,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct AdapterAuthCompletion {
    pub account_label: Option<String>,
    pub account_email: Option<String>,
    pub scopes: Vec<String>,
    pub capabilities: Vec<String>,
    pub secret: SecretMaterial,
}

#[derive(Debug, Clone)]
pub struct RevokeAuthAdapterInput {
    pub connection: ProviderConnection,
    pub secret: SecretMaterial,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct AdapterRevokeAuthResult {
    pub supported: bool,
    pub attempted: bool,
}

impl AdapterRevokeAuthResult {
    pub fn unsupported() -> Self {
        Self {
            supported: false,
            attempted: false,
        }
    }

    pub fn attempted() -> Self {
        Self {
            supported: true,
            attempted: true,
        }
    }

    pub fn nothing_to_revoke() -> Self {
        Self {
            supported: true,
            attempted: false,
        }
    }
}

pub trait ProviderAuthAdapterRegistry: Send + Sync {
    fn providers(&self) -> Vec<ProviderDescriptor>;

    fn adapter_for(
        &self,
        provider_id: &ProviderId,
        auth_method_id: &AuthMethodId,
    ) -> Option<&dyn ProviderAuthAdapter>;
}

pub struct StaticProviderAuthAdapterRegistry {
    adapters: Vec<Box<dyn ProviderAuthAdapter>>,
}

impl StaticProviderAuthAdapterRegistry {
    pub fn new(adapters: Vec<Box<dyn ProviderAuthAdapter>>) -> Self {
        Self { adapters }
    }
}

impl ProviderAuthAdapterRegistry for StaticProviderAuthAdapterRegistry {
    fn providers(&self) -> Vec<ProviderDescriptor> {
        self.adapters
            .iter()
            .map(|adapter| ProviderDescriptor {
                id: adapter.provider_id(),
                label: adapter.provider_label(),
                methods: adapter.auth_methods(),
            })
            .collect()
    }

    fn adapter_for(
        &self,
        provider_id: &ProviderId,
        auth_method_id: &AuthMethodId,
    ) -> Option<&dyn ProviderAuthAdapter> {
        self.adapters
            .iter()
            .map(|adapter| adapter.as_ref())
            .find(|adapter| {
                adapter.provider_id() == *provider_id
                    && adapter
                        .auth_methods()
                        .iter()
                        .any(|method| method.id == *auth_method_id)
            })
    }
}

pub trait ProviderAuthRepository: Send + Sync {
    fn save_auth_session(&self, session: &AuthSession) -> Result<()>;

    fn get_auth_session(&self, id: &AuthSessionId) -> Result<AuthSession>;

    fn save_provider_connection(&self, connection: &ProviderConnection) -> Result<()>;

    fn save_credential_record(&self, record: &CredentialRecord) -> Result<()>;

    fn list_provider_connections(&self) -> Result<Vec<ProviderConnection>>;

    fn get_provider_connection(&self, id: &ProviderConnectionId) -> Result<ProviderConnection>;

    fn mark_provider_connection_disconnected(&self, id: &ProviderConnectionId) -> Result<()>;
}

pub trait CredentialVault: Send + Sync {
    fn store(&self, request: StoreCredentialRequest) -> Result<VaultHandle>;

    fn load(&self, handle: &VaultHandle) -> Result<SecretMaterial>;

    fn replace(&self, handle: &VaultHandle, request: StoreCredentialRequest) -> Result<()>;

    fn delete(&self, handle: &VaultHandle) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct StoreCredentialRequest {
    pub provider_id: ProviderId,
    pub credential: SecretMaterial,
}

pub(crate) fn unsupported_adapter(
    provider_id: &ProviderId,
    method_id: &AuthMethodId,
) -> MothershipError {
    MothershipError::InvalidRequest(format!(
        "unsupported auth adapter: provider={provider_id}, method={method_id}"
    ))
}
