//! Core-owned personalization: user-authored additions to the system prompt.
//!
//! The user can append their own instructions to Mothership's base prompt at
//! three scopes — globally, per provider, or per provider+model. All applicable
//! scopes layer on top of one another (global, then provider, then model), each
//! appended after Core's locked base sections. The global response-language
//! preference is stored with the same view because it is another prompt-level
//! personalization setting, not a frontend-only display preference.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const RESPONSE_LANGUAGE_AUTO: &str = "auto";
pub const RESPONSE_LANGUAGE_CUSTOM: &str = "custom";

/// Global response-language preference. `auto` adds no prompt section; known
/// language ids and `custom` become a Core-authored user preference section.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ResponseLanguageSettings {
    pub language_id: String,
    pub custom_language: String,
}

impl Default for ResponseLanguageSettings {
    fn default() -> Self {
        Self {
            language_id: RESPONSE_LANGUAGE_AUTO.to_string(),
            custom_language: String::new(),
        }
    }
}

impl ResponseLanguageSettings {
    pub fn normalized(language_id: &str, custom_language: &str) -> Self {
        let language_id = language_id.trim().to_ascii_lowercase();
        let custom_language = custom_language.trim().to_string();
        if language_id == RESPONSE_LANGUAGE_CUSTOM {
            if custom_language.is_empty() {
                return Self::default();
            }
            return Self {
                language_id,
                custom_language,
            };
        }
        if response_language_name(&language_id).is_some() {
            return Self {
                language_id,
                custom_language: String::new(),
            };
        }
        Self::default()
    }

    pub fn prompt_instruction(&self) -> Option<String> {
        let language = if self.language_id == RESPONSE_LANGUAGE_CUSTOM {
            self.custom_language.trim()
        } else {
            response_language_name(&self.language_id)?
        };
        if language.is_empty() {
            return None;
        }
        Some(format!(
            "Response language preference: respond in {language} unless the user explicitly asks for another language."
        ))
    }
}

pub fn response_language_name(language_id: &str) -> Option<&'static str> {
    match language_id {
        RESPONSE_LANGUAGE_AUTO => None,
        "en" => Some("English"),
        "ru" => Some("Russian"),
        "es" => Some("Spanish"),
        "de" => Some("German"),
        "fr" => Some("French"),
        "it" => Some("Italian"),
        "pt" => Some("Portuguese"),
        "zh" => Some("Chinese"),
        "ja" => Some("Japanese"),
        "ko" => Some("Korean"),
        "uk" => Some("Ukrainian"),
        "pl" => Some("Polish"),
        "tr" => Some("Turkish"),
        "ar" => Some("Arabic"),
        "hi" => Some("Hindi"),
        _ => None,
    }
}

/// One provider's stored instruction text.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ProviderInstruction {
    pub provider_id: String,
    pub content: String,
}

/// One provider+model's stored instruction text.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ModelInstruction {
    pub provider_id: String,
    pub model_id: String,
    pub content: String,
}

/// The full personalization view the settings UI renders and edits: the global
/// instruction plus every provider- and model-scoped override the user has saved.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PersonalizationSettings {
    pub global: String,
    pub providers: Vec<ProviderInstruction>,
    pub models: Vec<ModelInstruction>,
    pub response_language: ResponseLanguageSettings,
}
