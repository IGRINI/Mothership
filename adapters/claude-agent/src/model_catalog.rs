use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeModelInfo {
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) supports_effort: bool,
    #[serde(default)]
    pub(crate) supported_effort_levels: Vec<String>,
    #[serde(default)]
    pub(crate) supports_adaptive_thinking: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ClaudeModelCatalog {
    pub(crate) models: Vec<ClaudeModelInfo>,
    pub(crate) available_models: Option<Vec<String>>,
}
