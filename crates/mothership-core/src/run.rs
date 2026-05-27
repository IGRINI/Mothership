use std::path::PathBuf;

use mothership_adapter_host::AdapterRegistry;

use crate::auth::FileCredentialVault;
use crate::chat::{ChatRunEvent, ChatRunEventKind, ChatRunEventSink, SendChatMessageResult};
use crate::llm::{
    LlmChatCompletionEventSink, LlmChatCompletionGateway, LlmChatCompletionRequest, LlmTransportKind,
};
use crate::subprocess_gateway::SubprocessChatGateway;
use crate::{Database, MothershipError, Result};

const CHAT_CONTEXT_LIMIT: i64 = 80;

/// Orchestrates a single streaming chat run inside Core.
///
/// It selects the configured model, finds the subprocess adapter that provides
/// it, builds the chat context, drives the completion through that adapter, and
/// records the terminal state in the database. The core is provider-agnostic —
/// every provider is a runtime-loaded adapter. All observable run progress is
/// forwarded to the injected [`ChatRunEventSink`].
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
                "no LLM model selected; install a provider adapter and choose a model first"
                    .to_string(),
            ));
        }

        // Every provider is a subprocess adapter. Find the one that owns this
        // model; the adapter handles its own transport and auth.
        let adapters = AdapterRegistry::scan(&plugins_store_path(database));
        let entry = adapters.find(&selected_model.provider_id).ok_or_else(|| {
            MothershipError::InvalidRequest(format!(
                "no adapter installed for provider: {}",
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

        let mut llm_sink = DbForwardingSink {
            database,
            run_id: &run.run_id,
            chat_id: &run.chat.id,
            assistant_message_id: &run.assistant_message.id,
            sink,
        };
        // The adapter owns its own system prompt; settings (api key, base url,
        // user model list, OAuth tokens, …) live in the app's shared credential
        // vault, keyed by provider, and are pushed to the adapter on spawn.
        let vault = FileCredentialVault::new(auth_store_path(database));
        SubprocessChatGateway::new(
            entry.program.clone(),
            selected_model.provider_id.clone(),
            vault,
        )
        .complete_chat(
            LlmChatCompletionRequest {
                provider_id: selected_model.provider_id,
                model_id: selected_model.model_id,
                system_prompt: String::new(),
                messages,
            },
            &mut llm_sink,
        )?;

        let event =
            database.complete_chat_run(&run.run_id, &run.chat.id, &run.assistant_message.id)?;
        sink.emit(event);
        Ok(())
    }
}

/// Bridges the LLM gateway's streaming callbacks to durable database state and
/// the run event sink.
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

fn plugins_store_path(database: &Database) -> PathBuf {
    database
        .path()
        .parent()
        .map(|path| path.join("plugins"))
        .unwrap_or_else(|| PathBuf::from("plugins"))
}

/// Root of the app's shared credential vault (sibling of the database). Adapter
/// settings and secrets are stored here, keyed by provider — the same vault the
/// auth subsystem uses.
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
    use crate::chat::{
        ChatMessage, ChatMessageRole, ChatMessageStatus, ChatRunEvent, ChatRunEventKind,
        ChatRunEventSink, SendChatMessageResult,
    };
    use crate::Database;

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

        fn error_text(&self) -> String {
            self.events
                .last()
                .and_then(|event| event.error.clone())
                .unwrap_or_default()
        }
    }

    fn temp_database_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("mothership_run_{name}_{stamp}.sqlite"))
    }

    /// A run handle over a freshly created (message-less) chat. Enough to drive
    /// the orchestration's early error paths without any network.
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
            provider_id: None,
            model_id: None,
        };
        let user_message = ChatMessage {
            id: "chat_message_user_test".to_string(),
            chat_id: chat.id.clone(),
            position: 0,
            role: ChatMessageRole::User,
            content: "Hello there".to_string(),
            status: ChatMessageStatus::Complete,
            created_at: "0".to_string(),
            provider_id: None,
            model_id: None,
        };

        SendChatMessageResult {
            run_id: "chat_run_test".to_string(),
            chat,
            user_message,
            assistant_message,
        }
    }

    #[test]
    fn run_fails_when_no_model_selected() {
        let database_path = temp_database_path("no_model_selected");
        let database = Database::open(&database_path).expect("open database");

        let selected = database.selected_llm_model().expect("selected model");
        assert!(selected.model_id.trim().is_empty());

        let run = run_handle(&database);
        let mut sink = CapturingSink::default();
        ChatRunService::new(&database).run(&run, &mut sink);

        assert_eq!(
            sink.kinds(),
            vec![ChatRunEventKind::Started, ChatRunEventKind::Failed]
        );
        assert!(sink.error_text().contains("no LLM model selected"));

        let _ = fs::remove_file(database_path);
    }
}
