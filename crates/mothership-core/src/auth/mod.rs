mod domain;
mod mock;
mod oauth;
mod ports;
mod repository;
mod service;
mod vault;

pub use domain::{
    AuthMethod, AuthMethodId, AuthMethodKind, AuthMode, AuthNextAction, AuthSession, AuthSessionId,
    AuthSessionStatus, CompleteAuthRequest, ConnectionStatus, CredentialKind, CredentialRecord,
    CredentialRecordId, CredentialRef, CredentialStatus, ProviderConnection, ProviderConnectionId,
    ProviderDescriptor, ProviderId, SecretMaterial, SecretPayload, StartAuthRequest, VaultHandle,
};
pub use mock::MockProviderAuthAdapter;
pub use oauth::{generate_pkce_pair, generate_state, pkce_challenge_s256, PkcePair};
pub use ports::{
    AdapterAuthCompletion, AdapterRevokeAuthResult, AdapterStartAuthInput, AdapterStartAuthResult,
    CompleteAuthAdapterInput, CredentialVault, ProviderAuthAdapter, ProviderAuthAdapterRegistry,
    ProviderAuthRepository, RevokeAuthAdapterInput, StaticProviderAuthAdapterRegistry,
    StoreCredentialRequest,
};
pub use service::ProviderAuthService;
pub use vault::{FileCredentialVault, InMemoryCredentialVault};
