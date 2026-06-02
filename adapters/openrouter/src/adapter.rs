use std::collections::BTreeMap;

use mothership_adapter_sdk::protocol::{
    AuthKind, AuthStatus, Model, ModelManagement, SettingsField,
};
use mothership_adapter_sdk::{ChatRequest, ChatRoundOutcome, Context, ProviderAdapter};

use crate::settings::OpenRouterSettings;

pub(crate) struct OpenRouterAdapter {
    client: reqwest::Client,
    settings: OpenRouterSettings,
}

impl OpenRouterAdapter {
    pub(crate) fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: crate::settings::http_client()?,
            settings: OpenRouterSettings::default(),
        })
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for OpenRouterAdapter {
    fn identity(&self) -> (String, String) {
        ("openrouter".to_string(), "OpenRouter".to_string())
    }

    fn settings_schema(&self) -> Vec<SettingsField> {
        crate::settings::settings_schema()
    }

    fn auth_schema(&self) -> AuthKind {
        AuthKind::ApiKey {
            label: "OpenRouter API key".to_string(),
        }
    }

    fn auth_status(&self) -> AuthStatus {
        if self.settings.api_key().is_empty() {
            AuthStatus::missing("OpenRouter API key is not configured")
        } else {
            AuthStatus::configured("OpenRouter API key is configured")
        }
    }

    async fn set_settings(&mut self, values: BTreeMap<String, String>) -> anyhow::Result<()> {
        self.settings.apply_values(values);
        Ok(())
    }

    async fn models(&mut self, _ctx: &Context) -> anyhow::Result<(ModelManagement, Vec<Model>)> {
        let model_ids = crate::models::parse_model_ids(self.settings.models());
        if model_ids.is_empty() {
            return Ok((ModelManagement::UserDefined, Vec::new()));
        }
        let metadata = match crate::models::fetch_model_metadata(&self.client, &self.settings).await
        {
            Ok(metadata) => metadata,
            Err(error) => {
                eprintln!("openrouter-adapter: model metadata refresh failed: {error:#}");
                BTreeMap::new()
            }
        };
        Ok((
            ModelManagement::UserDefined,
            crate::models::models_from_user_list(model_ids, &metadata),
        ))
    }

    async fn chat(
        &mut self,
        request: ChatRequest,
        _ctx: &Context,
        sink: &mut mothership_adapter_sdk::ChatSink,
    ) -> anyhow::Result<ChatRoundOutcome> {
        crate::chat::stream_chat(&self.client, &self.settings, &request, sink).await
    }
}
