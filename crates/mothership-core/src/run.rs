use std::path::PathBuf;

use crate::auth::{
    ConnectionStatus, FileCredentialVault, OpenAiCodexOAuthAdapter, ProviderAuthService,
    ProviderConnection, StaticProviderAuthAdapterRegistry,
};
use crate::chat::{ChatRunEvent, ChatRunEventKind, ChatRunEventSink, SendChatMessageResult};
use crate::llm::{
    chat_system_prompt, LlmChatCompletionEventSink, LlmChatCompletionGateway,
    LlmChatCompletionRequest, LlmTransportKind, OpenAiCodexChatCompletionGateway,
};
use crate::{Database, MothershipError, Result};

const CHAT_CONTEXT_LIMIT: i64 = 80;

/// Orchestrates a single streaming chat run inside Core.
///
/// This owns the behavior that used to live in the Tauri host
/// (`run_chat_completion` / `complete_chat_run`): it selects the configured
/// model, finds an active provider connection, builds the chat context,
/// constructs the LLM gateway, drives the completion, and records the
/// terminal state in the database. All observable run progress is forwarded to
/// the injected [`ChatRunEventSink`], so the host only has to plumb those
/// events to the UI.
pub struct ChatRunService<'a> {
    database: &'a Database,
}

impl<'a> ChatRunService<'a> {
    pub fn new(database: &'a Database) -> Self {
        Self { database }
    }

    /// Runs the chat completion for `run` to completion, emitting `Started`,
    /// `TransportSelected`, `Delta`, and finally `Completed` or `Failed`
    /// events through `sink`.
    ///
    /// The terminal database state is always recorded: on success the run is
    /// marked complete, and on any error the run is marked failed (mirroring
    /// the previous host behavior, including the fallback `Failed` event if the
    /// database write itself fails).
    pub fn run(&self, run: &SendChatMessageResult, sink: &mut dyn ChatRunEventSink) {
        sink.emit(ChatRunEvent {
            run_id: run.run_id.clone(),
            chat_id: run.chat.id.clone(),
            message_id: run.assistant_message.id.clone(),
            kind: ChatRunEventKind::Started,
            delta: None,
            message: Some(run.assistant_message.clone()),
            chat: Some(run.chat.clone()),
            transport: None,
            error: None,
        });

        if let Err(error) = self.complete(run, sink) {
            let event = self
                .database
                .fail_chat_run(
                    &run.run_id,
                    &run.chat.id,
                    &run.assistant_message.id,
                    &error.to_string(),
                )
                .unwrap_or_else(|_| ChatRunEvent {
                    run_id: run.run_id.clone(),
                    chat_id: run.chat.id.clone(),
                    message_id: run.assistant_message.id.clone(),
                    kind: ChatRunEventKind::Failed,
                    delta: None,
                    message: None,
                    chat: None,
                    transport: None,
                    error: Some(error.to_string()),
                });
            sink.emit(event);
        }
    }

    fn complete(&self, run: &SendChatMessageResult, sink: &mut dyn ChatRunEventSink) -> Result<()> {
        let database = self.database;

        let selected_model = database.selected_llm_model()?;
        if selected_model.model_id.trim().is_empty() {
            return Err(MothershipError::InvalidRequest(
                "no LLM model selected; connect a provider and choose a model first".to_string(),
            ));
        }

        let connection = self
            .active_connection(&selected_model.provider_id)?
            .ok_or_else(|| {
                MothershipError::InvalidRequest(format!(
                    "no active provider connection for {}",
                    selected_model.provider_id
                ))
            })?;

        let messages =
            database.llm_chat_context(&run.chat.id, &run.assistant_message.id, CHAT_CONTEXT_LIMIT)?;
        if messages.is_empty() {
            return Err(MothershipError::InvalidRequest(
                "chat context is empty".to_string(),
            ));
        }

        let vault = FileCredentialVault::new(auth_store_path(database));
        let mut llm_sink = DbForwardingSink {
            database,
            run_id: &run.run_id,
            chat_id: &run.chat.id,
            assistant_message_id: &run.assistant_message.id,
            sink,
        };

        match selected_model.provider_id.as_str() {
            OpenAiCodexOAuthAdapter::PROVIDER_ID => {
                let gateway = OpenAiCodexChatCompletionGateway::new(&vault, &connection)?;
                let system_prompt =
                    chat_system_prompt(&selected_model.provider_id, &selected_model.model_id)?;
                gateway.complete_chat(
                    LlmChatCompletionRequest {
                        provider_id: selected_model.provider_id,
                        model_id: selected_model.model_id,
                        system_prompt,
                        messages,
                    },
                    &mut llm_sink,
                )?;
            }
            provider_id => {
                return Err(MothershipError::InvalidRequest(format!(
                    "chat runtime is not implemented for provider: {provider_id}"
                )));
            }
        }

        let event =
            database.complete_chat_run(&run.run_id, &run.chat.id, &run.assistant_message.id)?;
        sink.emit(event);
        Ok(())
    }

