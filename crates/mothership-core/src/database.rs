use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, params_from_iter, types::Type, Connection, OptionalExtension};

use mothership_adapter_host::protocol::ReasoningConfig;

use crate::{
    id::generate_id, ActivityEvent, ChatConversation, ChatMessage, ChatMessagePart,
    ChatMessagePartKind, ChatMessageRole, ChatMessageStatus, ChatRunContextSpec, ChatRunEvent,
    ChatRunEventKind, ChatThreadSummary, DashboardMetric, DashboardSnapshot, LlmChatMessage,
    LlmChatRole, MothershipError, ProjectSnapshot, ProjectSummary, Result, SelectedLlmModel,
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
const ACTIVE_PROJECT_SETTING_KEY: &str = "active_project_id";
const CONTINUE_CHAT_MESSAGE_CONTENT: &str = "Continue from where you stopped.";

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

    pub fn list_projects(&self) -> Result<ProjectSnapshot> {
        let connection = self.connect()?;
        project_snapshot(&connection)
    }

    pub fn open_project(&self, path: &str) -> Result<ProjectSnapshot> {
        let project_path = normalize_project_path(path)?;
        let name = project_name_from_path(&project_path);
        let now = current_timestamp();
        let mut connection = self.connect()?;
        let tx = connection.transaction()?;

        let existing_id = tx
            .query_row(
                "SELECT id FROM projects WHERE path = ?1",
                params![project_path],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        let project_id = match existing_id {
            Some(id) => {
                tx.execute(
                    "
                    UPDATE projects
                    SET name = ?2,
                        updated_at = ?3,
                        last_opened_at = ?4
                    WHERE id = ?1
                    ",
                    params![id, name, now, now],
                )?;
                id
            }
            None => {
                let id = generate_id("project")?;
                tx.execute(
                    "
                    INSERT INTO projects (id, name, path, created_at, updated_at, last_opened_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    ",
                    params![id, name, project_path, now, now, now],
                )?;
                id
            }
        };

        set_setting(&tx, ACTIVE_PROJECT_SETTING_KEY, &project_id, &now)?;
        tx.commit()?;

        project_snapshot(&connection)
    }

    pub fn set_active_project(&self, project_id: &str) -> Result<ProjectSnapshot> {
        validate_identifier("project_id", project_id)?;

        let connection = self.connect()?;
        select_project_summary(&connection, project_id)?;
        let now = current_timestamp();
        set_setting(&connection, ACTIVE_PROJECT_SETTING_KEY, project_id, &now)?;

        project_snapshot(&connection)
    }

    pub fn chat_project(&self, chat_id: &str) -> Result<Option<ProjectSummary>> {
        validate_identifier("chat_id", chat_id)?;

        let connection = self.connect()?;
        select_chat_project(&connection, chat_id)
    }

    pub fn chat_provider_state(
        &self,
        chat_id: &str,
        provider_id: &str,
    ) -> Result<Option<serde_json::Value>> {
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("provider_id", provider_id)?;

        let connection = self.connect()?;
        let state = connection
            .query_row(
                "
                SELECT provider_state_provider_id, provider_state_json
                FROM chats
                WHERE id = ?1 AND archived = 0
                ",
                params![chat_id],
                |row| {
                    let stored_provider_id: Option<String> = row.get(0)?;
                    let state_json: Option<String> = row.get(1)?;
                    if stored_provider_id.as_deref() != Some(provider_id) {
                        return Ok(None);
                    }
                    json_value(state_json, 1)
                },
            )
            .optional()?
            .ok_or_else(|| MothershipError::InvalidRequest(format!("chat not found: {chat_id}")))?;
        Ok(state)
    }

    pub fn save_chat_provider_state(
        &self,
        chat_id: &str,
        provider_id: &str,
        state: &serde_json::Value,
    ) -> Result<()> {
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("provider_id", provider_id)?;

        let state_json = serde_json::to_string(state)?;
        let connection = self.connect()?;
        let changed = connection.execute(
            "
            UPDATE chats
            SET provider_state_provider_id = ?2,
                provider_state_json = ?3
            WHERE id = ?1 AND archived = 0
            ",
            params![chat_id, provider_id, state_json],
        )?;
        if changed == 0 {
            return Err(MothershipError::InvalidRequest(format!(
                "chat not found: {chat_id}"
            )));
        }
        Ok(())
    }

    pub fn list_chats(
        &self,
        project_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ChatThreadSummary>> {
        let connection = self.connect()?;
        select_chat_summaries(
            &connection,
            project_id,
            normalize_limit(limit, CHAT_LIST_LIMIT),
        )
    }

    pub fn create_chat(&self, project_id: &str) -> Result<ChatConversation> {
        validate_identifier("project_id", project_id)?;

        let connection = self.connect()?;
        select_project_summary(&connection, project_id)?;
        let now = current_timestamp();
        let chat = ChatThreadSummary {
            id: generate_id("chat")?,
            project_id: Some(project_id.to_string()),
            title: "New chat".to_string(),
            preview: String::new(),
            message_count: 0,
            created_at: now.clone(),
            updated_at: now,
        };

        connection.execute(
            "
            INSERT INTO chats (id, project_id, title, preview, message_count, archived, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)
            ",
            params![
                chat.id,
                chat.project_id.as_deref(),
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
            message_parts: Vec::new(),
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
        let message_parts = select_chat_message_parts(&connection, chat_id, &message_ids)?;

        Ok(ChatConversation {
            chat,
            messages,
            tool_executions,
            message_parts,
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

    pub fn record_chat_tool_call_part(
        &self,
        run_id: &str,
        chat_id: &str,
        message_id: &str,
        tool_call_id: &str,
    ) -> Result<()> {
        validate_identifier("run_id", run_id)?;
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("message_id", message_id)?;
        validate_identifier("tool_call_id", tool_call_id)?;

        let connection = self.connect()?;
        insert_tool_message_part(&connection, run_id, chat_id, message_id, tool_call_id)
    }

    pub fn send_chat_message(
        &self,
        chat_id: Option<&str>,
        project_id: Option<&str>,
        content: &str,
    ) -> Result<SendChatMessageResult> {
        self.begin_chat_run(chat_id, project_id, content, None)
    }

    pub fn recover_interrupted_chat_runs(&self) -> Result<usize> {
        let connection = self.connect()?;
        let interrupted_message =
            "Run interrupted before completion. Send a new message to start another run.";

        let changed = connection.execute(
            "
            UPDATE chat_messages
            SET status = ?1,
                error = ?2
            WHERE role = ?3
              AND status = ?4
            ",
            params![
                chat_status_to_db(ChatMessageStatus::Failed),
                interrupted_message,
                chat_role_to_db(ChatMessageRole::Assistant),
                chat_status_to_db(ChatMessageStatus::Sending),
            ],
        )?;

        Ok(changed)
    }

    pub fn begin_chat_run(
        &self,
        chat_id: Option<&str>,
        project_id: Option<&str>,
        content: &str,
        reasoning: Option<ReasoningConfig>,
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
                let chat = select_chat_summary(&tx, id)?;
                if let Some(project_id) = project_id {
                    validate_identifier("project_id", project_id)?;
                    if chat.project_id.as_deref() != Some(project_id) {
                        return Err(MothershipError::InvalidRequest(
                            "chat does not belong to the selected project".to_string(),
                        ));
                    }
                }
                chat
            }
            None => {
                let project_id = project_id.ok_or_else(|| {
                    MothershipError::InvalidRequest(
                        "select or open a project before starting a chat".to_string(),
                    )
                })?;
                validate_identifier("project_id", project_id)?;
                select_project_summary(&tx, project_id)?;
                let chat = ChatThreadSummary {
                    id: generate_id("chat")?,
                    project_id: Some(project_id.to_string()),
                    title: derive_chat_title(content),
                    preview: String::new(),
                    message_count: 0,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                };
                tx.execute(
                    "
                    INSERT INTO chats (id, project_id, title, preview, message_count, archived, created_at, updated_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)
                    ",
                    params![
                        chat.id,
                        chat.project_id.as_deref(),
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
            error: None,
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
            error: None,
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
            project_id: chat.project_id,
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
            context: ChatRunContextSpec {
                reasoning,
                ..ChatRunContextSpec::default()
            },
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
        clear_chat_provider_state(&tx, chat_id)?;

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
            error: None,
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
            error: None,
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
            project_id: chat.project_id,
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
            context: ChatRunContextSpec::default(),
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
            project_id: source_chat.project_id.clone(),
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

        let tool_call_id_map = copy_chat_tool_events(&tx, chat_id, &chat.id, &message_id_map)?;
        copy_chat_message_parts(&tx, chat_id, &chat.id, &message_id_map, &tool_call_id_map)?;
        let copied_message_ids = copied_messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>();
        let tool_executions = select_chat_tool_executions(&tx, &chat.id, &copied_message_ids)?;
        let message_parts = select_chat_message_parts(&tx, &chat.id, &copied_message_ids)?;
        tx.commit()?;

        Ok(ChatConversation {
            chat,
            messages: copied_messages,
            tool_executions,
            message_parts,
        })
    }

    /// Starts a fresh assistant attempt for the last failed assistant message,
    /// reusing the original user prompt without deleting the failed partial
    /// answer or its tool history.
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
        let user_message =
            select_last_user_message_before_position(&tx, chat_id, assistant_message.position)?
                .ok_or_else(|| {
                    MothershipError::InvalidRequest("no user message to retry".to_string())
                })?;

        let now = current_timestamp();
        let mut next_assistant_message = ChatMessage {
            id: generate_id("chat_message")?,
            chat_id: chat_id.to_string(),
            position: 0,
            role: ChatMessageRole::Assistant,
            content: String::new(),
            status: ChatMessageStatus::Sending,
            created_at: now.clone(),
            error: None,
            provider_id: Some(selected_model.provider_id.clone()),
            model_id: Some(selected_model.model_id.clone()),
        };
        next_assistant_message.position = insert_chat_message(&tx, &next_assistant_message)?;

        let updated_chat = ChatThreadSummary {
            id: chat.id,
            project_id: chat.project_id,
            title: chat.title,
            preview: chat.preview,
            message_count: chat.message_count + 1,
            created_at: chat.created_at,
            updated_at: now,
        };
        update_chat_summary(&tx, &updated_chat)?;
        tx.commit()?;

        Ok(SendChatMessageResult {
            run_id: generate_id("chat_run")?,
            chat: updated_chat,
            user_message,
            assistant_message: next_assistant_message,
            context: ChatRunContextSpec::default(),
        })
    }

    /// Adds an explicit continuation prompt after the last failed assistant
    /// message and starts a new assistant run. The failed assistant content is
    /// included as a one-off context anchor for this run, but remains failed in
    /// history so the UI can still render the error.
    pub fn begin_continue_run(&self, chat_id: &str) -> Result<SendChatMessageResult> {
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
        let failed_assistant_message = select_last_message_by_role(&tx, chat_id, "assistant")?
            .ok_or_else(|| {
                MothershipError::InvalidRequest("no assistant message to continue".to_string())
            })?;
        if failed_assistant_message.status != ChatMessageStatus::Failed {
            return Err(MothershipError::InvalidRequest(
                "the latest run did not fail; nothing to continue".to_string(),
            ));
        }

        let now = current_timestamp();
        let mut user_message = ChatMessage {
            id: generate_id("chat_message")?,
            chat_id: chat_id.to_string(),
            position: 0,
            role: ChatMessageRole::User,
            content: CONTINUE_CHAT_MESSAGE_CONTENT.to_string(),
            status: ChatMessageStatus::Complete,
            created_at: now.clone(),
            error: None,
            provider_id: None,
            model_id: None,
        };
        let mut assistant_message = ChatMessage {
            id: generate_id("chat_message")?,
            chat_id: chat_id.to_string(),
            position: 0,
            role: ChatMessageRole::Assistant,
            content: String::new(),
            status: ChatMessageStatus::Sending,
            created_at: now.clone(),
            error: None,
            provider_id: Some(selected_model.provider_id.clone()),
            model_id: Some(selected_model.model_id.clone()),
        };

        user_message.position = insert_chat_message(&tx, &user_message)?;
        assistant_message.position = insert_chat_message(&tx, &assistant_message)?;

        let updated_chat = ChatThreadSummary {
            id: chat.id,
            project_id: chat.project_id,
            title: chat.title,
            preview: derive_chat_preview(CONTINUE_CHAT_MESSAGE_CONTENT),
            message_count: chat.message_count + 2,
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
            context: ChatRunContextSpec {
                include_failed_assistant_message_id: Some(failed_assistant_message.id),
                reasoning: None,
            },
        })
    }

    pub fn llm_chat_context(
        &self,
        chat_id: &str,
        assistant_message_id: &str,
        limit: i64,
        context: &ChatRunContextSpec,
    ) -> Result<Vec<LlmChatMessage>> {
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("assistant_message_id", assistant_message_id)?;
        if let Some(message_id) = &context.include_failed_assistant_message_id {
            validate_identifier("include_failed_assistant_message_id", message_id)?;
        }

        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "
            SELECT role, content
            FROM (
                SELECT rowid, role, content
                FROM chat_messages
                WHERE chat_id = ?1
                    AND id <> ?2
                    AND (
                        status = 'complete'
                        OR (
                            ?4 IS NOT NULL
                            AND id = ?4
                            AND role = 'assistant'
                            AND status = 'failed'
                            AND trim(content) <> ''
                        )
                    )
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
                normalize_limit(limit, CHAT_MESSAGE_LIMIT),
                context.include_failed_assistant_message_id.as_deref()
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
                tool_call_id: None,
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
        append_text_message_part(&connection, run_id, chat_id, assistant_message_id, delta)?;

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
            tool_call_id: None,
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
            tool_call_id: None,
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
            tool_call_id: None,
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
            tool_call_id: None,
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
                error = ?2
            WHERE id = ?1 AND chat_id = ?3
            ",
            params![assistant_message_id, content, chat_id],
        )?;
        let message = select_chat_message_by_id(&connection, assistant_message_id)?;
        let preview = if message.content.trim().is_empty() {
            content.as_str()
        } else {
            message.content.as_str()
        };
        let chat = update_chat_after_assistant(&connection, chat_id, preview)?;

        Ok(ChatRunEvent {
            run_id: run_id.to_string(),
            chat_id: chat_id.to_string(),
            message_id: assistant_message_id.to_string(),
            kind: ChatRunEventKind::Failed,
            delta: None,
            message: Some(message),
            chat: Some(chat),
            transport: None,
            tool_call_id: None,
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

        CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            path TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_opened_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_projects_last_opened
            ON projects (last_opened_at DESC);

        CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS chats (
            id TEXT PRIMARY KEY,
            project_id TEXT,
            title TEXT NOT NULL,
            preview TEXT NOT NULL,
            message_count INTEGER NOT NULL DEFAULT 0,
            archived INTEGER NOT NULL DEFAULT 0,
            provider_state_provider_id TEXT,
            provider_state_json TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE SET NULL
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
            error TEXT,
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

        CREATE TABLE IF NOT EXISTS chat_message_parts (
            id INTEGER PRIMARY KEY,
            chat_id TEXT NOT NULL,
            message_id TEXT NOT NULL,
            run_id TEXT,
            kind TEXT NOT NULL,
            text TEXT,
            tool_call_id TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY(chat_id) REFERENCES chats(id) ON DELETE CASCADE,
            FOREIGN KEY(message_id) REFERENCES chat_messages(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_chat_message_parts_message
            ON chat_message_parts (message_id, id);

        CREATE UNIQUE INDEX IF NOT EXISTS idx_chat_message_parts_tool
            ON chat_message_parts (message_id, tool_call_id)
            WHERE kind = 'tool' AND tool_call_id IS NOT NULL;

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

    // Fresh databases already have these columns; ALTER brings existing ones
    // up to date (guarded, so re-running is a no-op).
    add_column_if_missing(connection, "chat_messages", "provider_id", "TEXT")?;
    add_column_if_missing(connection, "chat_messages", "model_id", "TEXT")?;
    add_column_if_missing(connection, "chat_messages", "error", "TEXT")?;
    add_column_if_missing(connection, "chats", "project_id", "TEXT")?;
    add_column_if_missing(connection, "chats", "provider_state_provider_id", "TEXT")?;
    add_column_if_missing(connection, "chats", "provider_state_json", "TEXT")?;
    connection.execute(
        "CREATE INDEX IF NOT EXISTS idx_chats_project_updated_at ON chats (project_id, updated_at DESC)",
        [],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![6_i64, current_timestamp()],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![7_i64, current_timestamp()],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![8_i64, current_timestamp()],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![9_i64, current_timestamp()],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![10_i64, current_timestamp()],
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

fn project_snapshot(connection: &Connection) -> Result<ProjectSnapshot> {
    let projects = select_project_summaries(connection)?;
    let active_project_id = get_setting(connection, ACTIVE_PROJECT_SETTING_KEY)?
        .filter(|project_id| projects.iter().any(|project| project.id == *project_id));

    Ok(ProjectSnapshot {
        projects,
        active_project_id,
    })
}

fn select_project_summaries(connection: &Connection) -> Result<Vec<ProjectSummary>> {
    let mut statement = connection.prepare(
        "
        SELECT
            projects.id,
            projects.name,
            projects.path,
            COUNT(chats.id) AS chat_count,
            projects.created_at,
            projects.updated_at,
            projects.last_opened_at
        FROM projects
        LEFT JOIN chats
            ON chats.project_id = projects.id
            AND chats.archived = 0
        GROUP BY projects.id
        ORDER BY projects.last_opened_at DESC, projects.updated_at DESC
        ",
    )?;

    let rows = statement.query_map([], project_summary_from_row)?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn select_project_summary(connection: &Connection, project_id: &str) -> Result<ProjectSummary> {
    connection
        .query_row(
            "
            SELECT
                projects.id,
                projects.name,
                projects.path,
                COUNT(chats.id) AS chat_count,
                projects.created_at,
                projects.updated_at,
                projects.last_opened_at
            FROM projects
            LEFT JOIN chats
                ON chats.project_id = projects.id
                AND chats.archived = 0
            WHERE projects.id = ?1
            GROUP BY projects.id
            ",
            params![project_id],
            project_summary_from_row,
        )
        .optional()?
        .ok_or_else(|| MothershipError::InvalidRequest(format!("project not found: {project_id}")))
}

fn select_chat_project(connection: &Connection, chat_id: &str) -> Result<Option<ProjectSummary>> {
    let project_id = connection
        .query_row(
            "SELECT project_id FROM chats WHERE id = ?1 AND archived = 0",
            params![chat_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .ok_or_else(|| MothershipError::InvalidRequest(format!("chat not found: {chat_id}")))?;

    match project_id {
        Some(project_id) => select_project_summary(connection, &project_id).map(Some),
        None => Ok(None),
    }
}

fn get_setting(connection: &Connection, key: &str) -> Result<Option<String>> {
    connection
        .query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

fn set_setting(connection: &Connection, key: &str, value: &str, updated_at: &str) -> Result<()> {
    connection.execute(
        "
        INSERT INTO app_settings (key, value, updated_at)
        VALUES (?1, ?2, ?3)
        ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at = excluded.updated_at
        ",
        params![key, value, updated_at],
    )?;
    Ok(())
}

fn clear_chat_provider_state(connection: &Connection, chat_id: &str) -> Result<()> {
    connection.execute(
        "
        UPDATE chats
        SET provider_state_provider_id = NULL,
            provider_state_json = NULL
        WHERE id = ?1
        ",
        params![chat_id],
    )?;
    Ok(())
}

fn select_chat_summaries(
    connection: &Connection,
    project_id: Option<&str>,
    limit: i64,
) -> Result<Vec<ChatThreadSummary>> {
    if let Some(project_id) = project_id {
        validate_identifier("project_id", project_id)?;
        let mut statement = connection.prepare(
            "
            SELECT id, project_id, title, preview, message_count, created_at, updated_at
            FROM chats
            WHERE archived = 0 AND project_id = ?1
            ORDER BY updated_at DESC, rowid DESC
            LIMIT ?2
            ",
        )?;

        let rows = statement.query_map(params![project_id, limit], chat_summary_from_row)?;

        return rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into);
    }

    let mut statement = connection.prepare(
        "
        SELECT id, project_id, title, preview, message_count, created_at, updated_at
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
            SELECT id, project_id, title, preview, message_count, created_at, updated_at
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
        INSERT INTO chats (id, project_id, title, preview, message_count, archived, created_at, updated_at)
        VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)
        ",
        params![
            chat.id,
            chat.project_id.as_deref(),
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
        SELECT id, chat_id, rowid, role, content, status, created_at, error, provider_id, model_id
        FROM (
            SELECT rowid, id, chat_id, role, content, status, created_at, error, provider_id, model_id
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
        SELECT id, chat_id, rowid, role, content, status, created_at, error, provider_id, model_id
        FROM chat_messages
        WHERE chat_id = ?1 AND rowid <= ?2
        ORDER BY rowid ASC
        ",
    )?;

    let rows = statement.query_map(params![chat_id, through_position], chat_message_from_row)?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn select_chat_message_parts(
    connection: &Connection,
    chat_id: &str,
    message_ids: &[&str],
) -> Result<Vec<ChatMessagePart>> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat("?")
        .take(message_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "
        SELECT id, chat_id, message_id, kind, text, tool_call_id, created_at
        FROM chat_message_parts
        WHERE chat_id = ?
          AND message_id IN ({placeholders})
        ORDER BY message_id ASC, id ASC
        "
    );
    let mut statement = connection.prepare(&sql)?;
    let params = std::iter::once(chat_id)
        .chain(message_ids.iter().copied())
        .collect::<Vec<_>>();
    let rows = statement.query_map(params_from_iter(params), chat_message_part_from_row)?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn append_text_message_part(
    connection: &Connection,
    run_id: &str,
    chat_id: &str,
    message_id: &str,
    delta: &str,
) -> Result<()> {
    if delta.is_empty() {
        return Ok(());
    }

    let latest = connection
        .query_row(
            "
            SELECT id, kind
            FROM chat_message_parts
            WHERE message_id = ?1
            ORDER BY id DESC
            LIMIT 1
            ",
            params![message_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;

    if let Some((part_id, kind)) = latest {
        if kind == chat_message_part_kind_to_db(ChatMessagePartKind::Text) {
            connection.execute(
                "
                UPDATE chat_message_parts
                SET text = COALESCE(text, '') || ?2
                WHERE id = ?1
                ",
                params![part_id, delta],
            )?;
            return Ok(());
        }
    }

    connection.execute(
        "
        INSERT INTO chat_message_parts (
            chat_id, message_id, run_id, kind, text, tool_call_id, created_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)
        ",
        params![
            chat_id,
            message_id,
            run_id,
            chat_message_part_kind_to_db(ChatMessagePartKind::Text),
            delta,
            current_timestamp()
        ],
    )?;
    Ok(())
}

fn insert_tool_message_part(
    connection: &Connection,
    run_id: &str,
    chat_id: &str,
    message_id: &str,
    tool_call_id: &str,
) -> Result<()> {
    connection.execute(
        "
        INSERT OR IGNORE INTO chat_message_parts (
            chat_id, message_id, run_id, kind, text, tool_call_id, created_at
        )
        VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)
        ",
        params![
            chat_id,
            message_id,
            run_id,
            chat_message_part_kind_to_db(ChatMessagePartKind::Tool),
            tool_call_id,
            current_timestamp()
        ],
    )?;
    Ok(())
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
) -> Result<HashMap<String, String>> {
    if message_id_map.is_empty() {
        return Ok(HashMap::new());
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

    Ok(tool_call_id_map)
}

fn copy_chat_message_parts(
    connection: &Connection,
    source_chat_id: &str,
    target_chat_id: &str,
    message_id_map: &HashMap<String, String>,
    tool_call_id_map: &HashMap<String, String>,
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
        SELECT message_id, kind, text, tool_call_id, created_at
        FROM chat_message_parts
        WHERE chat_id = ?
          AND message_id IN ({placeholders})
        ORDER BY id ASC
        "
    );

    struct PartCopyRow {
        message_id: String,
        kind: String,
        text: Option<String>,
        tool_call_id: Option<String>,
        created_at: String,
    }

    let rows = {
        let mut statement = connection.prepare(&sql)?;
        let params = std::iter::once(source_chat_id)
            .chain(source_message_ids.iter().copied())
            .collect::<Vec<_>>();
        let rows = statement.query_map(params_from_iter(params), |row| {
            Ok(PartCopyRow {
                message_id: row.get(0)?,
                kind: row.get(1)?,
                text: row.get(2)?,
                tool_call_id: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    for row in rows {
        let Some(target_message_id) = message_id_map.get(&row.message_id) else {
            continue;
        };
        let target_tool_call_id = row
            .tool_call_id
            .as_ref()
            .and_then(|id| tool_call_id_map.get(id))
            .cloned();
        if row.kind == chat_message_part_kind_to_db(ChatMessagePartKind::Tool)
            && target_tool_call_id.is_none()
        {
            continue;
        }

        connection.execute(
            "
            INSERT INTO chat_message_parts (
                chat_id, message_id, run_id, kind, text, tool_call_id, created_at
            )
            VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6)
            ",
            params![
                target_chat_id,
                target_message_id,
                row.kind,
                row.text,
                target_tool_call_id,
                row.created_at
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
        INSERT INTO chat_messages (id, chat_id, role, content, status, created_at, error, provider_id, model_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ",
        params![
            message.id,
            message.chat_id,
            chat_role_to_db(message.role),
            message.content,
            chat_status_to_db(message.status),
            message.created_at,
            message.error,
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
            SELECT id, chat_id, rowid, role, content, status, created_at, error, provider_id, model_id
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
            SELECT id, chat_id, rowid, role, content, status, created_at, error, provider_id, model_id
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

fn select_last_user_message_before_position(
    connection: &Connection,
    chat_id: &str,
    position: i64,
) -> Result<Option<ChatMessage>> {
    connection
        .query_row(
            "
            SELECT id, chat_id, rowid, role, content, status, created_at, error, provider_id, model_id
            FROM chat_messages
            WHERE chat_id = ?1
              AND role = 'user'
              AND rowid < ?2
            ORDER BY rowid DESC
            LIMIT 1
            ",
            params![chat_id, position],
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
        project_id: row.get(1)?,
        title: row.get(2)?,
        preview: row.get(3)?,
        message_count: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn project_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectSummary> {
    Ok(ProjectSummary {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        chat_count: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        last_opened_at: row.get(6)?,
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
        error: row.get(7)?,
        provider_id: row.get(8)?,
        model_id: row.get(9)?,
    })
}

fn chat_message_part_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatMessagePart> {
    let kind_value: String = row.get(3)?;
    Ok(ChatMessagePart {
        id: row.get(0)?,
        chat_id: row.get(1)?,
        message_id: row.get(2)?,
        kind: chat_message_part_kind_from_db(&kind_value, 3)?,
        text: row.get(4)?,
        tool_call_id: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn chat_message_part_kind_to_db(kind: ChatMessagePartKind) -> &'static str {
    match kind {
        ChatMessagePartKind::Text => "text",
        ChatMessagePartKind::Tool => "tool",
    }
}

fn chat_message_part_kind_from_db(
    value: &str,
    column: usize,
) -> rusqlite::Result<ChatMessagePartKind> {
    match value {
        "text" => Ok(ChatMessagePartKind::Text),
        "tool" => Ok(ChatMessagePartKind::Tool),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Text,
            Box::new(MothershipError::InvalidRequest(format!(
                "unsupported chat message part kind: {value}"
            ))),
        )),
    }
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
        ToolExecutionEventKind::LoopBlocked => "loop_blocked",
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
        "loop_blocked" => Ok(ToolExecutionEventKind::LoopBlocked),
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

fn normalize_project_path(path: &str) -> Result<String> {
    let path = path.trim();
    if path.is_empty() {
        return Err(MothershipError::InvalidRequest(
            "project path cannot be empty".to_string(),
        ));
    }

    let canonical = fs::canonicalize(path).map_err(|error| {
        MothershipError::InvalidRequest(format!("project path is not available: {error}"))
    })?;
    if !canonical.is_dir() {
        return Err(MothershipError::InvalidRequest(
            "project path must be a directory".to_string(),
        ));
    }

    Ok(display_project_path(&canonical))
}

fn project_name_from_path(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(path)
        .to_string()
}

fn display_project_path(path: &Path) -> String {
    let raw = path.to_string_lossy();

    #[cfg(windows)]
    {
        if let Some(stripped) = raw.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{stripped}");
        }
        if let Some(stripped) = raw.strip_prefix(r"\\?\") {
            return stripped.to_string();
        }
    }

    raw.to_string()
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
        path::{Path, PathBuf},
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
        let project = create_project(&database, &database_path, "persistent_conversation");

        let result = database
            .send_chat_message(None, Some(&project.id), "Hello from the UI")
            .expect("send message");

        assert_eq!(result.chat.message_count, 2);
        assert_eq!(result.chat.project_id.as_deref(), Some(project.id.as_str()));
        assert_eq!(result.user_message.role, ChatMessageRole::User);
        assert_eq!(result.assistant_message.role, ChatMessageRole::Assistant);

        let listed = database
            .list_chats(Some(&project.id), 10)
            .expect("list chats");
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
    fn retry_keeps_failed_assistant_and_starts_new_attempt() {
        let database_path =
            temp_database_path("retry_keeps_failed_assistant_and_starts_new_attempt");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let project = create_project(&database, &database_path, "retry");

        let run = database
            .begin_chat_run(None, Some(&project.id), "Retry me", None)
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
            .append_chat_run_delta(
                &run.run_id,
                &run.chat.id,
                &run.assistant_message.id,
                "Partial answer",
            )
            .expect("append partial answer");
        database
            .fail_chat_run(&run.run_id, &run.chat.id, &run.assistant_message.id, "boom")
            .expect("fail run");

        let retry = database.begin_retry_run(&run.chat.id).expect("begin retry");
        assert_ne!(retry.assistant_message.id, run.assistant_message.id);
        assert_eq!(retry.assistant_message.status, ChatMessageStatus::Sending);
        assert!(retry.assistant_message.content.is_empty());
        // The original prompt is reused, not duplicated.
        assert_eq!(retry.user_message.content, "Retry me");
        let conversation = database.get_chat(&run.chat.id, 200).expect("get chat");
        assert_eq!(conversation.messages.len(), 3);
        let failed = conversation
            .messages
            .iter()
            .find(|message| message.id == run.assistant_message.id)
            .expect("failed assistant message");
        assert_eq!(failed.status, ChatMessageStatus::Failed);
        assert_eq!(failed.content, "Partial answer");
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert_eq!(conversation.tool_executions.len(), 1);

        // Nothing to retry while the run is pending again.
        let error = database
            .begin_retry_run(&run.chat.id)
            .expect_err("no failed run to retry");
        assert!(matches!(error, MothershipError::InvalidRequest(_)));

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn continue_run_includes_failed_assistant_as_context_anchor() {
        let database_path =
            temp_database_path("continue_run_includes_failed_assistant_as_context_anchor");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let project = create_project(&database, &database_path, "continue");

        let run = database
            .begin_chat_run(None, Some(&project.id), "Continue me", None)
            .expect("begin run");
        database
            .append_chat_run_delta(
                &run.run_id,
                &run.chat.id,
                &run.assistant_message.id,
                "Partial work",
            )
            .expect("append partial work");
        database
            .fail_chat_run(&run.run_id, &run.chat.id, &run.assistant_message.id, "boom")
            .expect("fail run");

        let continued = database
            .begin_continue_run(&run.chat.id)
            .expect("begin continue run");

        assert_eq!(
            continued.user_message.content,
            CONTINUE_CHAT_MESSAGE_CONTENT
        );
        assert_eq!(
            continued
                .context
                .include_failed_assistant_message_id
                .as_deref(),
            Some(run.assistant_message.id.as_str())
        );

        let context = database
            .llm_chat_context(
                &continued.chat.id,
                &continued.assistant_message.id,
                20,
                &continued.context,
            )
            .expect("load LLM context");
        let context_contents = context
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            context_contents,
            vec!["Continue me", "Partial work", CONTINUE_CHAT_MESSAGE_CONTENT]
        );

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn chat_tool_events_are_restored_with_conversation() {
        let database_path = temp_database_path("chat_tool_events_are_restored");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let project = create_project(&database, &database_path, "tool_history");
        let run = database
            .begin_chat_run(None, Some(&project.id), "Show the current folder", None)
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
    fn chat_message_parts_preserve_text_tool_text_order() {
        let database_path = temp_database_path("chat_message_parts_preserve_order");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let project = create_project(&database, &database_path, "message_parts");

        let run = database
            .begin_chat_run(None, Some(&project.id), "Inspect the workspace", None)
            .expect("begin run");
        database
            .append_chat_run_delta(
                &run.run_id,
                &run.chat.id,
                &run.assistant_message.id,
                "I'll inspect the files first.\n",
            )
            .expect("append first text");
        database
            .record_chat_tool_call_part(
                &run.run_id,
                &run.chat.id,
                &run.assistant_message.id,
                "tool_order_1",
            )
            .expect("record tool part");
        database
            .record_chat_tool_execution_event(
                &run.chat.id,
                &run.assistant_message.id,
                &ToolExecutionEvent {
                    tool_call_id: "tool_order_1".to_string(),
                    run_id: Some(run.run_id.clone()),
                    project_id: Some("project_1".to_string()),
                    command: Some(ToolCommand::new(
                        "powershell",
                        ["-Command", "Get-ChildItem"],
                    )),
                    kind: ToolExecutionEventKind::Queued,
                    stream: None,
                    chunk: None,
                    message: None,
                    result: None,
                },
            )
            .expect("record tool event");
        database
            .append_chat_run_delta(
                &run.run_id,
                &run.chat.id,
                &run.assistant_message.id,
                "Done, here is what changed.",
            )
            .expect("append second text");

        drop(database);

        let reopened = Database::open(database_path.clone()).expect("reopen database");
        let conversation = reopened
            .get_chat(&run.chat.id, 200)
            .expect("restore conversation");

        assert_eq!(conversation.message_parts.len(), 3);
        assert_eq!(
            conversation.message_parts[0].kind,
            ChatMessagePartKind::Text
        );
        assert_eq!(
            conversation.message_parts[0].text.as_deref(),
            Some("I'll inspect the files first.\n")
        );
        assert_eq!(
            conversation.message_parts[1].kind,
            ChatMessagePartKind::Tool
        );
        assert_eq!(
            conversation.message_parts[1].tool_call_id.as_deref(),
            Some("tool_order_1")
        );
        assert_eq!(
            conversation.message_parts[2].kind,
            ChatMessagePartKind::Text
        );
        assert_eq!(
            conversation.message_parts[2].text.as_deref(),
            Some("Done, here is what changed.")
        );

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn editing_user_message_truncates_later_history_and_starts_run() {
        let database_path = temp_database_path("editing_user_message_truncates_history");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let project = create_project(&database, &database_path, "editing");

        let first = database
            .begin_chat_run(None, Some(&project.id), "Original prompt", None)
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
            .begin_chat_run(
                Some(&first.chat.id),
                Some(&project.id),
                "Follow-up prompt",
                None,
            )
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
        let project = create_project(&database, &database_path, "branch");

        let first = database
            .begin_chat_run(None, Some(&project.id), "First prompt", None)
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
            .begin_chat_run(
                Some(&first.chat.id),
                Some(&project.id),
                "Second prompt",
                None,
            )
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
        let project = create_project(&database, &database_path, "empty_chat");
        let conversation = database.create_chat(&project.id).expect("create chat");

        let result = database
            .send_chat_message(
                Some(&conversation.chat.id),
                Some(&project.id),
                "  Implement Codex auth  ",
            )
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
        let project = create_project(&database, &database_path, "startup_recovery");
        let result = database
            .begin_chat_run(None, Some(&project.id), "Hello", None)
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
        assert!(assistant.content.is_empty());
        assert!(assistant
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("Run interrupted"));

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn project_snapshot_tracks_active_project_and_project_chats() {
        let database_path = temp_database_path("project_snapshot_tracks_chats");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");

        let first_project = create_project(&database, &database_path, "first");
        let second_project = create_project(&database, &database_path, "second");

        database
            .begin_chat_run(None, Some(&first_project.id), "First project prompt", None)
            .expect("begin first project run");
        database
            .begin_chat_run(
                None,
                Some(&second_project.id),
                "Second project prompt",
                None,
            )
            .expect("begin second project run");

        let first_chats = database
            .list_chats(Some(&first_project.id), 10)
            .expect("list first project chats");
        let second_chats = database
            .list_chats(Some(&second_project.id), 10)
            .expect("list second project chats");

        assert_eq!(first_chats.len(), 1);
        assert_eq!(second_chats.len(), 1);
        assert_eq!(
            first_chats[0].project_id.as_deref(),
            Some(first_project.id.as_str())
        );
        assert_eq!(
            second_chats[0].project_id.as_deref(),
            Some(second_project.id.as_str())
        );

        let snapshot = database
            .set_active_project(&first_project.id)
            .expect("select first project");
        assert_eq!(
            snapshot.active_project_id.as_deref(),
            Some(first_project.id.as_str())
        );
        assert_eq!(
            snapshot
                .projects
                .iter()
                .find(|project| project.id == first_project.id)
                .map(|project| project.chat_count),
            Some(1)
        );

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

    fn create_project(database: &Database, database_path: &Path, name: &str) -> ProjectSummary {
        let project_path = database_path.with_file_name(format!(
            "{}_{name}_project",
            database_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("mothership")
        ));
        fs::create_dir_all(&project_path).expect("create project dir");
        let snapshot = database
            .open_project(project_path.to_str().expect("project path"))
            .expect("open project");
        let active_project_id = snapshot.active_project_id.expect("active project id");
        snapshot
            .projects
            .into_iter()
            .find(|project| project.id == active_project_id)
            .expect("active project")
    }
}
