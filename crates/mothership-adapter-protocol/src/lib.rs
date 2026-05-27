//! Wire protocol between the host and an adapter process: newline-delimited JSON.
//!
//! The host sends one [`Request`] per line; the adapter replies with one or more
//! [`Outbound`] messages per line, each echoing the originating request `id`. A
//! chat request produces a stream of `Delta`s terminated by `Done` (or `Error`).
//! Every provider — HTTP, WebSocket, or one that spawns an external CLI — speaks
//! this same contract, so the host never learns how the adapter talks upstream.
//!
//! This crate is the single source of truth for the contract. It is depended on
//! by the host runtime (`mothership-adapter-host`), the Rust adapter SDK
//! (`mothership-adapter-sdk`), and is the documented boundary any out-of-tree /
//! non-Rust adapter implements.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Wire-protocol version between host and adapter. The host sends it on
/// `initialize` and refuses an adapter that reports a different version, rather
/// than mis-parsing a contract it doesn't understand. Bump on any incompatible
/// change to `Request`/`Outbound`.
pub const PROTOCOL_VERSION: u32 = 1;

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
    /// Ask the adapter to revoke / clean up its current auth (e.g. revoke an
    /// OAuth token server-side) before the host forgets the stored credential.
    /// Best-effort: the adapter acks even if revoke fails. Adapters with nothing
    /// to revoke (api-key) just ack.
    Logout { id: u64 },
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
