//! Core-owned personalization: user-authored additions to the system prompt.
//!
//! The user can append their own instructions to Mothership's base prompt at
//! three scopes — globally, per provider, or per provider+model. All applicable
//! scopes layer on top of one another (global, then provider, then model), each
//! appended after Core's locked base sections. Storage lives in the `app_settings`
//! key-value table; these are just the serializable views the settings UI reads
//! and edits.

use serde::{Deserialize, Serialize};

/// One provider's stored instruction text.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstruction {
    pub provider_id: String,
    pub content: String,
}

/// One provider+model's stored instruction text.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelInstruction {
    pub provider_id: String,
    pub model_id: String,
    pub content: String,
}

/// The full personalization view the settings UI renders and edits: the global
/// instruction plus every provider- and model-scoped override the user has saved.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PersonalizationSettings {
    pub global: String,
    pub providers: Vec<ProviderInstruction>,
    pub models: Vec<ModelInstruction>,
}
