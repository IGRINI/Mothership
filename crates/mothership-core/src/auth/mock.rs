use serde_json::json;

use crate::{MothershipError, Result};

use super::{
    AdapterAuthCompletion, AdapterStartAuthInput, AdapterStartAuthResult, AuthMethod, AuthMethodId,
    AuthMethodKind, AuthMode, AuthNextAction, AuthSessionStatus, CredentialKind,
    OpenAiCodexOAuthAdapter, ProviderAuthAdapter, ProviderId, SecretMaterial, SecretPayload,
    StaticProviderAuthAdapterRegistry,
};

pub struct MockProviderAuthAdapter;

impl MockProviderAuthAdapter {
    pub const PROVIDER_ID: &'static str = "mock-ai";
    pub const AUTH_METHOD_ID: &'static str = "mock_oauth_browser";
}

impl ProviderAuthAdapter for MockProviderAuthAdapter {
    fn provider_id(&self) -> ProviderId {
        ProviderId::from(Self::PROVIDER_ID)
    }

    fn provider_label(&self) -> String {
        "Mock AI".to_string()
    }

    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![AuthMethod {
            id: AuthMethodId::from(Self::AUTH_METHOD_ID),
            provider_id: self.provider_id(),
            kind: AuthMethodKind::Mock,
            label: "Mock OAuth Browser".to_string(),
            description: "Local development auth flow without provider network calls.".to_string(),
        }]
    }

    fn start_auth(&self, input: AdapterStartAuthInput) -> Result<AdapterStartAuthResult> {
        if input.auth_method_id.as_str() != Self::AUTH_METHOD_ID {
            return Err(MothershipError::InvalidRequest(format!(
                "unsupported mock auth method: {}",
                input.auth_method_id
            )));
        }

        Ok(AdapterStartAuthResult {
            mode: AuthMode::Mock,
            next_action: AuthNextAction {
                authorization_url: Some(format!(
                    "https://auth.example.invalid/mock?session={}",
                    input.auth_session_id
                )),
                user_code: Some("MOCK-CODE".to_string()),
                verification_uri: Some("https://auth.example.invalid/mock".to_string()),
                message: Some("Use auth complete in the console test flow.".to_string()),
            },
            expires_at: None,
            provider_metadata: json!({ "adapter": "mock" }),
        })
    }

    fn complete_auth(
        &self,
        input: super::CompleteAuthAdapterInput,
    ) -> Result<AdapterAuthCompletion> {
        if input.session.status != AuthSessionStatus::Pending {
            return Err(MothershipError::InvalidRequest(format!(
                "mock auth session is not pending: {}",
                input.session.id
            )));
        }

        let label = input
            .payload
            .get("accountLabel")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("Mock Account")
            .to_string();

        let secret_payload = json!({
            "type": "mock_oauth_token_set",
            "access_token": format!("mock-access-token-for-{}", input.session.id),
            "refresh_token": format!("mock-refresh-token-for-{}", input.session.id),
            "expires_at": null
        });

        Ok(AdapterAuthCompletion {
            account_label: Some(label),
            account_email: Some("mock@example.invalid".to_string()),
            scopes: vec!["chat".to_string(), "models".to_string()],
            capabilities: vec!["chat_completions".to_string()],
            secret: SecretMaterial {
                credential_kind: CredentialKind::MockTokenSet,
                payload: SecretPayload::new(secret_payload.to_string()),
                expires_at: None,
                fingerprint_hash: Some(format!("mock:{}", input.session.id)),
            },
        })
    }
}

impl StaticProviderAuthAdapterRegistry {
    pub fn with_mock_adapter() -> Self {
        Self::new(vec![Box::new(MockProviderAuthAdapter)])
    }

    pub fn with_openai_codex() -> Self {
        Self::new(vec![Box::new(OpenAiCodexOAuthAdapter::default())])
    }

    pub fn with_mock_and_openai_codex() -> Self {
        Self::new(vec![
            Box::new(MockProviderAuthAdapter),
            Box::new(OpenAiCodexOAuthAdapter::default()),
        ])
    }
}
