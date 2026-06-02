//! Wire protocol between the host and an adapter process: newline-delimited JSON.
//!
//! The host sends one [`Request`] per line; the adapter replies with one or more
//! [`Outbound`] messages per line, each echoing the originating request `id`. A
//! chat request produces a stream of `Delta`s terminated by
//! `ChatRoundComplete` (or `Error`).
//! Every provider — HTTP, WebSocket, or one that spawns an external CLI — speaks
//! this same contract, so the host never learns how the adapter talks upstream.
//!
//! This crate is the single source of truth for the contract. It is depended on
//! by the host runtime (`mothership-adapter-host`), the Rust adapter SDK
//! (`mothership-adapter-sdk`), and is the documented boundary any out-of-tree /
//! non-Rust adapter implements.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire-protocol version between host and adapter. The host sends it on
/// `initialize` and refuses an adapter that reports a different version, rather
/// than mis-parsing a contract it doesn't understand. Bump on any incompatible
/// change to `Request`/`Outbound`.
pub const PROTOCOL_VERSION: u32 = 8;

/// Host -> adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Initialize {
        id: u64,
        /// The host's protocol version. Defaulted so a frame from an older host
        /// (without the field) still parses.
        #[serde(default)]
        protocol_version: u32,
    },
    GetIdentity {
        id: u64,
    },
    GetModels {
        id: u64,
    },
    GetSettingsSchema {
        id: u64,
    },
    SetSettings {
        id: u64,
        values: BTreeMap<String, String>,
    },
    GetAuthSchema {
        id: u64,
    },
    GetAuthStatus {
        id: u64,
    },
    /// Ask the adapter to run its own auth flow now (e.g. browser OAuth) and
    /// persist the result via [`Outbound::StoreSecret`]. Drives the UI's
    /// per-adapter "Authorize" action for `oauth_internal` / `external_process`
    /// schemes. Adapters with no interactive auth (api-key) just ack.
    Authenticate {
        id: u64,
    },
    ChatStart {
        id: u64,
        model: String,
        #[serde(default)]
        reasoning: Option<ReasoningConfig>,
        #[serde(default)]
        prompt: PromptBundle,
        #[serde(default)]
        runtime_context: RuntimeContext,
        messages: Vec<ChatMessage>,
        #[serde(default)]
        tools: Vec<ToolDescriptor>,
        /// Opaque provider-specific continuation state returned by the previous
        /// round. Core stores this only in the active run actor and never
        /// interprets provider wire shapes.
        #[serde(default)]
        state: Option<Value>,
        /// Tool results Core decided to execute after the previous round.
        #[serde(default)]
        tool_results: Vec<ToolCallResponse>,
        /// Extra user-facing messages Core wants appended to the provider
        /// continuation, for example a final no-tool synthesis instruction.
        #[serde(default)]
        extra_messages: Vec<ChatMessage>,
    },
    ChatCancel {
        id: u64,
    },
    /// Ask the adapter to revoke / clean up its current auth (e.g. revoke an
    /// OAuth token server-side) before the host forgets the stored credential.
    /// Best-effort: the adapter acks even if revoke fails. Adapters with nothing
    /// to revoke (api-key) just ack.
    Logout {
        id: u64,
    },
}

