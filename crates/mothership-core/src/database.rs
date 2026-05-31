use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, params_from_iter, types::Type, Connection, OptionalExtension};

use crate::{
    id::generate_id, ActivityEvent, ChatConversation, ChatMessage, ChatMessageRole,
    ChatMessageStatus, ChatRunEvent, ChatRunEventKind, ChatThreadSummary, DashboardMetric,
    DashboardSnapshot, LlmChatMessage, LlmChatRole, MothershipError, Result, SelectedLlmModel,
    SendChatMessageResult, SidecarStatus, ToolCommand, ToolExecutionEvent, ToolExecutionEventKind,
    ToolExecutionRecord, ToolExecutionResult, ToolOutputStream, WorkspaceItem,
};

const WORKSPACE_LIMIT: i64 = 2_500;
const EVENT_LIMIT: i64 = 5_000;
const CHAT_LIST_LIMIT: i64 = 100;
const CHAT_MESSAGE_LIMIT: i64 = 200;
const CHAT_MESSAGE_MAX_BYTES: usize = 20_000;
const TOOL_OUTPUT_DISPLAY_MAX_BYTES: usize = 12_000;
const DEFAULT_MODEL_SCOPE: &str = "default";

#[derive(Debug, Clone)]
pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let database = Self { path: path.into() };

        if let Some(parent) = database.path.parent() {
            fs::create_dir_all(parent)?;
        }

        {
            let mut connection = database.connect()?;
            migrate(&connection)?;
            seed(&mut connection)?;
        }

        Ok(database)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn snapshot(&self) -> Result<DashboardSnapshot> {
        let connection = self.connect()?;

        let workspace_total = count(&connection, "workspace_items")?;
        let event_total = count(&connection, "activity_events")?;

        let workspace_items = select_workspace_items(&connection)?;
        let activity_events = select_activity_events(&connection)?;

        Ok(DashboardSnapshot {
            metrics: vec![
                DashboardMetric {
                    label: "Workspace rows".to_string(),
                    value: workspace_total.to_string(),
                    tone: "data".to_string(),
                },
                DashboardMetric {
                    label: "Activity events".to_string(),
                    value: event_total.to_string(),
                    tone: "signal".to_string(),
                },
                DashboardMetric {
                    label: "Virtualized rows".to_string(),
                    value: (workspace_items.len() + activity_events.len()).to_string(),
                    tone: "compute".to_string(),
                },
                DashboardMetric {
                    label: "SQLite mode".to_string(),
                    value: "WAL".to_string(),
                    tone: "storage".to_string(),
                },
            ],
            workspace_items,
            activity_events,
        })
    }

    pub fn append_activity_event(&self, message: &str) -> Result<ActivityEvent> {
        let message = message.trim();
        if message.is_empty() {
            return Err(MothershipError::InvalidRequest(
                "activity message cannot be empty".to_string(),
            ));
        }

        if message.len() > 500 {
            return Err(MothershipError::InvalidRequest(
                "activity message cannot exceed 500 characters".to_string(),
            ));
        }

        let connection = self.connect()?;
        let occurred_at = current_timestamp();

        connection.execute(
            "INSERT INTO activity_events (source, level, message, occurred_at) VALUES (?1, ?2, ?3, ?4)",
            params!["ui", "info", message, occurred_at],
        )?;

        let id = connection.last_insert_rowid();

        Ok(ActivityEvent {
            id,
            source: "ui".to_string(),
            level: "info".to_string(),
            message: message.to_string(),
            occurred_at,
        })
    }

    pub fn list_chats(&self, limit: i64) -> Result<Vec<ChatThreadSummary>> {
        let connection = self.connect()?;
        select_chat_summaries(&connection, normalize_limit(limit, CHAT_LIST_LIMIT))
    }

    pub fn create_chat(&self) -> Result<ChatConversation> {
        let connection = self.connect()?;
        let now = current_timestamp();
        let chat = ChatThreadSummary {
            id: generate_id("chat")?,
            title: "New chat".to_string(),
            preview: String::new(),
            message_count: 0,
            created_at: now.clone(),
            updated_at: now,
        };

        connection.execute(
            "
            INSERT INTO chats (id, title, preview, message_count, archived, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)
            ",
            params![
                chat.id,
                chat.title,
                chat.preview,
                chat.message_count,
                chat.created_at,
                chat.updated_at
            ],
        )?;

        Ok(ChatConversation {
            chat,
            messages: Vec::new(),
            tool_executions: Vec::new(),
        })
    }

    pub fn get_chat(&self, chat_id: &str, limit: i64) -> Result<ChatConversation> {
        validate_identifier("chat_id", chat_id)?;

        let connection = self.connect()?;
        let chat = select_chat_summary(&connection, chat_id)?;
        let messages = select_chat_messages(
            &connection,
            chat_id,
            normalize_limit(limit, CHAT_MESSAGE_LIMIT),
        )?;
        let message_ids = messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>();
        let tool_executions = select_chat_tool_executions(&connection, chat_id, &message_ids)?;

        Ok(ChatConversation {
            chat,
            messages,
            tool_executions,
        })
    }

    pub fn record_chat_tool_execution_event(
        &self,
        chat_id: &str,
        message_id: &str,
        event: &ToolExecutionEvent,
    ) -> Result<()> {
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("message_id", message_id)?;
        validate_identifier("tool_call_id", &event.tool_call_id)?;

        let connection = self.connect()?;
        let occurred_at = current_timestamp();
        let command_json = json_string(&event.command)?;
        let result_json = json_string(&event.result)?;

        connection.execute(
            "
            INSERT INTO chat_tool_events (
                chat_id,
                message_id,
                tool_call_id,
                run_id,
                project_id,
                command_json,
                kind,
                stream,
                chunk,
                message,
                result_json,
                occurred_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ",
            params![
                chat_id,
                message_id,
                event.tool_call_id.as_str(),
                event.run_id.as_deref(),
                event.project_id.as_deref(),
                command_json,
                tool_event_kind_to_db(event.kind),
                event.stream.map(tool_output_stream_to_db),
                event.chunk.as_deref(),
                event.message.as_deref(),
                result_json,
                occurred_at
            ],
        )?;

        Ok(())
    }

    pub fn send_chat_message(
        &self,
        chat_id: Option<&str>,
        content: &str,
    ) -> Result<SendChatMessageResult> {
        self.begin_chat_run(chat_id, content)
    }

    pub fn recover_interrupted_chat_runs(&self) -> Result<usize> {
        let connection = self.connect()?;
        let interrupted_message =
            "Run interrupted before completion. Send a new message to start another run.";

        let changed = connection.execute(
            "
            UPDATE chat_messages
            SET status = ?1,
                content = CASE
                    WHEN trim(content) = '' THEN ?2
                    ELSE content || ?3
                END
            WHERE role = ?4
              AND status = ?5
            ",
            params![
                chat_status_to_db(ChatMessageStatus::Failed),
                interrupted_message,
                format!("\n\n{interrupted_message}"),
                chat_role_to_db(ChatMessageRole::Assistant),
                chat_status_to_db(ChatMessageStatus::Sending),
            ],
        )?;

        Ok(changed)
    }

    pub fn begin_chat_run(
        &self,
        chat_id: Option<&str>,
        content: &str,
    ) -> Result<SendChatMessageResult> {
        let content = validate_chat_message_content(content)?;
        let mut connection = self.connect()?;
        let selected_model = selected_llm_model(&connection)?;
        if selected_model.model_id.trim().is_empty() {
            return Err(MothershipError::InvalidRequest(
                "no LLM model selected; connect a provider and choose a model first".to_string(),
            ));
        }

        let tx = connection.transaction()?;
        let now = current_timestamp();
        let chat = match chat_id {
            Some(id) => {
                validate_identifier("chat_id", id)?;
                select_chat_summary(&tx, id)?
            }
            None => {
                let chat = ChatThreadSummary {
                    id: generate_id("chat")?,
                    title: derive_chat_title(content),
                    preview: String::new(),
                    message_count: 0,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                };
                tx.execute(
                    "
                    INSERT INTO chats (id, title, preview, message_count, archived, created_at, updated_at)
                    VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)
                    ",
                    params![
                        chat.id,
                        chat.title,
                        chat.preview,
                        chat.message_count,
                        chat.created_at,
                        chat.updated_at
                    ],
                )?;
                chat
            }
        };

        let mut user_message = ChatMessage {
            id: generate_id("chat_message")?,
            chat_id: chat.id.clone(),
            position: 0,
            role: ChatMessageRole::User,
            content: content.to_string(),
            status: ChatMessageStatus::Complete,
            created_at: now.clone(),
            provider_id: None,
            model_id: None,
        };
        let mut assistant_message = ChatMessage {
            id: generate_id("chat_message")?,
            chat_id: chat.id.clone(),
            position: 0,
            role: ChatMessageRole::Assistant,
            content: String::new(),
            status: ChatMessageStatus::Sending,
            created_at: now.clone(),
            // Attribute the reply to the model that will produce it.
            provider_id: Some(selected_model.provider_id.clone()),
            model_id: Some(selected_model.model_id.clone()),
        };

        user_message.position = insert_chat_message(&tx, &user_message)?;
        assistant_message.position = insert_chat_message(&tx, &assistant_message)?;

        let title = if chat.message_count == 0 && chat.title == "New chat" {
            derive_chat_title(content)
        } else {
            chat.title
        };
        let updated_chat = ChatThreadSummary {
            id: chat.id,
            title,
            preview: derive_chat_preview(content),
            message_count: chat.message_count + 2,
            created_at: chat.created_at,
            updated_at: now,
        };

        tx.execute(
            "
            UPDATE chats
            SET title = ?2,
                preview = ?3,
                message_count = ?4,
                updated_at = ?5
            WHERE id = ?1
            ",
            params![
                updated_chat.id,
                updated_chat.title,
                updated_chat.preview,
                updated_chat.message_count,
                updated_chat.updated_at
            ],
        )?;

        tx.commit()?;

        Ok(SendChatMessageResult {
            run_id: generate_id("chat_run")?,
            chat: updated_chat,
            user_message,
            assistant_message,
        })
    }

    /// Edits a completed user message, deletes all later messages in the same
    /// chat, and creates a fresh assistant placeholder after the edited prompt.
    pub fn begin_edited_chat_run(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<SendChatMessageResult> {
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("message_id", message_id)?;
        let content = validate_chat_message_content(content)?;
        let mut connection = self.connect()?;
        let selected_model = selected_llm_model(&connection)?;
        if selected_model.model_id.trim().is_empty() {
            return Err(MothershipError::InvalidRequest(
                "no LLM model selected; connect a provider and choose a model first".to_string(),
            ));
        }

        let tx = connection.transaction()?;
        let chat = select_chat_summary(&tx, chat_id)?;
        let original_user_message = select_chat_message_by_id(&tx, message_id)?;
        if original_user_message.chat_id != chat_id {
            return Err(MothershipError::InvalidRequest(
                "message does not belong to the selected chat".to_string(),
            ));
        }
        if original_user_message.role != ChatMessageRole::User {
            return Err(MothershipError::InvalidRequest(
                "only user messages can be edited and re-sent".to_string(),
            ));
        }
        if original_user_message.status != ChatMessageStatus::Complete {
            return Err(MothershipError::InvalidRequest(
                "only completed user messages can be edited".to_string(),
            ));
        }

        let messages_before_target =
            count_chat_messages_before(&tx, chat_id, original_user_message.position)?;
        tx.execute(
            "DELETE FROM chat_messages WHERE chat_id = ?1 AND rowid > ?2",
            params![chat_id, original_user_message.position],
        )?;

        let now = current_timestamp();
        tx.execute(
            "
            UPDATE chat_messages
            SET content = ?1,
                status = ?2,
                created_at = ?3,
                provider_id = NULL,
                model_id = NULL
            WHERE id = ?4 AND chat_id = ?5
            ",
            params![
                content,
                chat_status_to_db(ChatMessageStatus::Complete),
                now,
                message_id,
                chat_id
            ],
        )?;

        let user_message = ChatMessage {
            content: content.to_string(),
            status: ChatMessageStatus::Complete,
            created_at: now.clone(),
            provider_id: None,
            model_id: None,
            ..original_user_message
        };
        let mut assistant_message = ChatMessage {
            id: generate_id("chat_message")?,
            chat_id: chat_id.to_string(),
            position: 0,
            role: ChatMessageRole::Assistant,
            content: String::new(),
            status: ChatMessageStatus::Sending,
            created_at: now.clone(),
            provider_id: Some(selected_model.provider_id.clone()),
            model_id: Some(selected_model.model_id.clone()),
        };
        assistant_message.position = insert_chat_message(&tx, &assistant_message)?;

        let message_count = count_chat_messages(&tx, chat_id)?;
        let title = if messages_before_target == 0 {
            derive_chat_title(content)
        } else {
            chat.title
        };
        let updated_chat = ChatThreadSummary {
            id: chat.id,
            title,
            preview: derive_chat_preview(content),
            message_count,
            created_at: chat.created_at,
            updated_at: now,
        };
        update_chat_summary(&tx, &updated_chat)?;
        tx.commit()?;

        Ok(SendChatMessageResult {
            run_id: generate_id("chat_run")?,
            chat: updated_chat,
            user_message,
            assistant_message,
        })
    }

    /// Creates a new chat whose messages are copied from `chat_id` through the
    /// selected assistant message, inclusive. The copied chat is independent:
    /// message ids and copied tool-call ids are regenerated.
    pub fn branch_chat_from_message(
        &self,
        chat_id: &str,
        message_id: &str,
    ) -> Result<ChatConversation> {
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("message_id", message_id)?;

        let mut connection = self.connect()?;
        let tx = connection.transaction()?;
        let source_chat = select_chat_summary(&tx, chat_id)?;
        let branch_point = select_chat_message_by_id(&tx, message_id)?;
        if branch_point.chat_id != chat_id {
            return Err(MothershipError::InvalidRequest(
                "message does not belong to the selected chat".to_string(),
            ));
        }
        if branch_point.role != ChatMessageRole::Assistant {
            return Err(MothershipError::InvalidRequest(
                "chat branches can only start from assistant messages".to_string(),
            ));
        }
        if branch_point.status == ChatMessageStatus::Sending {
            return Err(MothershipError::InvalidRequest(
                "cannot branch from a message that is still streaming".to_string(),
            ));
        }

        let source_messages =
            select_chat_messages_through_position(&tx, chat_id, branch_point.position)?;
        if source_messages.is_empty() {
            return Err(MothershipError::InvalidRequest(
                "no chat messages to copy".to_string(),
            ));
        }

        let now = current_timestamp();
        let chat = ChatThreadSummary {
            id: generate_id("chat")?,
            title: branch_chat_title(&source_chat.title),
            preview: derive_chat_preview(&branch_point.content),
            message_count: source_messages.len() as i64,
            created_at: now.clone(),
            updated_at: now,
        };
        insert_chat_summary(&tx, &chat)?;

        let mut copied_messages = Vec::with_capacity(source_messages.len());
        let mut message_id_map = HashMap::with_capacity(source_messages.len());
        for source_message in source_messages {
            let source_message_id = source_message.id.clone();
            let mut copied_message = ChatMessage {
                id: generate_id("chat_message")?,
                chat_id: chat.id.clone(),
                position: 0,
                ..source_message
            };
            message_id_map.insert(source_message_id, copied_message.id.clone());
            copied_message.position = insert_chat_message(&tx, &copied_message)?;
            copied_messages.push(copied_message);
        }

        copy_chat_tool_events(&tx, chat_id, &chat.id, &message_id_map)?;
        let copied_message_ids = copied_messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>();
        let tool_executions = select_chat_tool_executions(&tx, &chat.id, &copied_message_ids)?;
        tx.commit()?;

        Ok(ChatConversation {
            chat,
            messages: copied_messages,
            tool_executions,
        })
    }

    /// Rolls the last (failed) assistant message in a chat back to a fresh
    /// pending state in place and returns a run handle to re-drive it — reusing
    /// the existing user message instead of creating a duplicate exchange.
    /// Errors if there's nothing to retry (no failed assistant message).
    pub fn begin_retry_run(&self, chat_id: &str) -> Result<SendChatMessageResult> {
        validate_identifier("chat_id", chat_id)?;
        let mut connection = self.connect()?;
        let selected_model = selected_llm_model(&connection)?;
        if selected_model.model_id.trim().is_empty() {
            return Err(MothershipError::InvalidRequest(
                "no LLM model selected; connect a provider and choose a model first".to_string(),
            ));
        }

        let tx = connection.transaction()?;
        let chat = select_chat_summary(&tx, chat_id)?;

        let assistant_message = select_last_message_by_role(&tx, chat_id, "assistant")?
            .ok_or_else(|| {
                MothershipError::InvalidRequest("no assistant message to retry".to_string())
            })?;
        if assistant_message.status != ChatMessageStatus::Failed {
            return Err(MothershipError::InvalidRequest(
                "the latest run did not fail; nothing to retry".to_string(),
            ));
        }
        let user_message = select_last_message_by_role(&tx, chat_id, "user")?.ok_or_else(|| {
            MothershipError::InvalidRequest("no user message to retry".to_string())
        })?;

        tx.execute(
            "
            UPDATE chat_messages
            SET content = '', status = ?2, provider_id = ?4, model_id = ?5
            WHERE id = ?1 AND chat_id = ?3
            ",
            params![
                assistant_message.id,
                chat_status_to_db(ChatMessageStatus::Sending),
                chat_id,
                selected_model.provider_id,
                selected_model.model_id
            ],
        )?;
        tx.execute(
            "DELETE FROM chat_tool_events WHERE message_id = ?1",
            params![assistant_message.id],
        )?;

        let now = current_timestamp();
        tx.execute(
            "UPDATE chats SET updated_at = ?2 WHERE id = ?1",
            params![chat_id, now],
        )?;
        tx.commit()?;

        Ok(SendChatMessageResult {
            run_id: generate_id("chat_run")?,
            chat: ChatThreadSummary {
                updated_at: now,
                ..chat
            },
            user_message,
            // Re-attribute to the model the retry will actually use.
            assistant_message: ChatMessage {
                content: String::new(),
                status: ChatMessageStatus::Sending,
                provider_id: Some(selected_model.provider_id.clone()),
                model_id: Some(selected_model.model_id.clone()),
                ..assistant_message
            },
        })
    }

    pub fn llm_chat_context(
        &self,
        chat_id: &str,
        assistant_message_id: &str,
        limit: i64,
    ) -> Result<Vec<LlmChatMessage>> {
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("assistant_message_id", assistant_message_id)?;

        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "
            SELECT role, content
            FROM (
                SELECT rowid, role, content
                FROM chat_messages
                WHERE chat_id = ?1
                    AND id <> ?2
                    AND status = 'complete'
                    AND role IN ('user', 'assistant')
                ORDER BY rowid DESC
                LIMIT ?3
            )
            ORDER BY rowid ASC
            ",
        )?;
        let rows = statement.query_map(
            params![
                chat_id,
                assistant_message_id,
                normalize_limit(limit, CHAT_MESSAGE_LIMIT)
            ],
            |row| {
                let role: String = row.get(0)?;
                let role = match role.as_str() {
                    "user" => LlmChatRole::User,
                    "assistant" => LlmChatRole::Assistant,
                    _ => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            0,
                            Type::Text,
                            Box::new(MothershipError::InvalidRequest(format!(
                                "unsupported LLM message role: {role}"
                            ))),
                        ))
                    }
                };

                Ok(LlmChatMessage {
                    role,
                    content: row.get(1)?,
                })
            },
        )?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn append_chat_run_delta(
        &self,
        run_id: &str,
        chat_id: &str,
        assistant_message_id: &str,
        delta: &str,
    ) -> Result<ChatRunEvent> {
        validate_identifier("run_id", run_id)?;
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("assistant_message_id", assistant_message_id)?;

        if delta.is_empty() {
            return Ok(ChatRunEvent {
                run_id: run_id.to_string(),
                chat_id: chat_id.to_string(),
                message_id: assistant_message_id.to_string(),
                kind: ChatRunEventKind::Delta,
                delta: Some(String::new()),
                message: Some(select_chat_message_by_id(
                    &self.connect()?,
                    assistant_message_id,
                )?),
                chat: None,
                transport: None,
                error: None,
            });
        }

        let connection = self.connect()?;
        connection.execute(
            "
            UPDATE chat_messages
            SET content = content || ?2
            WHERE id = ?1 AND chat_id = ?3
            ",
            params![assistant_message_id, delta, chat_id],
        )?;

        Ok(ChatRunEvent {
            run_id: run_id.to_string(),
            chat_id: chat_id.to_string(),
            message_id: assistant_message_id.to_string(),
            kind: ChatRunEventKind::Delta,
            delta: Some(delta.to_string()),
            message: Some(select_chat_message_by_id(
                &connection,
                assistant_message_id,
            )?),
            chat: None,
            transport: None,
            error: None,
        })
    }

    pub fn mark_chat_run_transport(
        &self,
        run_id: &str,
        chat_id: &str,
        assistant_message_id: &str,
        transport: &str,
    ) -> Result<ChatRunEvent> {
        Ok(ChatRunEvent {
            run_id: run_id.to_string(),
            chat_id: chat_id.to_string(),
            message_id: assistant_message_id.to_string(),
            kind: ChatRunEventKind::TransportSelected,
            delta: None,
            message: None,
            chat: None,
            transport: Some(transport.to_string()),
            error: None,
        })
    }

    pub fn complete_chat_run(
        &self,
        run_id: &str,
        chat_id: &str,
        assistant_message_id: &str,
    ) -> Result<ChatRunEvent> {
        validate_identifier("run_id", run_id)?;
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("assistant_message_id", assistant_message_id)?;

        let connection = self.connect()?;
        connection.execute(
            "
            UPDATE chat_messages
            SET status = 'complete'
            WHERE id = ?1 AND chat_id = ?2
            ",
            params![assistant_message_id, chat_id],
        )?;
        let message = select_chat_message_by_id(&connection, assistant_message_id)?;
        let chat = update_chat_after_assistant(&connection, chat_id, &message.content)?;

        Ok(ChatRunEvent {
            run_id: run_id.to_string(),
            chat_id: chat_id.to_string(),
            message_id: assistant_message_id.to_string(),
            kind: ChatRunEventKind::Completed,
            delta: None,
            message: Some(message),
            chat: Some(chat),
            transport: None,
            error: None,
        })
    }

    pub fn cancel_chat_run(
        &self,
        run_id: &str,
        chat_id: &str,
        assistant_message_id: &str,
    ) -> Result<ChatRunEvent> {
        validate_identifier("run_id", run_id)?;
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("assistant_message_id", assistant_message_id)?;

        let connection = self.connect()?;
        connection.execute(
            "
            UPDATE chat_messages
            SET status = ?2
            WHERE id = ?1 AND chat_id = ?3
            ",
            params![
                assistant_message_id,
                chat_status_to_db(ChatMessageStatus::Cancelled),
                chat_id
            ],
        )?;
        let message = select_chat_message_by_id(&connection, assistant_message_id)?;
        let preview = if message.content.trim().is_empty() {
            "Response cancelled."
        } else {
            &message.content
        };
        let chat = update_chat_after_assistant(&connection, chat_id, preview)?;

        Ok(ChatRunEvent {
            run_id: run_id.to_string(),
            chat_id: chat_id.to_string(),
            message_id: assistant_message_id.to_string(),
            kind: ChatRunEventKind::Cancelled,
            delta: None,
            message: Some(message),
            chat: Some(chat),
            transport: None,
            error: None,
        })
    }

    pub fn fail_chat_run(
        &self,
        run_id: &str,
        chat_id: &str,
        assistant_message_id: &str,
        error: &str,
    ) -> Result<ChatRunEvent> {
        validate_identifier("run_id", run_id)?;
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("assistant_message_id", assistant_message_id)?;

        let error = error.trim();
        let content = if error.is_empty() {
            "LLM request failed.".to_string()
        } else {
            error.to_string()
        };
        let connection = self.connect()?;
        connection.execute(
            "
            UPDATE chat_messages
            SET status = 'failed',
                content = ?2
            WHERE id = ?1 AND chat_id = ?3
            ",
            params![assistant_message_id, content, chat_id],
        )?;
        let message = select_chat_message_by_id(&connection, assistant_message_id)?;
        let chat = update_chat_after_assistant(&connection, chat_id, &message.content)?;

        Ok(ChatRunEvent {
            run_id: run_id.to_string(),
            chat_id: chat_id.to_string(),
            message_id: assistant_message_id.to_string(),
            kind: ChatRunEventKind::Failed,
            delta: None,
            message: Some(message),
            chat: Some(chat),
            transport: None,
            error: Some(content),
        })
    }

    pub fn selected_llm_model(&self) -> Result<SelectedLlmModel> {
        let connection = self.connect()?;
        selected_llm_model(&connection)
    }

    pub fn set_selected_llm_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<SelectedLlmModel> {
        validate_identifier("provider_id", provider_id)?;
        validate_identifier("model_id", model_id)?;

        // The core is provider-agnostic and just persists the choice. Validation
        // against the available adapter models happens at the app layer, which
        // knows the installed adapters.
        let connection = self.connect()?;
        let now = current_timestamp();
        connection.execute(
            "
            INSERT INTO llm_model_preferences (scope, provider_id, model_id, updated_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(scope) DO UPDATE SET
                provider_id = excluded.provider_id,
                model_id = excluded.model_id,
                updated_at = excluded.updated_at
            ",
            params![DEFAULT_MODEL_SCOPE, provider_id, model_id, now],
        )?;

        selected_llm_model(&connection)
    }

    pub fn sidecar_status(&self) -> Result<SidecarStatus> {
        let connection = self.connect()?;
        let page_count: i64 =
            connection.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))?;
        let page_size: i64 =
            connection.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))?;

        Ok(SidecarStatus {
            healthy: true,
            database_path: self.path.display().to_string(),
            workspace_items: count(&connection, "workspace_items")?,
            activity_events: count(&connection, "activity_events")?,
            database_bytes: page_count.saturating_mul(page_size),
        })
    }

    pub(crate) fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            ",
        )?;
        Ok(connection)
    }
}

