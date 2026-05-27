//! Provider-agnostic LLM connector scaffolding.
//!
//! The core knows nothing about any specific provider. Providers are subprocess
//! adapters (see `mothership-adapter-host` + the `adapters/` crates); this module
//! only defines the generic model/catalog/chat types, the connector trait, and a
//! registry. The shipped registry is empty — built-in providers are gone; chat
//! and model listing flow through adapters.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::auth::{
    ConnectionStatus, CredentialVault, ProviderConnection, SecretMaterial, StoreCredentialRequest,
};
use crate::{MothershipError, Result};

const MODEL_CATALOG_CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmModel {
    pub provider_id: String,
    pub provider_label: String,
    pub id: String,
    pub label: String,
    pub family: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub recommended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SelectedLlmModel {
    pub provider_id: String,
    pub model_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorSettingsSchema {
    pub model_management: ConnectorModelManagementSchema,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorModelManagementSchema {
    pub kind: ConnectorModelManagementKind,
    pub title: String,
    pub description: String,
    pub add_model_label: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorModelManagementKind {
    FixedCatalog,
    RemoteCatalog,
    EditableList,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmModelCatalogCache {
    pub provider_id: String,
    pub models: Vec<LlmModel>,
    pub etag: Option<String>,
    pub fetched_at: String,
    pub expires_at: String,
}

impl LlmModelCatalogCache {
    pub fn is_fresh(&self) -> bool {
        parse_timestamp(&self.expires_at)
            .map(|expires_at| expires_at > unix_timestamp_secs())
            .unwrap_or(false)
    }
}

pub trait LlmModelCatalogRepository: Send + Sync {
    fn load_llm_model_catalog_cache(
        &self,
        provider_id: &str,
    ) -> Result<Option<LlmModelCatalogCache>>;

    fn save_llm_model_catalog_cache(&self, cache: &LlmModelCatalogCache) -> Result<()>;
}

pub struct RemoteModelCatalogInput<'a> {
    pub connection: &'a ProviderConnection,
    pub secret: &'a SecretMaterial,
}

pub struct RemoteModelCatalog {
    pub models: Vec<LlmModel>,
    pub etag: Option<String>,
    pub refreshed_secret: Option<SecretMaterial>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmChatCompletionRequest {
    pub provider_id: String,
    pub model_id: String,
    pub system_prompt: String,
    pub messages: Vec<LlmChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmChatMessage {
    pub role: LlmChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LlmChatRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LlmTransportKind {
    WebSocket,
    HttpSse,
    HttpJson,
    Subprocess,
}

impl LlmTransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::HttpSse => "http_sse",
            Self::HttpJson => "http_json",
            Self::Subprocess => "subprocess",
        }
    }
}

pub trait LlmChatCompletionEventSink {
    fn transport_selected(&mut self, transport: LlmTransportKind);

    fn delta(&mut self, delta: &str);
}

pub trait LlmChatCompletionGateway: Send + Sync {
    fn complete_chat(
        &self,
        request: LlmChatCompletionRequest,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<String>;
}

pub trait LlmConnectorAdapter: Send + Sync {
    fn provider_id(&self) -> &'static str;

    fn provider_label(&self) -> &'static str;

    fn bundled_models(&self) -> Vec<LlmModel>;

    fn chat_system_prompt(&self, _model_id: &str) -> Option<String> {
        None
    }

    /// Builds the chat-completion gateway for this provider, borrowing the
    /// credential vault and the active connection. Providers that do not
    /// implement chat leave the default, which reports an unsupported provider.
    fn chat_gateway<'a>(
        &self,
        _vault: &'a dyn CredentialVault,
        _connection: &'a ProviderConnection,
    ) -> Result<Box<dyn LlmChatCompletionGateway + 'a>> {
        Err(MothershipError::InvalidRequest(format!(
            "chat completion is not implemented for provider: {}",
            self.provider_id()
        )))
    }

    fn remote_model_catalog(
        &self,
        _input: RemoteModelCatalogInput<'_>,
    ) -> Result<Option<RemoteModelCatalog>> {
        Ok(None)
    }

    fn settings_schema(&self) -> ConnectorSettingsSchema;
}

pub struct StaticLlmConnectorRegistry {
    adapters: Vec<Box<dyn LlmConnectorAdapter>>,
}

impl StaticLlmConnectorRegistry {
    pub fn new(adapters: Vec<Box<dyn LlmConnectorAdapter>>) -> Self {
        Self { adapters }
    }

    pub fn list_models(&self) -> Vec<LlmModel> {
        self.list_bundled_models()
    }

    pub fn list_bundled_models(&self) -> Vec<LlmModel> {
        self.adapters
            .iter()
            .flat_map(|adapter| adapter.bundled_models())
            .collect()
    }

    pub fn find_model(&self, provider_id: &str, model_id: &str) -> Option<LlmModel> {
        self.adapters
            .iter()
            .find(|adapter| adapter.provider_id() == provider_id)
            .and_then(|adapter| {
                adapter
                    .bundled_models()
                    .into_iter()
                    .find(|model| model.id == model_id)
            })
    }

    pub fn settings_schema(&self, provider_id: &str) -> Option<ConnectorSettingsSchema> {
        self.adapters
            .iter()
            .find(|adapter| adapter.provider_id() == provider_id)
            .map(|adapter| adapter.settings_schema())
    }

    pub fn provider_label(&self, provider_id: &str) -> Option<&'static str> {
        self.adapters
            .iter()
            .find(|adapter| adapter.provider_id() == provider_id)
            .map(|adapter| adapter.provider_label())
    }

    pub fn chat_system_prompt(&self, provider_id: &str, model_id: &str) -> Option<String> {
        self.adapters
            .iter()
            .find(|adapter| adapter.provider_id() == provider_id)
            .and_then(|adapter| adapter.chat_system_prompt(model_id))
    }

    pub fn chat_gateway<'a>(
        &self,
        provider_id: &str,
        vault: &'a dyn CredentialVault,
        connection: &'a ProviderConnection,
    ) -> Result<Box<dyn LlmChatCompletionGateway + 'a>> {
        self.adapters
            .iter()
            .find(|adapter| adapter.provider_id() == provider_id)
            .ok_or_else(|| {
                MothershipError::InvalidRequest(format!("unknown provider: {provider_id}"))
            })?
            .chat_gateway(vault, connection)
    }
}

