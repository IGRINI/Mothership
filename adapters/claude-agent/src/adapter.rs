use std::collections::BTreeMap;

use anyhow::Context as _;
use mothership_adapter_sdk::protocol::{
    AuthKind, AuthStatus, Model, ModelManagement, SettingsField,
};
use mothership_adapter_sdk::{ChatRequest, ChatRoundOutcome, ChatSink, Context, ProviderAdapter};

use crate::auth::ClaudeAuthState;
use crate::settings::{self, ClaudeAgentSettings};
use crate::{auth, cli, models};

#[derive(Default)]
pub(crate) struct ClaudeAgentAdapter {
    settings: ClaudeAgentSettings,
    /// Shared across rounds so the persistent credentials file is only
    /// rewritten when the configured secret actually changes.
    auth_state: ClaudeAuthState,
}

#[async_trait::async_trait]
impl ProviderAdapter for ClaudeAgentAdapter {
    fn identity(&self) -> (String, String) {
        ("claude-agent".to_string(), "Claude Agent SDK".to_string())
    }

    fn settings_schema(&self) -> Vec<SettingsField> {
        settings::settings_schema()
    }

    fn auth_schema(&self) -> AuthKind {
        AuthKind::ApiKey {
            label: "Claude auth token".to_string(),
        }
    }

    fn auth_status(&self) -> AuthStatus {
        if self.settings.has_credentials() {
            AuthStatus::configured(
                "Claude credentials are present; chat requests are validated by Claude Agent SDK at runtime",
            )
        } else {
            AuthStatus::missing(
                "Paste a `claude setup-token` token or configure a Claude config directory",
            )
        }
    }

    async fn set_settings(&mut self, values: BTreeMap<String, String>) -> anyhow::Result<()> {
        self.settings = ClaudeAgentSettings::from_values(values);
        // The host clears a secret by force-evicting the adapter and pushing the
        // remaining settings to the next spawn; there is no dedicated "secret
        // cleared" frame. So whenever the configured payload no longer carries a
        // credentials JSON, drop the stale persisted copy.
        if auth::persisted_credentials_are_stale(self.settings.credential_payload()) {
            auth::remove_persisted_credentials();
        }
        Ok(())
    }

    async fn models(&mut self, ctx: &Context) -> anyhow::Result<(ModelManagement, Vec<Model>)> {
        if !self.settings.has_credentials() {
            return Ok((ModelManagement::Fixed, Vec::new()));
        }

        let sdk_catalog = cli::supported_models(&self.settings, &self.auth_state, ctx)
            .await
            .context("read Claude Agent SDK model metadata")?;
        Ok((
            ModelManagement::ServerWithCustom,
            models::from_sdk_catalog(sdk_catalog, &self.settings),
        ))
    }

    async fn chat(
        &mut self,
        request: ChatRequest,
        ctx: &Context,
        sink: &mut ChatSink,
    ) -> anyhow::Result<ChatRoundOutcome> {
        if !self.settings.has_credentials() {
            anyhow::bail!(
                "missing Claude credentials (paste a `claude setup-token` token or configure a Claude config directory)"
            );
        }
        cli::stream_chat(&self.settings, &self.auth_state, request, ctx, sink).await
    }

    async fn logout(&mut self, _ctx: &Context) -> anyhow::Result<()> {
        // Core seeds this call with the old settings right before it forgets the
        // stored credential; remove the plaintext copy this adapter persisted.
        auth::remove_persisted_credentials();
        Ok(())
    }
}
