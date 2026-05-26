//! Wire protocol between the core and an adapter process: newline-delimited JSON.
//!
//! The host sends one [`Request`] per line; the adapter replies with one or more
//! [`Outbound`] messages per line, each echoing the originating request `id`. A
//! chat request produces a stream of `Delta`s terminated by `Done` (or `Error`).
//! Every provider — HTTP, WebSocket, or one that spawns an external CLI — speaks
//! this same contract, so the core never learns how the adapter talks upstream.

use serde::{Deserialize, Serialize};

/// Host -> adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Initialize { id: u64 },
    GetIdentity { id: u64 },
    GetModels { id: u64 },
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
        models: Vec<Model>,
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