pub struct LlmModelCatalogService<'a> {
    repository: &'a dyn LlmModelCatalogRepository,
    vault: &'a dyn CredentialVault,
    registry: &'a StaticLlmConnectorRegistry,
}

impl<'a> LlmModelCatalogService<'a> {
    pub fn new(
        repository: &'a dyn LlmModelCatalogRepository,
        vault: &'a dyn CredentialVault,
        registry: &'a StaticLlmConnectorRegistry,
    ) -> Self {
        Self {
            repository,
            vault,
            registry,
        }
    }

    pub fn list_models(&self, connections: &[ProviderConnection]) -> Result<Vec<LlmModel>> {
        let mut models = Vec::new();

        for adapter in &self.registry.adapters {
            models.extend(self.models_for_adapter(adapter.as_ref(), connections)?);
        }

        Ok(models)
    }

    fn models_for_adapter(
        &self,
        adapter: &dyn LlmConnectorAdapter,
        connections: &[ProviderConnection],
    ) -> Result<Vec<LlmModel>> {
        let provider_id = adapter.provider_id();
        let model_management = adapter.settings_schema().model_management.kind;

        if model_management != ConnectorModelManagementKind::RemoteCatalog {
            return Ok(adapter.bundled_models());
        }

        let Some(connection) = active_connection(connections, provider_id) else {
            return Ok(Vec::new());
        };

        let cached = self.repository.load_llm_model_catalog_cache(provider_id)?;
        if let Some(cache) = cached
            .as_ref()
            .filter(|cache| cache.is_fresh() && !cache.models.is_empty())
        {
            return Ok(cache.models.clone());
        }

        let catalog = match self.load_remote_catalog(adapter, connection) {
            Ok(Some(catalog)) => catalog,
            Ok(None) => {
                return Err(MothershipError::InvalidRequest(format!(
                    "connector does not implement remote model catalog: {provider_id}"
                )))
            }
            Err(error) => {
                if let Some(cache) = cached.as_ref().filter(|cache| !cache.models.is_empty()) {
                    return Ok(cache.models.clone());
                }
                return Err(error);
            }
        };

        if catalog.models.is_empty() {
            return Err(MothershipError::InvalidRequest(format!(
                "remote model catalog returned no models: {provider_id}"
            )));
        }

        let now = unix_timestamp_secs();
        self.repository
            .save_llm_model_catalog_cache(&LlmModelCatalogCache {
                provider_id: provider_id.to_string(),
                models: catalog.models.clone(),
                etag: catalog.etag,
                fetched_at: now.to_string(),
                expires_at: now
                    .saturating_add(MODEL_CATALOG_CACHE_TTL.as_secs())
                    .to_string(),
            })?;

        if let Some(secret) = catalog.refreshed_secret {
            self.vault.replace(
                &connection.credential_ref.vault_handle,
                StoreCredentialRequest {
                    provider_id: connection.provider_id.clone(),
                    credential: secret,
                },
            )?;
        }

        Ok(catalog.models)
    }

