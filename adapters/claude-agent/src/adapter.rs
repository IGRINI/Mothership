use std::collections::BTreeMap;

use anyhow::Context as _;
use mothership_adapter_sdk::protocol::{
    AuthKind, AuthStatus, Model, ModelManagement, SettingsField,
};
use mothership_adapter_sdk::{ChatRequest, ChatRoundOutcome, ChatSink, Context, ProviderAdapter};

use crate::settings::{self, ClaudeAgentSettings};
use crate::{cli, models};

#[derive(Default)]
pub(crate) struct ClaudeAgentAdapter {
    settings: ClaudeAgentSettings,
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
        Ok(())
    }

    async fn models(&mut self, ctx: &Context) -> anyhow::Result<(ModelManagement, Vec<Model>)> {
        if !self.settings.has_credentials() {
            return Ok((ModelManagement::Fixed, Vec::new()));
        }

        let sdk_catalog = cli::supported_models(&self.settings, ctx)
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
        cli::stream_chat(&self.settings, request, ctx, sink).await
    }
}
