//! Wire protocol between the core and an adapter process: newline-delimited JSON.
//!
//! The host sends one [`Request`] per line; the adapter replies with one or more
//! [`Outbound`] messages per line, each echoing the originating request `id`. A
//! chat request produces a stream of `Delta`s terminated by `Done` (or `Error`).
//! Every provider — HTTP, WebSocket, or one that spawns an external CLI — speaks
//! this same contract, so the core never learns how the adapter talks upstream.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Host -> adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Initialize { id: u64 },
    GetIdentity { id: u64 },
    GetModels { id: u64 },
    GetSettingsSchema { id: u64 },
    SetSettings {
        id: u64,
        values: BTreeMap<String, String>,
    },
    GetAuthSchema { id: u64 },
    /// Ask the adapter to run its own auth flow now (e.g. browser OAuth) and
    /// persist the result via [`Outbound::StoreSecret`]. Drives the UI's
    /// per-adapter "Authorize" action for `oauth_internal` / `external_process`
    /// schemes. Adapters with no interactive auth (api-key) just ack.
    Authenticate { id: u64 },
    ChatStart {
        id: u64,
        model: String,
        messages: Vec<ChatMessage>,
    },
    ChatCancel { id: u64 },
}

/// Adapter -> host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outbound {
    Ack {
        id: u64,
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
    Delta {
        id: u64,
        text: String,
    },
    Done {
        id: u64,
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
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub recommended: bool,
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
