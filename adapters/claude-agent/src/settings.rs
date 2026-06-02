use std::collections::BTreeMap;
use std::path::PathBuf;

use mothership_adapter_sdk::protocol::{SettingsField, SettingsFieldKind};

pub(crate) const OAUTH_TOKEN_KEY: &str = "oauth_token";
pub(crate) const EXECUTABLE_PATH_KEY: &str = "claude_executable_path";

#[derive(Debug, Clone, Default)]
pub(crate) struct ClaudeAgentSettings {
    oauth_token: String,
    executable_path: Option<PathBuf>,
}

impl ClaudeAgentSettings {
    pub(crate) fn from_values(values: BTreeMap<String, String>) -> Self {
        let oauth_token = values
            .get(OAUTH_TOKEN_KEY)
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        let executable_path = values
            .get(EXECUTABLE_PATH_KEY)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);

        Self {
            oauth_token,
            executable_path,
        }
    }

    pub(crate) fn oauth_token(&self) -> &str {
        &self.oauth_token
    }

    pub(crate) fn executable_path(&self) -> Option<&PathBuf> {
        self.executable_path.as_ref()
    }

    pub(crate) fn has_oauth_token(&self) -> bool {
        !self.oauth_token.trim().is_empty()
    }
}

pub(crate) fn settings_schema() -> Vec<SettingsField> {
    vec![
        SettingsField {
            key: OAUTH_TOKEN_KEY.to_string(),
            label: "Claude OAuth token".to_string(),
            kind: SettingsFieldKind::Secret,
            required: true,
        },
        SettingsField {
            key: EXECUTABLE_PATH_KEY.to_string(),
            label: "Claude executable path (optional)".to_string(),
            kind: SettingsFieldKind::Text,
            required: false,
        },
    ]
}
