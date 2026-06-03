use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context as _;
use mothership_adapter_sdk::protocol::{SettingsField, SettingsFieldKind};

pub(crate) const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub(crate) const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20 * 60);

#[derive(Default)]
pub(crate) struct OpenRouterSettings {
    api_key: String,
    base_url: String,
    models: String,
}

impl OpenRouterSettings {
    pub(crate) fn apply_values(&mut self, values: BTreeMap<String, String>) {
        self.api_key = values.get("api_key").cloned().unwrap_or_default();
        self.base_url = values.get("base_url").cloned().unwrap_or_default();
        self.models = values.get("models").cloned().unwrap_or_default();
    }

    pub(crate) fn api_key(&self) -> &str {
        self.api_key.trim()
    }

    pub(crate) fn base_url(&self) -> &str {
        if self.base_url.trim().is_empty() {
            DEFAULT_BASE_URL
        } else {
            self.base_url.trim()
        }
    }

    pub(crate) fn models(&self) -> &str {
        &self.models
    }
}

pub(crate) fn settings_schema() -> Vec<SettingsField> {
    vec![
        SettingsField {
            key: "api_key".to_string(),
            label: "OpenRouter API key".to_string(),
            kind: SettingsFieldKind::Secret,
            required: true,
            options: Vec::new(),
        },
        SettingsField {
            key: "base_url".to_string(),
            label: "Base URL (optional)".to_string(),
            kind: SettingsFieldKind::Text,
            required: false,
            options: Vec::new(),
        },
        SettingsField {
            key: "models".to_string(),
            label: "Models".to_string(),
            kind: SettingsFieldKind::StringList,
            required: false,
            options: Vec::new(),
        },
    ]
}

pub(crate) fn auth_headers(api_key: &str) -> Vec<(String, String)> {
    vec![("Authorization".to_string(), format!("Bearer {api_key}"))]
}

pub(crate) fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .context("build OpenRouter HTTP client")
}