    fn load_remote_catalog(
        &self,
        adapter: &dyn LlmConnectorAdapter,
        connection: &ProviderConnection,
    ) -> Result<Option<RemoteModelCatalog>> {
        let secret = self.vault.load(&connection.credential_ref.vault_handle)?;
        adapter.remote_model_catalog(RemoteModelCatalogInput {
            connection,
            secret: &secret,
        })
    }
}

/// The connectors that ship built into the app. Empty: every provider is now a
/// runtime-loaded subprocess adapter, so the core holds no provider code.
pub fn default_llm_registry() -> StaticLlmConnectorRegistry {
    StaticLlmConnectorRegistry::new(Vec::new())
}

/// Builds the chat-completion gateway for `provider_id` from the built-in
/// registry. Always errors now (no built-in providers) — chat routes through
/// subprocess adapters instead.
pub fn chat_completion_gateway<'a>(
    provider_id: &str,
    vault: &'a dyn CredentialVault,
    connection: &'a ProviderConnection,
) -> Result<Box<dyn LlmChatCompletionGateway + 'a>> {
    default_llm_registry().chat_gateway(provider_id, vault, connection)
}

/// The recommended built-in model, if any. None now (no built-in providers).
pub fn default_llm_model() -> Option<LlmModel> {
    default_llm_registry()
        .list_models()
        .into_iter()
        .find(|model| model.recommended)
}

pub fn find_llm_model(provider_id: &str, model_id: &str) -> Option<LlmModel> {
    default_llm_registry().find_model(provider_id, model_id)
}

pub fn connector_settings_schema(provider_id: &str) -> Result<ConnectorSettingsSchema> {
    default_llm_registry()
        .settings_schema(provider_id)
        .ok_or_else(|| MothershipError::InvalidRequest(format!("unknown provider: {provider_id}")))
}

pub fn chat_system_prompt(provider_id: &str, model_id: &str) -> Result<String> {
    default_llm_registry()
        .chat_system_prompt(provider_id, model_id)
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or_else(|| {
            MothershipError::InvalidRequest(format!(
                "no chat system prompt registered for {provider_id}/{model_id}"
            ))
        })
}

