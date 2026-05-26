use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.0)
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

string_id!(ProviderId);
string_id!(AuthMethodId);
string_id!(AuthSessionId);
string_id!(ProviderConnectionId);
string_id!(CredentialRecordId);
string_id!(VaultHandle);

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethodKind {
    #[serde(rename = "oauth_browser")]
    OAuthBrowser,
    #[serde(rename = "oauth_device")]
    OAuthDevice,
    #[serde(rename = "oauth_token_paste")]
    OAuthTokenPaste,
    ApiKey,
    Mock,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Browser,
    Device,
    Manual,
    Mock,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuthSessionStatus {
    Pending,
    Completed,
    Failed,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Active,
    Disconnected,
    NeedsRefresh,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    #[serde(rename = "oauth_token_set")]
    OAuthTokenSet,
    ApiKey,
    MockTokenSet,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Active,
    Deleted,
    Expired,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethod {
    pub id: AuthMethodId,
    pub provider_id: ProviderId,
    pub kind: AuthMethodKind,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub label: String,
    pub methods: Vec<AuthMethod>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSession {
    pub id: AuthSessionId,
    pub provider_id: ProviderId,
    pub auth_method_id: AuthMethodId,
    pub mode: AuthMode,
    pub status: AuthSessionStatus,
    pub next_action: AuthNextAction,
    pub expires_at: Option<String>,
    #[serde(skip_serializing)]
    pub provider_metadata: Value,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthNextAction {
    pub authorization_url: Option<String>,
    pub user_code: Option<String>,
    pub verification_uri: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialRef {
    pub record_id: CredentialRecordId,
    pub vault_handle: VaultHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConnection {
    pub id: ProviderConnectionId,
    pub provider_id: ProviderId,
    pub auth_method_id: AuthMethodId,
    pub status: ConnectionStatus,
    pub account_label: Option<String>,
    pub account_email: Option<String>,
    pub scopes: Vec<String>,
    pub capabilities: Vec<String>,
    pub credential_ref: CredentialRef,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialRecord {
    pub id: CredentialRecordId,
    pub connection_id: ProviderConnectionId,
    pub credential_kind: CredentialKind,
    pub vault_handle: VaultHandle,
    pub expires_at: Option<String>,
    pub fingerprint_hash: Option<String>,
    pub status: CredentialStatus,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct StartAuthRequest {
    pub provider_id: ProviderId,
    pub auth_method_id: AuthMethodId,
}

#[derive(Debug, Clone)]
pub struct CompleteAuthRequest {
    pub session_id: AuthSessionId,
    pub payload: Value,
}

#[derive(Clone)]
pub struct SecretPayload(String);

impl SecretPayload {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_for_vault(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretPayload([redacted])")
    }
}

#[derive(Clone)]
pub struct SecretMaterial {
    pub credential_kind: CredentialKind,
    pub payload: SecretPayload,
    pub expires_at: Option<String>,
    pub fingerprint_hash: Option<String>,
}

impl fmt::Debug for SecretMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretMaterial")
            .field("credential_kind", &self.credential_kind)
            .field("payload", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .field("fingerprint_hash", &self.fingerprint_hash)
            .finish()
    }
}

pub(crate) fn now_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();

    seconds.to_string()
}