/// Adapter -> host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outbound {
    Ack {
        id: u64,
    },
    /// Reply to `initialize`: confirms the adapter is alive and reports the
    /// protocol version it implements, so the host can refuse a mismatch.
    Initialized {
        id: u64,
        protocol_version: u32,
    },
    Identity {
        id: u64,
        provider_id: String,
        provider_label: String,
    },
    Models {
        id: u64,
        management: ModelManagement,
        models: Vec<Model>,
    },
    SettingsSchema {
        id: u64,
        fields: Vec<SettingsField>,
    },
    AuthSchema {
        id: u64,
        auth: AuthKind,
    },
    AuthStatus {
        id: u64,
        status: AuthStatus,
    },
    Delta {
        id: u64,
        text: String,
    },
    /// One provider model round has finished. If `tool_calls` is non-empty, Core
    /// owns the next decision: execute/queue/cancel those calls, then start a
    /// new round with the returned opaque `state` plus `tool_results`.
    ChatRoundComplete {
        id: u64,
        #[serde(default)]
        state: Option<Value>,
        #[serde(default)]
        tool_calls: Vec<ToolCallInvocation>,
    },
    Error {
        id: u64,
        message: String,
    },
    /// Adapter -> host side channel: persist these secret values in the app's
    /// shared credential store (e.g. an OAuth token the adapter just obtained or
    /// refreshed). Not tied to a request id.
    StoreSecret {
        values: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallInvocation {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallResponse {
    pub tool_call_id: String,
    pub result: ToolCallResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeContext {
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub project_name: Option<String>,
    #[serde(default)]
    pub project_root: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptBundle {
    #[serde(default)]
    pub sections: Vec<PromptSection>,
}

impl PromptBundle {
    pub fn rendered_text(&self) -> String {
        let mut sections = self
            .sections
            .iter()
            .filter(|section| !section.content.trim().is_empty())
            .collect::<Vec<_>>();
        sections.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.id.cmp(&right.id))
        });

        sections
            .into_iter()
            .map(|section| section.content.trim())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSection {
    pub id: String,
    #[serde(default)]
    pub source: String,
    pub priority: i32,
    pub locked: bool,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub strict: bool,
    #[serde(default)]
    pub annotations: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallResult {
    pub ok: bool,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub recommended: bool,
    #[serde(default)]
    pub reasoning: Option<ReasoningCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningCapabilities {
    pub supported: bool,
    #[serde(default)]
    pub efforts: Vec<ReasoningEffort>,
    #[serde(default)]
    pub options: Vec<ReasoningOption>,
    #[serde(default)]
    pub supports_budget: bool,
    #[serde(default)]
    pub supports_exclusion: bool,
    #[serde(default)]
    pub supports_summary: bool,
}

impl ReasoningCapabilities {
    pub fn from_efforts(
        efforts: Vec<ReasoningEffort>,
        supports_budget: bool,
        supports_exclusion: bool,
        supports_summary: bool,
    ) -> Self {
        let mut options = if efforts.is_empty() {
            Vec::new()
        } else {
            vec![ReasoningOption::auto()]
        };
        options.extend(efforts.iter().copied().map(ReasoningOption::from_effort));

        Self {
            supported: true,
            efforts,
            options,
            supports_budget,
            supports_exclusion,
            supports_summary,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub recommended: bool,
    #[serde(default)]
    pub config: ReasoningConfig,
}

impl ReasoningOption {
    pub fn from_effort(effort: ReasoningEffort) -> Self {
        let id = effort.as_wire_str().to_string();
        Self {
            id: id.clone(),
            label: id,
            description: None,
            recommended: false,
            config: ReasoningConfig {
                effort: Some(effort),
                budget_tokens: None,
                summary: None,
            },
        }
    }

    pub fn auto() -> Self {
        Self {
            id: "auto".to_string(),
            label: "auto".to_string(),
            description: None,
            recommended: true,
            config: ReasoningConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningConfig {
    #[serde(default)]
    pub effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub budget_tokens: Option<u32>,
    #[serde(default)]
    pub summary: Option<ReasoningSummary>,
}

impl ReasoningConfig {
    pub fn is_empty(&self) -> bool {
        self.effort.is_none() && self.budget_tokens.is_none() && self.summary.is_none()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl ReasoningEffort {
    pub fn openai_responses_values() -> Vec<Self> {
        vec![
            Self::None,
            Self::Minimal,
            Self::Low,
            Self::Medium,
            Self::High,
            Self::XHigh,
        ]
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x_high" | "very_high" | "very-high" | "very high" => Some(Self::XHigh),
            "max" | "maximum" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn ordinal(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Minimal => 1,
            Self::Low => 2,
            Self::Medium => 3,
            Self::High => 4,
            Self::XHigh => 5,
            Self::Max => 6,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
}

impl ReasoningSummary {
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Concise => "concise",
            Self::Detailed => "detailed",
        }
    }
}

/// How a provider's model list is managed — drives whether the UI lets the user
/// add models (the "+").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelManagement {
    /// A fixed built-in set.
    Fixed,
    /// Fetched from the provider's server (e.g. Codex).
    Server,
    /// The user maintains the list (e.g. OpenRouter); UI shows a "+".
    UserDefined,
}

/// One settings field the adapter asks the UI to render and feed back via
/// `set_settings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsField {
    pub key: String,
    pub label: String,
    pub kind: SettingsFieldKind,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsFieldKind {
    Text,
    Secret,
    Bool,
    /// An editable list of strings (e.g. a user-defined model-id list). The host
    /// renders add/remove rows; the value is stored as the items joined by `\n`.
    StringList,
}

/// The adapter's auth scheme. Secret values (API keys) are persisted by the host
/// in its credential vault; oauth / external-process flows are owned by the
/// adapter itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthKind {
    /// No auth needed.
    None,
    /// The user supplies an API key (stored as a secret setting).
    ApiKey { label: String },
    /// The adapter performs its own OAuth.
    OauthInternal,
    /// The adapter launches and drives an external process (e.g. claude-code).
    ExternalProcess,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatusKind {
    NotRequired,
    Missing,
    Configured,
    Authenticated,
    Expired,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub kind: AuthStatusKind,
    #[serde(default)]
    pub account_label: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

impl AuthStatus {
    pub fn not_required() -> Self {
        Self {
            kind: AuthStatusKind::NotRequired,
            account_label: None,
            expires_at: None,
            detail: None,
        }
    }

    pub fn missing(detail: impl Into<String>) -> Self {
        Self {
            kind: AuthStatusKind::Missing,
            account_label: None,
            expires_at: None,
            detail: Some(detail.into()),
        }
    }

    pub fn configured(detail: impl Into<String>) -> Self {
        Self {
            kind: AuthStatusKind::Configured,
            account_label: None,
            expires_at: None,
            detail: Some(detail.into()),
        }
    }

    pub fn authenticated(account_label: Option<String>, expires_at: Option<String>) -> Self {
        Self {
            kind: AuthStatusKind::Authenticated,
            account_label,
            expires_at,
            detail: None,
        }
    }

    pub fn expired(account_label: Option<String>, expires_at: Option<String>) -> Self {
        Self {
            kind: AuthStatusKind::Expired,
            account_label,
            expires_at,
            detail: Some("credential expired".to_string()),
        }
    }

    pub fn error(detail: impl Into<String>) -> Self {
        Self {
            kind: AuthStatusKind::Error,
            account_label: None,
            expires_at: None,
            detail: Some(detail.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_wire_values_are_centralized() {
        assert_eq!(ReasoningEffort::parse("max"), Some(ReasoningEffort::Max));
        assert_eq!(
            ReasoningEffort::parse("maximum"),
            Some(ReasoningEffort::Max)
        );
        assert_eq!(
            ReasoningEffort::parse("x_high"),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            ReasoningEffort::parse("very_high"),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            ReasoningEffort::parse("very-high"),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(ReasoningEffort::High.as_wire_str(), "high");
        assert_eq!(ReasoningEffort::XHigh.as_wire_str(), "xhigh");
        assert_eq!(ReasoningEffort::Max.as_wire_str(), "max");
        assert!(ReasoningEffort::Low.ordinal() < ReasoningEffort::High.ordinal());
        assert!(ReasoningEffort::XHigh.ordinal() < ReasoningEffort::Max.ordinal());
    }

    #[test]
    fn reasoning_capabilities_build_provider_options_from_efforts() {
        let capabilities = ReasoningCapabilities::from_efforts(
            vec![ReasoningEffort::Low, ReasoningEffort::Max],
            true,
            false,
            false,
        );

        assert_eq!(
            capabilities.options,
            vec![
                ReasoningOption::auto(),
                ReasoningOption::from_effort(ReasoningEffort::Low),
                ReasoningOption::from_effort(ReasoningEffort::Max)
            ]
        );
        assert!(capabilities.supports_budget);
    }

    #[test]
    fn reasoning_summary_wire_values_are_centralized() {
        assert_eq!(ReasoningSummary::Auto.as_wire_str(), "auto");
        assert_eq!(ReasoningSummary::Concise.as_wire_str(), "concise");
        assert_eq!(ReasoningSummary::Detailed.as_wire_str(), "detailed");
    }
}