    fn active_connection(&self, provider_id: &str) -> Result<Option<ProviderConnection>> {
        let vault = FileCredentialVault::new(auth_store_path(self.database));
        let registry = StaticProviderAuthAdapterRegistry::with_openai_codex();
        let service = ProviderAuthService::new(self.database, &vault, &registry);
        let connections = service.list_connections()?;
        Ok(connections.into_iter().find(|connection| {
            connection.provider_id.as_str() == provider_id
                && connection.status == ConnectionStatus::Active
        }))
    }
}

/// Bridges the LLM gateway's streaming callbacks to durable database state and
/// the run event sink.
///
/// This is the half of the old `TauriChatRunSink` that belongs in Core: it
/// performs the per-delta and transport database writes and then forwards the
/// resulting [`ChatRunEvent`] to the host sink. The host keeps only the other
/// half (emitting events to the UI).
struct DbForwardingSink<'a> {
    database: &'a Database,
    run_id: &'a str,
    chat_id: &'a str,
    assistant_message_id: &'a str,
    sink: &'a mut dyn ChatRunEventSink,
}

impl LlmChatCompletionEventSink for DbForwardingSink<'_> {
    fn transport_selected(&mut self, transport: LlmTransportKind) {
        if let Ok(event) = self.database.mark_chat_run_transport(
            self.run_id,
            self.chat_id,
            self.assistant_message_id,
            transport.as_str(),
        ) {
            self.sink.emit(event);
        }
    }

    fn delta(&mut self, delta: &str) {
        if let Ok(event) = self.database.append_chat_run_delta(
            self.run_id,
            self.chat_id,
            self.assistant_message_id,
            delta,
        ) {
            self.sink.emit(event);
        }
    }
}

