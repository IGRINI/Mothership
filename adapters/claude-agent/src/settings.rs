use std::collections::BTreeMap;
use std::path::PathBuf;

use mothership_adapter_sdk::protocol::{SettingsField, SettingsFieldKind, SettingsFieldOption};

use crate::builtin_models::BUILTIN_CLAUDE_MODELS;

pub(crate) const OAUTH_TOKEN_KEY: &str = "oauth_token";
pub(crate) const EXECUTABLE_PATH_KEY: &str = "claude_executable_path";
pub(crate) const CONFIG_DIR_KEY: &str = "claude_config_dir";
pub(crate) const CUSTOM_MODELS_KEY: &str = "models";
pub(crate) const HIDDEN_BUILTIN_MODELS_KEY: &str = "hidden_builtin_models";

#[derive(Debug, Clone, Default)]
pub(crate) struct ClaudeAgentSettings {
    credential_payload: String,
    executable_path: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    custom_models: String,
    hidden_builtin_models: String,
}

impl ClaudeAgentSettings {
    pub(crate) fn from_values(values: BTreeMap<String, String>) -> Self {
        let credential_payload = values
            .get(OAUTH_TOKEN_KEY)
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        let executable_path = values
            .get(EXECUTABLE_PATH_KEY)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let config_dir = values
            .get(CONFIG_DIR_KEY)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let custom_models = values
            .get(CUSTOM_MODELS_KEY)
            .cloned()
            .unwrap_or_default();
        let hidden_builtin_models = values
            .get(HIDDEN_BUILTIN_MODELS_KEY)
            .cloned()
            .unwrap_or_default();

        Self {
            credential_payload,
            executable_path,
            config_dir,
            custom_models,
            hidden_builtin_models,
        }
    }

    pub(crate) fn credential_payload(&self) -> &str {
        &self.credential_payload
    }

    pub(crate) fn executable_path(&self) -> Option<&PathBuf> {
        self.executable_path.as_ref()
    }

    pub(crate) fn config_dir(&self) -> Option<&PathBuf> {
        self.config_dir.as_ref()
    }

    pub(crate) fn custom_models(&self) -> &str {
        &self.custom_models
    }

    pub(crate) fn hidden_builtin_models(&self) -> &str {
        &self.hidden_builtin_models
    }

    pub(crate) fn has_credentials(&self) -> bool {
        !self.credential_payload.trim().is_empty()
            || self
                .config_dir
                .as_ref()
                .map(|path| path.join(".credentials.json").is_file())
                .unwrap_or(false)
            || default_claude_config_dir()
                .map(|path| path.join(".credentials.json").is_file())
                .unwrap_or(false)
    }
}

pub(crate) fn settings_schema() -> Vec<SettingsField> {
    vec![
        SettingsField {
            key: OAUTH_TOKEN_KEY.to_string(),
            label: "Claude credentials JSON".to_string(),
            kind: SettingsFieldKind::Secret,
            required: false,
            options: Vec::new(),
        },
        SettingsField {
            key: CONFIG_DIR_KEY.to_string(),
            label: "Claude config directory (optional)".to_string(),
            kind: SettingsFieldKind::Text,
            required: false,
            options: Vec::new(),
        },
        SettingsField {
            key: EXECUTABLE_PATH_KEY.to_string(),
            label: "Claude executable path (optional)".to_string(),
            kind: SettingsFieldKind::Text,
            required: false,
            options: Vec::new(),
        },
        SettingsField {
            key: CUSTOM_MODELS_KEY.to_string(),
            label: "Custom models".to_string(),
            kind: SettingsFieldKind::StringList,
            required: false,
            options: Vec::new(),
        },
        SettingsField {
            key: HIDDEN_BUILTIN_MODELS_KEY.to_string(),
            label: "Built-in Claude models".to_string(),
            kind: SettingsFieldKind::ModelVisibilityList,
            required: false,
            options: BUILTIN_CLAUDE_MODELS
                .iter()
                .map(|model| SettingsFieldOption {
                    value: model.id.to_string(),
                    label: model.label.to_string(),
                    description: None,
                })
                .collect(),
        },
    ]
}

pub(crate) fn default_claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                })
                .map(|home| home.join(".claude"))
        })
}
