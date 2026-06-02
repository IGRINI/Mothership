use std::collections::BTreeMap;

use anyhow::Context as _;
use mothership_adapter_sdk::protocol::{
    AuthKind, AuthStatus, AuthStatusKind, Model, ModelManagement, SettingsField,
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
            label: "Claude OAuth token".to_string(),
        }
    }

    fn auth_status(&self) -> AuthStatus {
        if self.settings.has_oauth_token() {
            AuthStatus {
                kind: AuthStatusKind::Configured,
                account_label: None,
                expires_at: None,
                detail: Some("Claude OAuth token is configured".to_string()),
            }
        } else {
            AuthStatus::missing("Paste a Claude OAuth token to use Claude Agent SDK")
        }
    }

    async fn set_settings(&mut self, values: BTreeMap<String, String>) -> anyhow::Result<()> {
        self.settings = ClaudeAgentSettings::from_values(values);
        Ok(())
    }

    async fn models(&mut self, _ctx: &Context) -> anyhow::Result<(ModelManagement, Vec<Model>)> {
        if !self.settings.has_oauth_token() {
            return Ok((ModelManagement::Fixed, models::fallback_models()));
        }

        let sdk_models = cli::supported_models(&self.settings)
            .await
            .context("read Claude Agent SDK model metadata")?;
        Ok((ModelManagement::Server, models::from_sdk_models(sdk_models)))
    }

    async fn chat(
        &mut self,
        request: ChatRequest,
        _ctx: &Context,
        sink: &mut ChatSink,
    ) -> anyhow::Result<ChatRoundOutcome> {
        if !self.settings.has_oauth_token() {
            anyhow::bail!("missing Claude OAuth token (set it in adapter settings)");
        }
        cli::stream_chat(&self.settings, request, sink).await
    }
}