fn migrate(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS workspace_items (
            id INTEGER PRIMARY KEY,
            kind TEXT NOT NULL,
            namespace TEXT NOT NULL,
            name TEXT NOT NULL,
            status TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_workspace_items_kind_status
            ON workspace_items (kind, status);

        CREATE TABLE IF NOT EXISTS activity_events (
            id INTEGER PRIMARY KEY,
            source TEXT NOT NULL,
            level TEXT NOT NULL,
            message TEXT NOT NULL,
            occurred_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_activity_events_occurred_at
            ON activity_events (occurred_at DESC);

        CREATE TABLE IF NOT EXISTS chats (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            preview TEXT NOT NULL,
            message_count INTEGER NOT NULL DEFAULT 0,
            archived INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_chats_updated_at
            ON chats (updated_at DESC);

        CREATE TABLE IF NOT EXISTS chat_messages (
            id TEXT PRIMARY KEY,
            chat_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            provider_id TEXT,
            model_id TEXT,
            FOREIGN KEY(chat_id) REFERENCES chats(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_chat_messages_chat
            ON chat_messages (chat_id);

        CREATE TABLE IF NOT EXISTS chat_tool_events (
            id INTEGER PRIMARY KEY,
            chat_id TEXT NOT NULL,
            message_id TEXT NOT NULL,
            tool_call_id TEXT NOT NULL,
            run_id TEXT,
            project_id TEXT,
            command_json TEXT,
            kind TEXT NOT NULL,
            stream TEXT,
            chunk TEXT,
            message TEXT,
            result_json TEXT,
            occurred_at TEXT NOT NULL,
            FOREIGN KEY(chat_id) REFERENCES chats(id) ON DELETE CASCADE,
            FOREIGN KEY(message_id) REFERENCES chat_messages(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_chat_tool_events_chat
            ON chat_tool_events (chat_id, id);

        CREATE INDEX IF NOT EXISTS idx_chat_tool_events_message
            ON chat_tool_events (message_id, id);

        CREATE TABLE IF NOT EXISTS llm_model_preferences (
            scope TEXT PRIMARY KEY,
            provider_id TEXT NOT NULL,
            model_id TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        ",
    )?;

    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![1_i64, current_timestamp()],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![2_i64, current_timestamp()],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![3_i64, current_timestamp()],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![4_i64, current_timestamp()],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![5_i64, current_timestamp()],
    )?;

    // Migration 6: per-message model attribution. The CREATE above already has
    // these columns for fresh databases; ALTER brings existing ones up to date
    // (guarded, so re-running is a no-op).
    add_column_if_missing(connection, "chat_messages", "provider_id", "TEXT")?;
    add_column_if_missing(connection, "chat_messages", "model_id", "TEXT")?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![6_i64, current_timestamp()],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![7_i64, current_timestamp()],
    )?;

    Ok(())
}

/// Adds a column to a table if it isn't already present. `table` and `column`
/// are fixed internal identifiers (never user input), so the formatted SQL is
/// safe. SQLite has no `ADD COLUMN IF NOT EXISTS`, hence the PRAGMA check.
fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let present = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(std::result::Result::ok)
        .any(|name| name == column);
    if !present {
        connection.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
    }
    Ok(())
}

fn seed(connection: &mut Connection) -> Result<()> {
    if count(connection, "workspace_items")? > 0 {
        return Ok(());
    }

    let tx = connection.transaction()?;
    let kinds = ["agent", "source", "queue", "index", "task"];
    let statuses = ["active", "idle", "queued", "research", "blocked"];
    let namespaces = ["core", "tauri", "solid", "sidecar", "research"];

    for index in 0..WORKSPACE_LIMIT {
        tx.execute(
            "INSERT INTO workspace_items (kind, namespace, name, status, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                kinds[index as usize % kinds.len()],
                namespaces[index as usize % namespaces.len()],
                format!("mothership-unit-{index:04}"),
                statuses[index as usize % statuses.len()],
                current_timestamp(),
            ],
        )?;
    }

    for index in 0..EVENT_LIMIT {
        let source = if index % 3 == 0 { "sidecar" } else { "app" };
        let level = if index % 17 == 0 { "warn" } else { "info" };
        tx.execute(
            "INSERT INTO activity_events (source, level, message, occurred_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                source,
                level,
                format!("virtualized pipeline event #{index:04}"),
                current_timestamp(),
            ],
        )?;
    }

    tx.commit()?;
    Ok(())
}

fn select_workspace_items(connection: &Connection) -> Result<Vec<WorkspaceItem>> {
    let mut statement = connection.prepare(
        "
        SELECT id, kind, namespace, name, status, updated_at
        FROM workspace_items
        ORDER BY id ASC
        LIMIT ?1
        ",
    )?;

    let rows = statement.query_map(params![WORKSPACE_LIMIT], |row| {
        Ok(WorkspaceItem {
            id: row.get(0)?,
            kind: row.get(1)?,
            namespace: row.get(2)?,
            name: row.get(3)?,
            status: row.get(4)?,
            updated_at: row.get(5)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn select_activity_events(connection: &Connection) -> Result<Vec<ActivityEvent>> {
    let mut statement = connection.prepare(
        "
        SELECT id, source, level, message, occurred_at
        FROM activity_events
        ORDER BY id DESC
        LIMIT ?1
        ",
    )?;

    let rows = statement.query_map(params![EVENT_LIMIT], |row| {
        Ok(ActivityEvent {
            id: row.get(0)?,
            source: row.get(1)?,
            level: row.get(2)?,
            message: row.get(3)?,
            occurred_at: row.get(4)?,
        })
    })?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn select_chat_summaries(connection: &Connection, limit: i64) -> Result<Vec<ChatThreadSummary>> {
    let mut statement = connection.prepare(
        "
        SELECT id, title, preview, message_count, created_at, updated_at
        FROM chats
        WHERE archived = 0
        ORDER BY updated_at DESC, rowid DESC
        LIMIT ?1
        ",
    )?;

    let rows = statement.query_map(params![limit], chat_summary_from_row)?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn select_chat_summary(connection: &Connection, chat_id: &str) -> Result<ChatThreadSummary> {
    connection
        .query_row(
            "
            SELECT id, title, preview, message_count, created_at, updated_at
            FROM chats
            WHERE id = ?1 AND archived = 0
            ",
            params![chat_id],
            chat_summary_from_row,
        )
        .optional()?
        .ok_or_else(|| MothershipError::InvalidRequest(format!("chat not found: {chat_id}")))
}

fn insert_chat_summary(connection: &Connection, chat: &ChatThreadSummary) -> Result<()> {
    connection.execute(
        "
        INSERT INTO chats (id, title, preview, message_count, archived, created_at, updated_at)
        VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)
        ",
        params![
            chat.id,
            chat.title,
            chat.preview,
            chat.message_count,
            chat.created_at,
            chat.updated_at
        ],
    )?;
    Ok(())
}

fn update_chat_summary(connection: &Connection, chat: &ChatThreadSummary) -> Result<()> {
    connection.execute(
        "
        UPDATE chats
        SET title = ?2,
            preview = ?3,
            message_count = ?4,
            updated_at = ?5
        WHERE id = ?1
        ",
        params![
            chat.id,
            chat.title,
            chat.preview,
            chat.message_count,
            chat.updated_at
        ],
    )?;
    Ok(())
}

fn count_chat_messages(connection: &Connection, chat_id: &str) -> Result<i64> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM chat_messages WHERE chat_id = ?1",
            params![chat_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn count_chat_messages_before(
    connection: &Connection,
    chat_id: &str,
    position: i64,
) -> Result<i64> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM chat_messages WHERE chat_id = ?1 AND rowid < ?2",
            params![chat_id, position],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn select_chat_messages(
    connection: &Connection,
    chat_id: &str,
    limit: i64,
) -> Result<Vec<ChatMessage>> {
    let mut statement = connection.prepare(
        "
        SELECT id, chat_id, rowid, role, content, status, created_at, provider_id, model_id
        FROM (
            SELECT rowid, id, chat_id, role, content, status, created_at, provider_id, model_id
            FROM chat_messages
            WHERE chat_id = ?1
            ORDER BY rowid DESC
            LIMIT ?2
        )
        ORDER BY rowid ASC
        ",
    )?;

    let rows = statement.query_map(params![chat_id, limit], chat_message_from_row)?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn select_chat_messages_through_position(
    connection: &Connection,
    chat_id: &str,
    through_position: i64,
) -> Result<Vec<ChatMessage>> {
    let mut statement = connection.prepare(
        "
        SELECT id, chat_id, rowid, role, content, status, created_at, provider_id, model_id
        FROM chat_messages
        WHERE chat_id = ?1 AND rowid <= ?2
        ORDER BY rowid ASC
        ",
    )?;

    let rows = statement.query_map(params![chat_id, through_position], chat_message_from_row)?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn select_chat_tool_executions(
    connection: &Connection,
    chat_id: &str,
    message_ids: &[&str],
) -> Result<Vec<ToolExecutionRecord>> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat("?")
        .take(message_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "
        SELECT id, message_id, tool_call_id, run_id, project_id, command_json, kind, stream, chunk, message, result_json, occurred_at
        FROM chat_tool_events
        WHERE chat_id = ?
          AND message_id IN ({placeholders})
        ORDER BY id ASC
        "
    );
    let mut statement = connection.prepare(&sql)?;
    let params = std::iter::once(chat_id)
        .chain(message_ids.iter().copied())
        .collect::<Vec<_>>();
    let rows = statement.query_map(params_from_iter(params), chat_tool_event_row_from_row)?;

    let mut records = Vec::<ToolExecutionRecord>::new();
    let mut index_by_tool_call_id = HashMap::<String, usize>::new();
    for row in rows {
        let row = row?;
        let record_index = match index_by_tool_call_id.get(&row.tool_call_id) {
            Some(index) => *index,
            None => {
                let index = records.len();
                index_by_tool_call_id.insert(row.tool_call_id.clone(), index);
                records.push(ToolExecutionRecord {
                    tool_call_id: row.tool_call_id.clone(),
                    run_id: row.run_id.clone(),
                    chat_id: chat_id.to_string(),
                    message_id: row.message_id.clone(),
                    project_id: row.project_id.clone(),
                    command: row.command.clone(),
                    kind: row.kind,
                    message: row.message.clone(),
                    output: String::new(),
                    result: row.result.clone(),
                    created_at: row.occurred_at.clone(),
                    updated_at: row.occurred_at.clone(),
                });
                index
            }
        };

        apply_tool_event_row(&mut records[record_index], row);
    }

    Ok(records)
}

fn copy_chat_tool_events(
    connection: &Connection,
    source_chat_id: &str,
    target_chat_id: &str,
    message_id_map: &HashMap<String, String>,
) -> Result<()> {
    if message_id_map.is_empty() {
        return Ok(());
    }

    let source_message_ids = message_id_map
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let placeholders = std::iter::repeat("?")
        .take(source_message_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "
        SELECT message_id, tool_call_id, project_id, command_json, kind, stream, chunk, message, result_json, occurred_at
        FROM chat_tool_events
        WHERE chat_id = ?
          AND message_id IN ({placeholders})
        ORDER BY id ASC
        "
    );

    struct EventCopyRow {
        message_id: String,
        tool_call_id: String,
        project_id: Option<String>,
        command_json: Option<String>,
        kind: String,
        stream: Option<String>,
        chunk: Option<String>,
        message: Option<String>,
        result_json: Option<String>,
        occurred_at: String,
    }

    let rows = {
        let mut statement = connection.prepare(&sql)?;
        let params = std::iter::once(source_chat_id)
            .chain(source_message_ids.iter().copied())
            .collect::<Vec<_>>();
        let rows = statement.query_map(params_from_iter(params), |row| {
            Ok(EventCopyRow {
                message_id: row.get(0)?,
                tool_call_id: row.get(1)?,
                project_id: row.get(2)?,
                command_json: row.get(3)?,
                kind: row.get(4)?,
                stream: row.get(5)?,
                chunk: row.get(6)?,
                message: row.get(7)?,
                result_json: row.get(8)?,
                occurred_at: row.get(9)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    let mut tool_call_id_map = HashMap::<String, String>::new();
    for row in rows {
        let Some(target_message_id) = message_id_map.get(&row.message_id) else {
            continue;
        };
        let target_tool_call_id = match tool_call_id_map.get(&row.tool_call_id) {
            Some(id) => id.clone(),
            None => {
                let id = generate_id("tool_call")?;
                tool_call_id_map.insert(row.tool_call_id.clone(), id.clone());
                id
            }
        };

        connection.execute(
            "
            INSERT INTO chat_tool_events (
                chat_id,
                message_id,
                tool_call_id,
                run_id,
                project_id,
                command_json,
                kind,
                stream,
                chunk,
                message,
                result_json,
                occurred_at
            )
            VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ",
            params![
                target_chat_id,
                target_message_id,
                target_tool_call_id,
                row.project_id,
                row.command_json,
                row.kind,
                row.stream,
                row.chunk,
                row.message,
                row.result_json,
                row.occurred_at
            ],
        )?;
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct ChatToolEventRow {
    message_id: String,
    tool_call_id: String,
    run_id: Option<String>,
    project_id: Option<String>,
    command: Option<ToolCommand>,
    kind: ToolExecutionEventKind,
    stream: Option<ToolOutputStream>,
    chunk: Option<String>,
    message: Option<String>,
    result: Option<ToolExecutionResult>,
    occurred_at: String,
}

fn chat_tool_event_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatToolEventRow> {
    let command_json: Option<String> = row.get(5)?;
    let result_json: Option<String> = row.get(10)?;
    Ok(ChatToolEventRow {
        message_id: row.get(1)?,
        tool_call_id: row.get(2)?,
        run_id: row.get(3)?,
        project_id: row.get(4)?,
        command: json_value(command_json, 5)?,
        kind: tool_event_kind_from_db(&row.get::<_, String>(6)?, 6)?,
        stream: match row.get::<_, Option<String>>(7)? {
            Some(stream) => Some(tool_output_stream_from_db(&stream, 7)?),
            None => None,
        },
        chunk: row.get(8)?,
        message: row.get(9)?,
        result: json_value(result_json, 10)?,
        occurred_at: row.get(11)?,
    })
}

fn apply_tool_event_row(record: &mut ToolExecutionRecord, row: ChatToolEventRow) {
    record.kind = row.kind;
    record.updated_at = row.occurred_at;

    if row.run_id.is_some() {
        record.run_id = row.run_id;
    }
    if row.project_id.is_some() {
        record.project_id = row.project_id;
    }
    if row.command.is_some() {
        record.command = row.command;
    }
    if row.message.is_some() {
        record.message = row.message;
    }
    if row.result.is_some() {
        record.result = row.result;
    }
    if let Some(chunk) = row.chunk {
        append_tool_output(&mut record.output, row.stream, &chunk);
    }
}

fn append_tool_output(output: &mut String, stream: Option<ToolOutputStream>, chunk: &str) {
    if stream == Some(ToolOutputStream::Stderr) {
        output.push_str("[stderr] ");
    }
    output.push_str(chunk);

    if output.len() > TOOL_OUTPUT_DISPLAY_MAX_BYTES {
        let keep_from = output
            .char_indices()
            .map(|(index, _)| index)
            .find(|index| output.len() - *index <= TOOL_OUTPUT_DISPLAY_MAX_BYTES)
            .unwrap_or(output.len());
        let tail = output[keep_from..].to_string();
        output.clear();
        output.push_str("... output trimmed ...\n");
        output.push_str(&tail);
    }
}

fn insert_chat_message(connection: &Connection, message: &ChatMessage) -> Result<i64> {
    connection.execute(
        "
        INSERT INTO chat_messages (id, chat_id, role, content, status, created_at, provider_id, model_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ",
        params![
            message.id,
            message.chat_id,
            chat_role_to_db(message.role),
            message.content,
            chat_status_to_db(message.status),
            message.created_at,
            message.provider_id,
            message.model_id
        ],
    )?;

    Ok(connection.last_insert_rowid())
}

fn select_chat_message_by_id(connection: &Connection, message_id: &str) -> Result<ChatMessage> {
    connection
        .query_row(
            "
            SELECT id, chat_id, rowid, role, content, status, created_at, provider_id, model_id
            FROM chat_messages
            WHERE id = ?1
            ",
            params![message_id],
            chat_message_from_row,
        )
        .optional()?
        .ok_or_else(|| {
            MothershipError::InvalidRequest(format!("chat message not found: {message_id}"))
        })
}

/// The most recent message of `role` in a chat (highest rowid), if any. Used by
/// retry to find the prompt and the failed assistant message to re-run.
fn select_last_message_by_role(
    connection: &Connection,
    chat_id: &str,
    role: &str,
) -> Result<Option<ChatMessage>> {
    connection
        .query_row(
            "
            SELECT id, chat_id, rowid, role, content, status, created_at, provider_id, model_id
            FROM chat_messages
            WHERE chat_id = ?1 AND role = ?2
            ORDER BY rowid DESC
            LIMIT 1
            ",
            params![chat_id, role],
            chat_message_from_row,
        )
        .optional()
        .map_err(Into::into)
}

fn update_chat_after_assistant(
    connection: &Connection,
    chat_id: &str,
    assistant_content: &str,
) -> Result<ChatThreadSummary> {
    let now = current_timestamp();
    connection.execute(
        "
        UPDATE chats
        SET preview = ?2,
            updated_at = ?3
        WHERE id = ?1
        ",
        params![chat_id, derive_chat_preview(assistant_content), now],
    )?;
    select_chat_summary(connection, chat_id)
}

fn selected_llm_model(connection: &Connection) -> Result<SelectedLlmModel> {
    let selected = connection
        .query_row(
            "
            SELECT provider_id, model_id, updated_at
            FROM llm_model_preferences
            WHERE scope = ?1
            ",
            params![DEFAULT_MODEL_SCOPE],
            |row| {
                Ok(SelectedLlmModel {
                    provider_id: row.get(0)?,
                    model_id: row.get(1)?,
                    updated_at: row.get(2)?,
                })
            },
        )
        .optional()?;

    Ok(selected.unwrap_or_else(|| SelectedLlmModel {
        provider_id: "openai".to_string(),
        model_id: String::new(),
        updated_at: current_timestamp(),
    }))
}

fn chat_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatThreadSummary> {
    Ok(ChatThreadSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        preview: row.get(2)?,
        message_count: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn chat_message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatMessage> {
    let role_value: String = row.get(3)?;
    let status_value: String = row.get(5)?;

    Ok(ChatMessage {
        id: row.get(0)?,
        chat_id: row.get(1)?,
        position: row.get(2)?,
        role: chat_role_from_db(&role_value)?,
        content: row.get(4)?,
        status: chat_status_from_db(&status_value)?,
        created_at: row.get(6)?,
        provider_id: row.get(7)?,
        model_id: row.get(8)?,
    })
}

fn chat_role_to_db(role: ChatMessageRole) -> &'static str {
    match role {
        ChatMessageRole::Assistant => "assistant",
        ChatMessageRole::User => "user",
    }
}

fn chat_role_from_db(value: &str) -> rusqlite::Result<ChatMessageRole> {
    match value {
        "assistant" => Ok(ChatMessageRole::Assistant),
        "user" => Ok(ChatMessageRole::User),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            2,
            Type::Text,
            Box::new(MothershipError::InvalidRequest(format!(
                "unsupported chat message role: {value}"
            ))),
        )),
    }
}

fn chat_status_to_db(status: ChatMessageStatus) -> &'static str {
    match status {
        ChatMessageStatus::Complete => "complete",
        ChatMessageStatus::Cancelled => "cancelled",
        ChatMessageStatus::Failed => "failed",
        ChatMessageStatus::Sending => "sending",
    }
}

fn chat_status_from_db(value: &str) -> rusqlite::Result<ChatMessageStatus> {
    match value {
        "complete" => Ok(ChatMessageStatus::Complete),
        "cancelled" => Ok(ChatMessageStatus::Cancelled),
        "failed" => Ok(ChatMessageStatus::Failed),
        "sending" => Ok(ChatMessageStatus::Sending),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            4,
            Type::Text,
            Box::new(MothershipError::InvalidRequest(format!(
                "unsupported chat message status: {value}"
            ))),
        )),
    }
}

fn tool_event_kind_to_db(kind: ToolExecutionEventKind) -> &'static str {
    match kind {
        ToolExecutionEventKind::Queued => "queued",
        ToolExecutionEventKind::PermissionRequested => "permission_requested",
        ToolExecutionEventKind::PermissionDenied => "permission_denied",
        ToolExecutionEventKind::WaitingForResource => "waiting_for_resource",
        ToolExecutionEventKind::Started => "started",
        ToolExecutionEventKind::Output => "output",
        ToolExecutionEventKind::Completed => "completed",
        ToolExecutionEventKind::Failed => "failed",
        ToolExecutionEventKind::Cancelled => "cancelled",
        ToolExecutionEventKind::TimedOut => "timed_out",
    }
}

fn tool_event_kind_from_db(value: &str, column: usize) -> rusqlite::Result<ToolExecutionEventKind> {
    match value {
        "queued" => Ok(ToolExecutionEventKind::Queued),
        "permission_requested" => Ok(ToolExecutionEventKind::PermissionRequested),
        "permission_denied" => Ok(ToolExecutionEventKind::PermissionDenied),
        "waiting_for_resource" => Ok(ToolExecutionEventKind::WaitingForResource),
        "started" => Ok(ToolExecutionEventKind::Started),
        "output" => Ok(ToolExecutionEventKind::Output),
        "completed" => Ok(ToolExecutionEventKind::Completed),
        "failed" => Ok(ToolExecutionEventKind::Failed),
        "cancelled" => Ok(ToolExecutionEventKind::Cancelled),
        "timed_out" => Ok(ToolExecutionEventKind::TimedOut),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Text,
            Box::new(MothershipError::InvalidRequest(format!(
                "unsupported tool event kind: {value}"
            ))),
        )),
    }
}

fn tool_output_stream_to_db(stream: ToolOutputStream) -> &'static str {
    match stream {
        ToolOutputStream::Stdout => "stdout",
        ToolOutputStream::Stderr => "stderr",
    }
}

fn tool_output_stream_from_db(value: &str, column: usize) -> rusqlite::Result<ToolOutputStream> {
    match value {
        "stdout" => Ok(ToolOutputStream::Stdout),
        "stderr" => Ok(ToolOutputStream::Stderr),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Text,
            Box::new(MothershipError::InvalidRequest(format!(
                "unsupported tool output stream: {value}"
            ))),
        )),
    }
}

fn json_string<T: serde::Serialize>(value: &Option<T>) -> Result<Option<String>> {
    value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

fn json_value<T: serde::de::DeserializeOwned>(
    value: Option<String>,
    column: usize,
) -> rusqlite::Result<Option<T>> {
    value
        .map(|json| {
            serde_json::from_str(&json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
            })
        })
        .transpose()
}

fn validate_identifier(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(MothershipError::InvalidRequest(format!(
            "{name} cannot be empty"
        )));
    }

    Ok(())
}

fn validate_chat_message_content(content: &str) -> Result<&str> {
    let content = content.trim();
    if content.is_empty() {
        return Err(MothershipError::InvalidRequest(
            "chat message cannot be empty".to_string(),
        ));
    }

    if content.len() > CHAT_MESSAGE_MAX_BYTES {
        return Err(MothershipError::InvalidRequest(format!(
            "chat message cannot exceed {CHAT_MESSAGE_MAX_BYTES} bytes"
        )));
    }

    Ok(content)
}

fn normalize_limit(limit: i64, default_limit: i64) -> i64 {
    if limit <= 0 {
        default_limit
    } else {
        limit.min(default_limit)
    }
}

fn derive_chat_title(content: &str) -> String {
    let title = compact_whitespace(content);
    if title.is_empty() {
        return "New chat".to_string();
    }

    truncate_chars(&title, 64)
}

fn branch_chat_title(source_title: &str) -> String {
    let title = compact_whitespace(source_title);
    if title.is_empty() || title == "New chat" {
        return "New chat branch".to_string();
    }

    truncate_chars(&format!("{title} branch"), 64)
}

fn derive_chat_preview(content: &str) -> String {
    truncate_chars(&compact_whitespace(content), 140)
}

fn compact_whitespace(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(content: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (index, character) in content.chars().enumerate() {
        if index == max_chars {
            output.push_str("...");
            break;
        }
        output.push(character);
    }
    output
}

fn count(connection: &Connection, table: &str) -> Result<i64> {
    let sql = match table {
        "workspace_items" => "SELECT COUNT(*) FROM workspace_items",
        "activity_events" => "SELECT COUNT(*) FROM activity_events",
        _ => {
            return Err(MothershipError::InvalidRequest(format!(
                "unsupported table: {table}"
            )))
        }
    };

    connection
        .query_row(sql, [], |row| row.get::<_, i64>(0))
        .map_err(Into::into)
}

fn current_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();

    seconds.to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::ToolExecutionStatus;

    #[test]
    fn chat_message_creates_persistent_conversation() {
        let database_path = temp_database_path("chat_message_creates_persistent_conversation");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");

        let result = database
            .send_chat_message(None, "Hello from the UI")
            .expect("send message");

        assert_eq!(result.chat.message_count, 2);
        assert_eq!(result.user_message.role, ChatMessageRole::User);
        assert_eq!(result.assistant_message.role, ChatMessageRole::Assistant);

        let listed = database.list_chats(10).expect("list chats");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, result.chat.id);

        let conversation = database
            .get_chat(&result.chat.id, 200)
            .expect("get conversation");
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(conversation.messages[0].content, "Hello from the UI");

        drop(database);

        let reopened = Database::open(database_path.clone()).expect("reopen database");
        let restored = reopened
            .get_chat(&result.chat.id, 200)
            .expect("restore conversation");
        assert_eq!(restored.messages.len(), 2);

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn retry_resets_failed_assistant_in_place() {
        let database_path = temp_database_path("retry_resets_failed_assistant_in_place");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");

        let run = database
            .begin_chat_run(None, "Retry me")
            .expect("begin run");
        database
            .record_chat_tool_execution_event(
                &run.chat.id,
                &run.assistant_message.id,
                &ToolExecutionEvent {
                    tool_call_id: "tool_retry_1".to_string(),
                    run_id: Some(run.run_id.clone()),
                    project_id: None,
                    command: Some(ToolCommand::new("pwd", std::iter::empty::<&str>())),
                    kind: ToolExecutionEventKind::Queued,
                    stream: None,
                    chunk: None,
                    message: None,
                    result: None,
                },
            )
            .expect("record tool event");
        database
            .fail_chat_run(&run.run_id, &run.chat.id, &run.assistant_message.id, "boom")
            .expect("fail run");

        let retry = database.begin_retry_run(&run.chat.id).expect("begin retry");
        // Same assistant message, rolled back to a fresh pending state.
        assert_eq!(retry.assistant_message.id, run.assistant_message.id);
        assert_eq!(retry.assistant_message.status, ChatMessageStatus::Sending);
        assert!(retry.assistant_message.content.is_empty());
        // The original prompt is reused, not duplicated.
        assert_eq!(retry.user_message.content, "Retry me");
        let conversation = database.get_chat(&run.chat.id, 200).expect("get chat");
        assert_eq!(conversation.messages.len(), 2);
        assert!(conversation.tool_executions.is_empty());

        // Nothing to retry while the run is pending again.
        let error = database
            .begin_retry_run(&run.chat.id)
            .expect_err("no failed run to retry");
        assert!(matches!(error, MothershipError::InvalidRequest(_)));

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn chat_tool_events_are_restored_with_conversation() {
        let database_path = temp_database_path("chat_tool_events_are_restored");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let run = database
            .begin_chat_run(None, "Show the current folder")
            .expect("begin run");
        let command = ToolCommand::new("powershell", ["-Command", "Get-Location"]);

        database
            .record_chat_tool_execution_event(
                &run.chat.id,
                &run.assistant_message.id,
                &ToolExecutionEvent {
                    tool_call_id: "tool_history_1".to_string(),
                    run_id: Some(run.run_id.clone()),
                    project_id: Some("project_1".to_string()),
                    command: Some(command.clone()),
                    kind: ToolExecutionEventKind::Queued,
                    stream: None,
                    chunk: None,
                    message: None,
                    result: None,
                },
            )
            .expect("record queued");
        database
            .record_chat_tool_execution_event(
                &run.chat.id,
                &run.assistant_message.id,
                &ToolExecutionEvent {
                    tool_call_id: "tool_history_1".to_string(),
                    run_id: Some(run.run_id.clone()),
                    project_id: Some("project_1".to_string()),
                    command: None,
                    kind: ToolExecutionEventKind::Output,
                    stream: Some(ToolOutputStream::Stdout),
                    chunk: Some("E:\\Mothership\n".to_string()),
                    message: None,
                    result: None,
                },
            )
            .expect("record output");
        database
            .record_chat_tool_execution_event(
                &run.chat.id,
                &run.assistant_message.id,
                &ToolExecutionEvent {
                    tool_call_id: "tool_history_1".to_string(),
                    run_id: Some(run.run_id.clone()),
                    project_id: Some("project_1".to_string()),
                    command: None,
                    kind: ToolExecutionEventKind::Completed,
                    stream: None,
                    chunk: None,
                    message: None,
                    result: Some(ToolExecutionResult {
                        tool_call_id: "tool_history_1".to_string(),
                        status: ToolExecutionStatus::Completed,
                        exit_code: Some(0),
                        stdout_preview: "E:\\Mothership\n".to_string(),
                        stderr_preview: String::new(),
                        stdout_tail: "E:\\Mothership\n".to_string(),
                        stderr_tail: String::new(),
                        stdout_bytes: 14,
                        stderr_bytes: 0,
                        truncated_for_display: false,
                        truncated_for_agent: false,
                        log_ref: None,
                        message: None,
                    }),
                },
            )
            .expect("record completed");

        drop(database);

        let reopened = Database::open(database_path.clone()).expect("reopen database");
        let conversation = reopened
            .get_chat(&run.chat.id, 200)
            .expect("restore conversation");

        assert_eq!(conversation.tool_executions.len(), 1);
        let tool = &conversation.tool_executions[0];
        assert_eq!(tool.tool_call_id, "tool_history_1");
        assert_eq!(tool.message_id, run.assistant_message.id);
        assert_eq!(tool.command, Some(command));
        assert_eq!(tool.kind, ToolExecutionEventKind::Completed);
        assert_eq!(
            tool.result.as_ref().map(|result| result.exit_code),
            Some(Some(0))
        );
        assert!(tool.output.contains("E:\\Mothership"));

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn editing_user_message_truncates_later_history_and_starts_run() {
        let database_path = temp_database_path("editing_user_message_truncates_history");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");

        let first = database
            .begin_chat_run(None, "Original prompt")
            .expect("begin first run");
        database
            .append_chat_run_delta(
                &first.run_id,
                &first.chat.id,
                &first.assistant_message.id,
                "Original answer",
            )
            .expect("append first answer");
        database
            .complete_chat_run(&first.run_id, &first.chat.id, &first.assistant_message.id)
            .expect("complete first run");

        let second = database
            .begin_chat_run(Some(&first.chat.id), "Follow-up prompt")
            .expect("begin second run");
        database
            .record_chat_tool_execution_event(
                &second.chat.id,
                &second.assistant_message.id,
                &ToolExecutionEvent {
                    tool_call_id: "tool_deleted_after_edit".to_string(),
                    run_id: Some(second.run_id.clone()),
                    project_id: None,
                    command: Some(ToolCommand::new("pwd", std::iter::empty::<&str>())),
                    kind: ToolExecutionEventKind::Queued,
                    stream: None,
                    chunk: None,
                    message: None,
                    result: None,
                },
            )
            .expect("record deleted tool event");
        database
            .append_chat_run_delta(
                &second.run_id,
                &second.chat.id,
                &second.assistant_message.id,
                "Follow-up answer",
            )
            .expect("append second answer");
        database
            .complete_chat_run(
                &second.run_id,
                &second.chat.id,
                &second.assistant_message.id,
            )
            .expect("complete second run");

        let edited = database
            .begin_edited_chat_run(&first.chat.id, &first.user_message.id, "Edited prompt")
            .expect("begin edited run");

        assert_eq!(edited.user_message.id, first.user_message.id);
        assert_eq!(edited.user_message.content, "Edited prompt");
        assert_ne!(edited.assistant_message.id, first.assistant_message.id);
        assert_eq!(edited.assistant_message.status, ChatMessageStatus::Sending);
        assert_eq!(edited.chat.message_count, 2);

        let conversation = database.get_chat(&first.chat.id, 200).expect("get chat");
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(conversation.messages[0].content, "Edited prompt");
        assert_eq!(conversation.messages[1].id, edited.assistant_message.id);
        assert!(conversation.tool_executions.is_empty());

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn branch_chat_copies_history_through_assistant_message() {
        let database_path = temp_database_path("branch_chat_copies_history");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");

        let first = database
            .begin_chat_run(None, "First prompt")
            .expect("begin first run");
        database
            .append_chat_run_delta(
                &first.run_id,
                &first.chat.id,
                &first.assistant_message.id,
                "First answer",
            )
            .expect("append first answer");
        database
            .complete_chat_run(&first.run_id, &first.chat.id, &first.assistant_message.id)
            .expect("complete first run");

        let second = database
            .begin_chat_run(Some(&first.chat.id), "Second prompt")
            .expect("begin second run");
        database
            .record_chat_tool_execution_event(
                &second.chat.id,
                &second.assistant_message.id,
                &ToolExecutionEvent {
                    tool_call_id: "tool_branch_source".to_string(),
                    run_id: Some(second.run_id.clone()),
                    project_id: Some("project_1".to_string()),
                    command: Some(ToolCommand::new("pwd", std::iter::empty::<&str>())),
                    kind: ToolExecutionEventKind::Queued,
                    stream: None,
                    chunk: None,
                    message: None,
                    result: None,
                },
            )
            .expect("record source tool event");
        database
            .append_chat_run_delta(
                &second.run_id,
                &second.chat.id,
                &second.assistant_message.id,
                "Second answer",
            )
            .expect("append second answer");
        database
            .complete_chat_run(
                &second.run_id,
                &second.chat.id,
                &second.assistant_message.id,
            )
            .expect("complete second run");

        let branch = database
            .branch_chat_from_message(&first.chat.id, &second.assistant_message.id)
            .expect("branch chat");

        assert_ne!(branch.chat.id, first.chat.id);
        assert_eq!(branch.chat.message_count, 4);
        assert_eq!(branch.messages.len(), 4);
        assert_eq!(branch.messages[0].content, "First prompt");
        assert_eq!(branch.messages[1].content, "First answer");
        assert_eq!(branch.messages[2].content, "Second prompt");
        assert_eq!(branch.messages[3].content, "Second answer");
        assert_ne!(branch.messages[3].id, second.assistant_message.id);
        assert_eq!(branch.tool_executions.len(), 1);
        assert_ne!(branch.tool_executions[0].tool_call_id, "tool_branch_source");
        assert_eq!(branch.tool_executions[0].message_id, branch.messages[3].id);
        assert_eq!(branch.tool_executions[0].run_id, None);

        let source = database.get_chat(&first.chat.id, 200).expect("get source");
        assert_eq!(source.messages.len(), 4);

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn empty_chat_receives_title_from_first_message() {
        let database_path = temp_database_path("empty_chat_receives_title_from_first_message");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let conversation = database.create_chat().expect("create chat");

        let result = database
            .send_chat_message(Some(&conversation.chat.id), "  Implement Codex auth  ")
            .expect("send message");

        assert_eq!(result.chat.title, "Implement Codex auth");
        assert_eq!(result.chat.message_count, 2);

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn startup_recovery_marks_interrupted_assistant_runs_failed() {
        let database_path =
            temp_database_path("startup_recovery_marks_interrupted_assistant_runs_failed");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let result = database
            .begin_chat_run(None, "Hello")
            .expect("begin chat run");

        let changed = database
            .recover_interrupted_chat_runs()
            .expect("recover interrupted runs");

        assert_eq!(changed, 1);
        let conversation = database
            .get_chat(&result.chat.id, 200)
            .expect("load recovered chat");
        let assistant = conversation
            .messages
            .iter()
            .find(|message| message.id == result.assistant_message.id)
            .expect("assistant message");
        assert_eq!(assistant.status, ChatMessageStatus::Failed);
        assert!(assistant.content.contains("Run interrupted"));

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn selected_model_persists_across_reopen() {
        let database_path = temp_database_path("selected_model_persists");
        let database = Database::open(database_path.clone()).expect("open database");

        let selected = database
            .set_selected_llm_model("some-adapter", "some-model")
            .expect("select model");
        assert_eq!(selected.provider_id, "some-adapter");
        assert_eq!(selected.model_id, "some-model");

        drop(database);

        let reopened = Database::open(database_path.clone()).expect("reopen database");
        let restored = reopened.selected_llm_model().expect("selected model");
        assert_eq!(restored.provider_id, "some-adapter");
        assert_eq!(restored.model_id, "some-model");

        let _ = fs::remove_file(database_path);
    }

    fn temp_database_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();

        std::env::temp_dir().join(format!("mothership_{name}_{unique}.sqlite3"))
    }
}