fn auth_store_path(database: &Database) -> PathBuf {
    database
        .path()
        .parent()
        .map(|path| path.join("auth"))
        .unwrap_or_else(|| PathBuf::from("auth"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::auth::{
        AuthMethodId, ConnectionStatus, CredentialRecordId, CredentialRef, ProviderConnection,
        ProviderConnectionId, ProviderId, VaultHandle,
    };
    use crate::chat::{
        ChatMessage, ChatMessageRole, ChatMessageStatus, ChatRunEvent, ChatRunEventKind,
        ChatRunEventSink, SendChatMessageResult,
    };
    use crate::llm::{LlmModel, LlmModelCatalogCache, LlmModelCatalogRepository};
    use crate::Database;

    const MODEL_PROVIDER: &str = "openai";
    const MODEL_ID: &str = "gpt-5.5";

    #[derive(Default)]
    struct CapturingSink {
        events: Vec<ChatRunEvent>,
    }

    impl ChatRunEventSink for CapturingSink {
        fn emit(&mut self, event: ChatRunEvent) {
            self.events.push(event);
        }
    }

    impl CapturingSink {
        fn kinds(&self) -> Vec<ChatRunEventKind> {
            self.events.iter().map(|event| event.kind).collect()
        }

        fn last(&self) -> &ChatRunEvent {
            self.events.last().expect("at least one event")
        }

        fn error_text(&self) -> String {
            self.last().error.clone().unwrap_or_default()
        }
    }

    fn temp_database_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("mothership_run_{name}_{stamp}.sqlite"))
    }

    /// Builds a run handle that points at a freshly created (message-less) chat
    /// and a synthetic assistant message id. This is enough to drive the
    /// orchestration's pure-DB-state error paths without going near the
    /// network: the `Started` event only clones these fields, and the failure
    /// path records terminal state by id.
    fn run_handle(database: &Database) -> SendChatMessageResult {
        let conversation = database.create_chat().expect("create chat");
        let chat = conversation.chat;
        let assistant_message = ChatMessage {
            id: "chat_message_assistant_test".to_string(),
            chat_id: chat.id.clone(),
            position: 1,
            role: ChatMessageRole::Assistant,
            content: String::new(),
            status: ChatMessageStatus::Sending,
            created_at: "0".to_string(),
        };
        let user_message = ChatMessage {
            id: "chat_message_user_test".to_string(),
            chat_id: chat.id.clone(),
            position: 0,
            role: ChatMessageRole::User,
            content: "Hello there".to_string(),
            status: ChatMessageStatus::Complete,
            created_at: "0".to_string(),
        };

        SendChatMessageResult {
            run_id: "chat_run_test".to_string(),
            chat,
            user_message,
            assistant_message,
        }
    }

    /// Seeds a fresh remote-catalog cache entry and selects the model, so the
    /// orchestration's `selected_llm_model()` check passes. The Codex connector
    /// exposes a remote catalog, so `set_selected_llm_model` only accepts an id
    /// that is present in a fresh cache (mirrors the existing database tests).
    fn select_model(database: &Database) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_secs();
        database
            .save_llm_model_catalog_cache(&LlmModelCatalogCache {
                provider_id: MODEL_PROVIDER.to_string(),
                models: vec![LlmModel {
                    provider_id: MODEL_PROVIDER.to_string(),
                    provider_label: "OpenAI".to_string(),
                    id: MODEL_ID.to_string(),
                    label: MODEL_ID.to_string(),
                    family: "Codex".to_string(),
                    description: "Test model".to_string(),
                    capabilities: vec!["text".to_string()],
                    recommended: true,
                }],
                etag: None,
                fetched_at: now.to_string(),
                expires_at: now.saturating_add(300).to_string(),
            })
            .expect("save catalog cache");
        database
            .set_selected_llm_model(MODEL_PROVIDER, MODEL_ID)
            .expect("set selected model");
    }

    /// Inserts an `Active` provider connection so the active-connection guard
    /// passes. No vault credential is stored: the empty-context guard runs
    /// before any gateway/credential access, so this keeps the test offline.
    fn insert_active_connection(database: &Database) {
        use crate::auth::ProviderAuthRepository;

        let connection = ProviderConnection {
            id: ProviderConnectionId::from("provider_connection_test"),
            provider_id: ProviderId::from(MODEL_PROVIDER),
            auth_method_id: AuthMethodId::from("openai_oauth"),
            status: ConnectionStatus::Active,
            account_label: None,
            account_email: None,
            scopes: Vec::new(),
            capabilities: Vec::new(),
            credential_ref: CredentialRef {
                record_id: CredentialRecordId::from("credential_record_test"),
                vault_handle: VaultHandle::new("file://credentials/missing".to_string()),
            },
            expires_at: None,
            created_at: "0".to_string(),
            updated_at: "0".to_string(),
        };
        database
            .save_provider_connection(&connection)
            .expect("save provider connection");
    }

    fn assert_failed_run(database_path: PathBuf, sink: &CapturingSink) {
        // The run always opens with Started and ends with a single Failed event.
        assert_eq!(
            sink.kinds(),
            vec![ChatRunEventKind::Started, ChatRunEventKind::Failed]
        );
        assert!(
            !sink.error_text().is_empty(),
            "failed event should carry an error message"
        );

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn run_fails_when_no_model_selected() {
        let database_path = temp_database_path("no_model_selected");
        let database = Database::open(&database_path).expect("open database");

        // A fresh database has no selected model, so the run must fail before
        // any network access is attempted.
        let selected = database.selected_llm_model().expect("selected model");
        assert!(selected.model_id.trim().is_empty());

        let run = run_handle(&database);
        let mut sink = CapturingSink::default();
        ChatRunService::new(&database).run(&run, &mut sink);

        assert!(sink.error_text().contains("no LLM model selected"));
        assert_failed_run(database_path, &sink);
    }

    #[test]
    fn run_fails_when_no_active_connection() {
        let database_path = temp_database_path("no_active_connection");
        let database = Database::open(&database_path).expect("open database");

        // Select a model but never connect a provider: the run must fail on
        // the missing active connection, still without network.
        select_model(&database);

        let run = run_handle(&database);
        let mut sink = CapturingSink::default();
        ChatRunService::new(&database).run(&run, &mut sink);

        assert!(sink.error_text().contains("no active provider connection"));
        assert_failed_run(database_path, &sink);
    }

    #[test]
    fn run_fails_when_context_is_empty() {
        let database_path = temp_database_path("empty_context");
        let database = Database::open(&database_path).expect("open database");

        // Model selected + an active connection present, so both earlier guards
        // pass. The run handle points at a message-less chat, so the context
        // lookup returns empty and the run must fail on the empty-context guard
        // (which is checked before any gateway/credential access).
        select_model(&database);
        insert_active_connection(&database);

        let run = run_handle(&database);
        let messages = database
            .llm_chat_context(&run.chat.id, &run.assistant_message.id, CHAT_CONTEXT_LIMIT)
            .expect("context query");
        assert!(messages.is_empty(), "precondition: context must be empty");

        let mut sink = CapturingSink::default();
        ChatRunService::new(&database).run(&run, &mut sink);

        assert!(sink.error_text().contains("chat context is empty"));
        assert_failed_run(database_path, &sink);
    }
}