fn active_connection<'a>(
    connections: &'a [ProviderConnection],
    provider_id: &str,
) -> Option<&'a ProviderConnection> {
    connections.iter().find(|connection| {
        connection.provider_id.as_str() == provider_id
            && connection.status == ConnectionStatus::Active
    })
}

fn parse_timestamp(value: &str) -> Option<u64> {
    value.parse::<u64>().ok()
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::{
        auth::{
            AuthMethodId, CredentialKind, CredentialRecordId, CredentialRef,
            InMemoryCredentialVault, ProviderConnectionId, ProviderId, SecretPayload, VaultHandle,
        },
        Database,
    };

    #[test]
    fn default_registry_is_empty() {
        assert!(default_llm_registry().list_models().is_empty());
        assert!(default_llm_model().is_none());
    }

    #[test]
    fn remote_catalog_error_is_returned_not_hidden_by_bundled_models() {
        let database_path = temp_database_path("remote_catalog_error_is_returned");
        let database = Database::open(database_path.clone()).expect("open database");
        let vault = InMemoryCredentialVault::default();
        let vault_handle = vault
            .store(StoreCredentialRequest {
                provider_id: ProviderId::from("mock"),
                credential: SecretMaterial {
                    credential_kind: CredentialKind::OAuthTokenSet,
                    payload: SecretPayload::new("{}"),
                    expires_at: None,
                    fingerprint_hash: None,
                },
            })
            .expect("store secret");
        let registry = StaticLlmConnectorRegistry::new(vec![Box::new(FailingRemoteConnector)]);
        let service = LlmModelCatalogService::new(&database, &vault, &registry);
        let connection = ProviderConnection {
            id: ProviderConnectionId::from("provider_connection_test"),
            provider_id: ProviderId::from("mock"),
            auth_method_id: AuthMethodId::from("mock_oauth"),
            status: ConnectionStatus::Active,
            account_label: None,
            account_email: None,
            scopes: Vec::new(),
            capabilities: Vec::new(),
            credential_ref: CredentialRef {
                record_id: CredentialRecordId::from("credential_record_test"),
                vault_handle: VaultHandle::new(vault_handle.as_str().to_string()),
            },
            expires_at: None,
            created_at: "0".to_string(),
            updated_at: "0".to_string(),
        };

        let error = service
            .list_models(&[connection])
            .expect_err("remote error should be returned");

        assert!(error.to_string().contains("remote failed"));

        let _ = fs::remove_file(database_path);
    }

    struct FailingRemoteConnector;

    impl LlmConnectorAdapter for FailingRemoteConnector {
        fn provider_id(&self) -> &'static str {
            "mock"
        }

        fn provider_label(&self) -> &'static str {
            "Mock"
        }

        fn bundled_models(&self) -> Vec<LlmModel> {
            vec![test_model("bundled-model")]
        }

        fn remote_model_catalog(
            &self,
            _input: RemoteModelCatalogInput<'_>,
        ) -> Result<Option<RemoteModelCatalog>> {
            Err(MothershipError::InvalidRequest("remote failed".to_string()))
        }

        fn settings_schema(&self) -> ConnectorSettingsSchema {
            ConnectorSettingsSchema {
                model_management: ConnectorModelManagementSchema {
                    kind: ConnectorModelManagementKind::RemoteCatalog,
                    title: "Mock models".to_string(),
                    description: "Mock remote catalog".to_string(),
                    add_model_label: None,
                },
            }
        }
    }

    fn test_model(id: &str) -> LlmModel {
        LlmModel {
            provider_id: "mock".to_string(),
            provider_label: "Mock".to_string(),
            id: id.to_string(),
            label: id.to_string(),
            family: "Mock".to_string(),
            description: "Test model".to_string(),
            capabilities: vec!["text".to_string()],
            recommended: true,
        }
    }

    fn temp_database_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();

        std::env::temp_dir().join(format!("mothership_llm_{name}_{unique}.sqlite3"))
    }
}
