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
    ChatRunEventKind, ChatThreadSummary, DashboardMetric, DashboardSnapshot, FeatureRoute,
    LlmChatMessage, LlmChatRole, ModelInstruction, MothershipError, PersonalizationSettings,
    ProjectSnapshot, ProjectSummary, ProviderInstruction, Result, SelectedLlmModel,
    SendChatMessageResult, SidecarStatus, ToolArtifact, ToolCommand, ToolExecutionEvent,
    ToolExecutionEventKind, ToolExecutionRecord, ToolExecutionResult, ToolKind, ToolOutputStream,
    ToolPolicySettings, WorkspaceItem,
};

const WORKSPACE_LIMIT: i64 = 2_500;
const EVENT_LIMIT: i64 = 5_000;
const CHAT_LIST_LIMIT: i64 = 100;
const CHAT_MESSAGE_LIMIT: i64 = 200;
const CHAT_MESSAGE_MAX_BYTES: usize = 20_000;
const TOOL_OUTPUT_DISPLAY_MAX_BYTES: usize = 12_000;
const DEFAULT_MODEL_SCOPE: &str = "default";
const ACTIVE_PROJECT_SETTING_KEY: &str = "active_project_id";
const PERSONALIZATION_GLOBAL_KEY: &str = "personalization.global";
const PERSONALIZATION_PROVIDER_PREFIX: &str = "personalization.provider.";
const PERSONALIZATION_MODEL_PREFIX: &str = "personalization.model.";
const PROVIDER_DISABLED_PREFIX: &str = "provider.disabled.";
const PERMISSIONS_COMMAND_ALLOW_KEY: &str = "permissions.command.allow";
const PERMISSIONS_COMMAND_DENY_KEY: &str = "permissions.command.deny";
const PERMISSIONS_DISABLED_TOOLS_KEY: &str = "permissions.tools.disabled";
const CHANGE_JOURNAL_RETENTION_KEY: &str = "change_journal_retention";
const CHANGE_JOURNAL_RETENTION_DEFAULT: u32 = 10;
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
            migrate(&mut connection)?;
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

    /// The on-disk root of a project, by id. Used to resolve + contain a path
    /// before opening it externally. `None` when the project is unknown.
    pub fn project_root(&self, project_id: &str) -> Result<Option<String>> {
        validate_identifier("project_id", project_id)?;

        let connection = self.connect()?;
        let path = connection
            .query_row(
                "SELECT path FROM projects WHERE id = ?1",
                params![project_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(path)
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

    /// Creates a new chat in `project_id`. When `copy_from_chat_id` is given, the
    /// new chat inherits that chat's settings — execution model, approval mode,
    /// and reasoning — so "New chat" carries over the current configuration. The
    /// draft is intentionally NOT copied (a fresh chat starts with empty input).
    pub fn create_chat(
        &self,
        project_id: &str,
        copy_from_chat_id: Option<&str>,
    ) -> Result<ChatConversation> {
        validate_identifier("project_id", project_id)?;

        let connection = self.connect()?;
        select_project_summary(&connection, project_id)?;
        let source = match copy_from_chat_id {
            Some(id) => select_chat_summary(&connection, id).ok(),
            None => None,
        };
        let now = current_timestamp();
        let chat = ChatThreadSummary {
            id: generate_id("chat")?,
            project_id: Some(project_id.to_string()),
            title: "New chat".to_string(),
            preview: String::new(),
            message_count: 0,
            provider_id: source.as_ref().and_then(|chat| chat.provider_id.clone()),
            model_id: source.as_ref().and_then(|chat| chat.model_id.clone()),
            approval_mode: source.as_ref().and_then(|chat| chat.approval_mode.clone()),
            reasoning: source.as_ref().and_then(|chat| chat.reasoning.clone()),
            fast_mode: source.as_ref().and_then(|chat| chat.fast_mode),
            draft: None,
            created_at: now.clone(),
            updated_at: now,
        };

        insert_chat_summary(&connection, &chat)?;

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

    /// Persist a tool event into the typed storage (the source of truth), in
    /// addition to the legacy `chat_tool_events` feed. Upserts the `tool_calls`
    /// row, records a lifecycle `tool_events` row, and stores any artifacts.
    ///
    /// For `run_command`, the typed payload + output artifact are synthesized
    /// from the event's `command` + `result` (so the supervisor stays untouched);
    /// the typed file/search tools supply `payload` + `artifacts` directly.
    /// Streaming `Output` events are skipped here — the legacy feed keeps the
    /// full chunk stream — so only lifecycle transitions are recorded. Large
    /// content (full diffs/output/results) is referenced via artifact `log_ref`,
    /// never inlined into these rows.
    pub fn record_typed_tool_event(
        &self,
        chat_id: &str,
        message_id: &str,
        event: &ToolExecutionEvent,
    ) -> Result<()> {
        // Only events carrying a typed kind drive typed storage; streaming output
        // chunks are left to the legacy feed.
        let Some(kind) = event.tool_kind else {
            return Ok(());
        };
        if event.kind == ToolExecutionEventKind::Output {
            return Ok(());
        }
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("message_id", message_id)?;
        validate_identifier("tool_call_id", &event.tool_call_id)?;

        let (payload, mut artifacts) = typed_payload_and_artifacts(event);
        artifacts.extend(event.artifacts.iter().cloned());

        let touched_paths_json = if event.touched_paths.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&event.touched_paths)?)
        };
        let payload_json = match &payload {
            Some(value) => Some(serde_json::to_string(value)?),
            None => None,
        };
        let artifact_refs = if artifacts.is_empty() {
            None
        } else {
            Some(serde_json::to_string(
                &artifacts
                    .iter()
                    .map(|artifact| artifact.artifact_id.as_str())
                    .collect::<Vec<_>>(),
            )?)
        };
        let status = tool_call_status_for_event(event.kind);
        let permission_state = permission_state_for_event(event.kind);
        let summary = event.message.as_deref().map(truncate_summary);
        let now = current_timestamp();
        let completed_at = is_terminal_event(event.kind).then(|| now.clone());

        let mut connection = self.connect()?;
        let tx = connection.transaction()?;

        // Upsert the call row: create on first sight, then refine in place.
        tx.execute(
            "INSERT OR IGNORE INTO tool_calls (
                tool_call_id, chat_id, message_id, run_id, project_id,
                tool_name, tool_kind, status, permission_state, started_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', 'auto', ?8)",
            params![
                event.tool_call_id.as_str(),
                chat_id,
                message_id,
                event.run_id.as_deref(),
                event.project_id.as_deref(),
                kind.as_str(),
                kind.as_str(),
                now,
            ],
        )?;
        tx.execute(
            "UPDATE tool_calls SET
                status = ?2,
                permission_state = COALESCE(?3, permission_state),
                summary = COALESCE(?4, summary),
                touched_paths = COALESCE(?5, touched_paths),
                payload_json = COALESCE(?6, payload_json),
                completed_at = COALESCE(?7, completed_at)
             WHERE tool_call_id = ?1",
            params![
                event.tool_call_id.as_str(),
                status,
                permission_state,
                summary,
                touched_paths_json,
                payload_json,
                completed_at,
            ],
        )?;

        tx.execute(
            "INSERT INTO tool_events (
                tool_call_id, kind, message_preview, typed_payload_json, artifact_refs, occurred_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.tool_call_id.as_str(),
                tool_event_kind_to_db(event.kind),
                summary,
                payload_json,
                artifact_refs,
                now,
            ],
        )?;

        for artifact in &artifacts {
            tx.execute(
                "INSERT OR REPLACE INTO tool_artifacts (
                    artifact_id, tool_call_id, artifact_kind, content_type,
                    preview, log_ref, size_bytes, sha256, truncated, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    artifact.artifact_id.as_str(),
                    event.tool_call_id.as_str(),
                    artifact.kind.as_str(),
                    artifact.content_type.as_str(),
                    artifact.preview.as_str(),
                    artifact.log_ref.as_deref(),
                    artifact.size_bytes as i64,
                    artifact.sha256.as_deref(),
                    artifact.truncated as i64,
                    now,
                ],
            )?;
        }

        tx.commit()?;
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
        self.begin_chat_run(chat_id, project_id, content, None, false)
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
        fast_mode: bool,
    ) -> Result<SendChatMessageResult> {
        let content = validate_chat_message_content(content)?;
        let mut connection = self.connect()?;
        let selected_model = selected_llm_model(&connection)?;

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
                    provider_id: None,
                    model_id: None,
                    approval_mode: None,
                    reasoning: None,
                    fast_mode: None,
                    draft: None,
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

        // Resolve + freeze this chat's execution model (locking in the global
        // default if the chat — including a brand-new one — has none yet); the
        // assistant placeholder + run are attributed to it, immune to a later
        // model switch.
        let selected_model = chat_run_model(&chat, &selected_model);
        if selected_model.model_id.trim().is_empty() {
            return Err(MothershipError::InvalidRequest(
                "no LLM model selected; connect a provider and choose a model first".to_string(),
            ));
        }
        persist_chat_model(
            &tx,
            &chat.id,
            &selected_model.provider_id,
            &selected_model.model_id,
        )?;

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
            provider_id: Some(selected_model.provider_id.clone()),
            model_id: Some(selected_model.model_id.clone()),
            approval_mode: chat.approval_mode,
            reasoning: chat.reasoning,
            fast_mode: fast_mode.then_some(true),
            // The composer draft is consumed by this send — cleared here and in
            // the UPDATE below, so it doesn't reappear when the chat is reopened.
            draft: None,
            created_at: chat.created_at,
            updated_at: now,
        };

        tx.execute(
            "
            UPDATE chats
            SET title = ?2,
                preview = ?3,
                message_count = ?4,
                fast_mode = ?5,
                draft = NULL,
                updated_at = ?6
            WHERE id = ?1
            ",
            params![
                updated_chat.id,
                updated_chat.title,
                updated_chat.preview,
                updated_chat.message_count,
                updated_chat.fast_mode,
                updated_chat.updated_at
            ],
        )?;

        tx.commit()?;

        Ok(SendChatMessageResult {
            run_id: generate_id("chat_run")?,
            chat: updated_chat,
            user_message,
            assistant_message,
            removed_message_ids: Vec::new(),
            context: ChatRunContextSpec {
                reasoning,
                fast_mode,
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

        let tx = connection.transaction()?;
        let chat = select_chat_summary(&tx, chat_id)?;
        // Resolve + freeze this chat's execution model (locking in the global
        // default if the chat has none yet); the assistant placeholder + run are
        // then attributed to it, immune to a later model switch.
        let selected_model = chat_run_model(&chat, &selected_model);
        if selected_model.model_id.trim().is_empty() {
            return Err(MothershipError::InvalidRequest(
                "no LLM model selected; connect a provider and choose a model first".to_string(),
            ));
        }
        persist_chat_model(
            &tx,
            &chat.id,
            &selected_model.provider_id,
            &selected_model.model_id,
        )?;
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
        let removed_message_ids =
            select_chat_message_ids_after_position(&tx, chat_id, original_user_message.position)?;
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
            provider_id: Some(selected_model.provider_id.clone()),
            model_id: Some(selected_model.model_id.clone()),
            approval_mode: chat.approval_mode,
            reasoning: chat.reasoning,
            fast_mode: chat.fast_mode,
            draft: chat.draft,
            created_at: chat.created_at,
            updated_at: now,
        };
        update_chat_summary(&tx, &updated_chat)?;
        tx.commit()?;
        let run_fast_mode = updated_chat.fast_mode.unwrap_or(false);

        Ok(SendChatMessageResult {
            run_id: generate_id("chat_run")?,
            chat: updated_chat,
            user_message,
            assistant_message,
            removed_message_ids,
            context: ChatRunContextSpec {
                fast_mode: run_fast_mode,
                ..ChatRunContextSpec::default()
            },
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
            // Branch chats inherit the source chat's execution model + tool/
            // reasoning settings for their future runs (message attribution stays
            // historical). The draft is not carried — a branch starts empty.
            provider_id: source_chat.provider_id.clone(),
            model_id: source_chat.model_id.clone(),
            approval_mode: source_chat.approval_mode.clone(),
            reasoning: source_chat.reasoning.clone(),
            fast_mode: source_chat.fast_mode,
            draft: None,
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
        copy_typed_tool_data(&tx, chat_id, &chat.id, &message_id_map, &tool_call_id_map)?;
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

    /// Rolls the chat back to the user message that produced the latest failed
    /// assistant run, then starts a fresh assistant attempt for that same user
    /// message. Unlike "continue", retry removes the failed tail so the next run
    /// behaves as if the user message was just sent again.
    pub fn begin_retry_run(&self, chat_id: &str) -> Result<SendChatMessageResult> {
        validate_identifier("chat_id", chat_id)?;
        let mut connection = self.connect()?;
        let selected_model = selected_llm_model(&connection)?;

        let tx = connection.transaction()?;
        let chat = select_chat_summary(&tx, chat_id)?;
        // Resolve + freeze this chat's execution model (locking in the global
        // default if the chat has none yet); the assistant placeholder + run are
        // then attributed to it, immune to a later model switch.
        let selected_model = chat_run_model(&chat, &selected_model);
        if selected_model.model_id.trim().is_empty() {
            return Err(MothershipError::InvalidRequest(
                "no LLM model selected; connect a provider and choose a model first".to_string(),
            ));
        }
        persist_chat_model(
            &tx,
            &chat.id,
            &selected_model.provider_id,
            &selected_model.model_id,
        )?;

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

        let removed_message_ids =
            select_chat_message_ids_after_position(&tx, chat_id, user_message.position)?;
        tx.execute(
            "DELETE FROM chat_messages WHERE chat_id = ?1 AND rowid > ?2",
            params![chat_id, user_message.position],
        )?;
        clear_chat_provider_state(&tx, chat_id)?;

        let now = current_timestamp();
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
        let updated_chat = ChatThreadSummary {
            id: chat.id,
            project_id: chat.project_id,
            title: chat.title,
            preview: derive_chat_preview(&user_message.content),
            message_count,
            provider_id: Some(selected_model.provider_id.clone()),
            model_id: Some(selected_model.model_id.clone()),
            approval_mode: chat.approval_mode,
            reasoning: chat.reasoning,
            fast_mode: chat.fast_mode,
            draft: chat.draft,
            created_at: chat.created_at,
            updated_at: now,
        };
        update_chat_summary(&tx, &updated_chat)?;
        tx.commit()?;
        let run_fast_mode = updated_chat.fast_mode.unwrap_or(false);

        Ok(SendChatMessageResult {
            run_id: generate_id("chat_run")?,
            chat: updated_chat,
            user_message,
            assistant_message,
            removed_message_ids,
            context: ChatRunContextSpec {
                fast_mode: run_fast_mode,
                ..ChatRunContextSpec::default()
            },
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

        let tx = connection.transaction()?;
        let chat = select_chat_summary(&tx, chat_id)?;
        // Resolve + freeze this chat's execution model (locking in the global
        // default if the chat has none yet); the assistant placeholder + run are
        // then attributed to it, immune to a later model switch.
        let selected_model = chat_run_model(&chat, &selected_model);
        if selected_model.model_id.trim().is_empty() {
            return Err(MothershipError::InvalidRequest(
                "no LLM model selected; connect a provider and choose a model first".to_string(),
            ));
        }
        persist_chat_model(
            &tx,
            &chat.id,
            &selected_model.provider_id,
            &selected_model.model_id,
        )?;
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
            provider_id: Some(selected_model.provider_id.clone()),
            model_id: Some(selected_model.model_id.clone()),
            approval_mode: chat.approval_mode,
            reasoning: chat.reasoning,
            fast_mode: chat.fast_mode,
            draft: chat.draft,
            created_at: chat.created_at,
            updated_at: now,
        };
        update_chat_summary(&tx, &updated_chat)?;
        tx.commit()?;
        let run_fast_mode = updated_chat.fast_mode.unwrap_or(false);

        Ok(SendChatMessageResult {
            run_id: generate_id("chat_run")?,
            chat: updated_chat,
            user_message,
            assistant_message,
            removed_message_ids: Vec::new(),
            context: ChatRunContextSpec {
                include_failed_assistant_message_id: Some(failed_assistant_message.id),
                reasoning: None,
                fast_mode: run_fast_mode,
            },
        })
    }

    /// Sets the execution model for a chat (used for its FUTURE runs). The
    /// provider/model is validated against installed adapters at the connector
    /// layer BEFORE this is called — this only persists. Clears any cached
    /// provider session when the provider changes, since it belongs to the old
    /// provider.
    pub fn set_chat_model(
        &self,
        chat_id: &str,
        provider_id: &str,
        model_id: &str,
    ) -> Result<ChatThreadSummary> {
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("provider_id", provider_id)?;
        validate_identifier("model_id", model_id)?;

        let connection = self.connect()?;
        let provider_changed = chat_model(&connection, chat_id)?
            .map(|model| model.provider_id != provider_id)
            .unwrap_or(true);

        let changed = connection.execute(
            "
            UPDATE chats
            SET chat_model_provider_id = ?2,
                chat_model_id = ?3
            WHERE id = ?1 AND archived = 0
            ",
            params![chat_id, provider_id, model_id],
        )?;
        if changed == 0 {
            return Err(MothershipError::InvalidRequest(format!(
                "chat not found: {chat_id}"
            )));
        }

        if provider_changed {
            clear_chat_provider_state(&connection, chat_id)?;
        }

        select_chat_summary(&connection, chat_id)
    }

    /// Persists a chat's per-chat session state: approval mode, reasoning option,
    /// fast mode, and the unsent composer draft. Each is a full overwrite (the
    /// UI sends the chat's current values); an empty/blank string clears text
    /// fields to NULL.
    /// `updated_at` is deliberately NOT bumped, so saving a draft doesn't reorder
    /// the chat list. Returns the refreshed summary.
    pub fn set_chat_state(
        &self,
        chat_id: &str,
        approval_mode: Option<&str>,
        reasoning: Option<&str>,
        fast_mode: Option<bool>,
        draft: Option<&str>,
    ) -> Result<ChatThreadSummary> {
        validate_identifier("chat_id", chat_id)?;
        fn blank_to_none(value: Option<&str>) -> Option<&str> {
            value.filter(|text| !text.trim().is_empty())
        }

        let connection = self.connect()?;
        let changed = connection.execute(
            "
            UPDATE chats
            SET approval_mode = ?2,
                reasoning_option = ?3,
                fast_mode = ?4,
                draft = ?5
            WHERE id = ?1 AND archived = 0
            ",
            params![
                chat_id,
                blank_to_none(approval_mode),
                blank_to_none(reasoning),
                fast_mode,
                blank_to_none(draft),
            ],
        )?;
        if changed == 0 {
            return Err(MothershipError::InvalidRequest(format!(
                "chat not found: {chat_id}"
            )));
        }

        select_chat_summary(&connection, chat_id)
    }

    /// Renames a chat (user-initiated). The title is trimmed and length-capped;
    /// `updated_at` is deliberately NOT bumped so renaming doesn't reorder the
    /// list. Returns the refreshed summary.
    pub fn rename_chat(&self, chat_id: &str, title: &str) -> Result<ChatThreadSummary> {
        validate_identifier("chat_id", chat_id)?;
        let title = sanitize_user_label(title, 200)?;

        let connection = self.connect()?;
        let changed = connection.execute(
            "UPDATE chats SET title = ?2 WHERE id = ?1 AND archived = 0",
            params![chat_id, title],
        )?;
        if changed == 0 {
            return Err(MothershipError::InvalidRequest(format!(
                "chat not found: {chat_id}"
            )));
        }

        select_chat_summary(&connection, chat_id)
    }

    /// Permanently deletes a chat with its messages, tool calls, message parts,
    /// and recorded change sets (snapshot blobs are content-addressed and left
    /// to the pruning pass). Irreversible.
    pub fn delete_chat(&self, chat_id: &str) -> Result<()> {
        validate_identifier("chat_id", chat_id)?;

        let mut connection = self.connect()?;
        let tx = connection.transaction()?;
        // Change sets reference chats by plain column (no FK cascade) — clear
        // them explicitly; their files/reverts/conflicts cascade off them.
        tx.execute(
            "DELETE FROM change_sets WHERE chat_id = ?1",
            params![chat_id],
        )?;
        let deleted = tx.execute("DELETE FROM chats WHERE id = ?1", params![chat_id])?;
        if deleted == 0 {
            return Err(MothershipError::InvalidRequest(format!(
                "chat not found: {chat_id}"
            )));
        }
        tx.commit()?;
        Ok(())
    }

    /// Renames a project's display name (the on-disk folder is untouched).
    pub fn rename_project(&self, project_id: &str, name: &str) -> Result<ProjectSnapshot> {
        validate_identifier("project_id", project_id)?;
        let name = sanitize_user_label(name, 120)?;

        let connection = self.connect()?;
        let now = current_timestamp();
        let changed = connection.execute(
            "UPDATE projects SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![project_id, name, now],
        )?;
        if changed == 0 {
            return Err(MothershipError::InvalidRequest(format!(
                "project not found: {project_id}"
            )));
        }

        project_snapshot(&connection)
    }

    /// Removes a project from Mothership together with ALL its chats and their
    /// recorded change sets. The workspace folder on disk is untouched.
    /// Irreversible (for the chat history).
    pub fn delete_project(&self, project_id: &str) -> Result<ProjectSnapshot> {
        validate_identifier("project_id", project_id)?;

        let mut connection = self.connect()?;
        let tx = connection.transaction()?;
        tx.execute(
            "DELETE FROM change_sets WHERE project_id = ?1",
            params![project_id],
        )?;
        // Chats cascade their messages/tool calls/parts via FKs.
        tx.execute(
            "DELETE FROM chats WHERE project_id = ?1",
            params![project_id],
        )?;
        let deleted = tx.execute("DELETE FROM projects WHERE id = ?1", params![project_id])?;
        if deleted == 0 {
            return Err(MothershipError::InvalidRequest(format!(
                "project not found: {project_id}"
            )));
        }
        // A dangling active-project hint would point at nothing; clear it so
        // clients fall back to their own session restore.
        if get_setting(&tx, ACTIVE_PROJECT_SETTING_KEY)?.as_deref() == Some(project_id) {
            delete_setting(&tx, ACTIVE_PROJECT_SETTING_KEY)?;
        }
        tx.commit()?;

        project_snapshot(&connection)
    }

    /// Persists the user-picked sidebar appearance for a project. `icon` is
    /// `emoji:<char>` / `lucide:<id>` (None clears back to the folder glyph);
    /// `icon_color` is `#rrggbb` (None clears to the theme default).
    pub fn set_project_appearance(
        &self,
        project_id: &str,
        icon: Option<&str>,
        icon_color: Option<&str>,
    ) -> Result<ProjectSnapshot> {
        validate_identifier("project_id", project_id)?;
        let icon = icon.map(str::trim).filter(|value| !value.is_empty());
        if let Some(icon) = icon {
            if icon.chars().count() > 64 {
                return Err(MothershipError::InvalidRequest(
                    "project icon value is too long".to_string(),
                ));
            }
        }
        let icon_color = icon_color.map(str::trim).filter(|value| !value.is_empty());
        if let Some(color) = icon_color {
            let valid = color.len() == 7
                && color.starts_with('#')
                && color[1..].chars().all(|c| c.is_ascii_hexdigit());
            if !valid {
                return Err(MothershipError::InvalidRequest(
                    "project icon color must be #rrggbb".to_string(),
                ));
            }
        }

        let connection = self.connect()?;
        let now = current_timestamp();
        let changed = connection.execute(
            "UPDATE projects SET icon = ?2, icon_color = ?3, updated_at = ?4 WHERE id = ?1",
            params![project_id, icon, icon_color, now],
        )?;
        if changed == 0 {
            return Err(MothershipError::InvalidRequest(format!(
                "project not found: {project_id}"
            )));
        }

        project_snapshot(&connection)
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

    /// Append a streamed delta to the assistant message and its live text
    /// part. Deliberately returns no event and re-reads nothing: callers emit
    /// their own coalesced UI delta, and re-SELECTing the accumulated content
    /// on every flush made DB read traffic quadratic in answer length.
    pub fn append_chat_run_delta(
        &self,
        run_id: &str,
        chat_id: &str,
        assistant_message_id: &str,
        delta: &str,
    ) -> Result<()> {
        validate_identifier("run_id", run_id)?;
        validate_identifier("chat_id", chat_id)?;
        validate_identifier("assistant_message_id", assistant_message_id)?;

        if delta.is_empty() {
            return Ok(());
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
        Ok(())
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
            removed_message_ids: Vec::new(),
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
            removed_message_ids: Vec::new(),
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
            removed_message_ids: Vec::new(),
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
            removed_message_ids: Vec::new(),
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

    pub fn feature_routes(&self) -> Result<Vec<FeatureRoute>> {
        let connection = self.connect()?;
        feature_routes(&connection)
    }

    pub fn set_feature_route(
        &self,
        feature: &str,
        provider_id: &str,
        model_id: &str,
        options: &serde_json::Value,
    ) -> Result<FeatureRoute> {
        validate_feature_id(feature)?;
        validate_identifier("provider_id", provider_id)?;
        validate_identifier("model_id", model_id)?;
        let connection = self.connect()?;
        let now = current_timestamp();
        let options_json = serde_json::to_string(options)
            .map_err(|error| MothershipError::InvalidRequest(error.to_string()))?;
        connection.execute(
            "
            INSERT INTO feature_routes (
                feature, scope_kind, scope_id, provider_id, model_id, options_json, updated_at
            )
            VALUES (?1, 'global', '', ?2, ?3, ?4, ?5)
            ON CONFLICT(feature, scope_kind, scope_id) DO UPDATE SET
                provider_id = excluded.provider_id,
                model_id = excluded.model_id,
                options_json = excluded.options_json,
                updated_at = excluded.updated_at
            ",
            params![feature, provider_id, model_id, options_json, now],
        )?;
        feature_route(&connection, feature)
    }

    /// Provider ids the user has switched off. A provider is enabled by default;
    /// only the explicitly disabled ones are persisted (as `provider.disabled.<id>`
    /// rows), so a never-touched or freshly installed adapter is always on.
    pub fn disabled_provider_ids(&self) -> Result<std::collections::BTreeSet<String>> {
        let connection = self.connect()?;
        Ok(settings_with_prefix(&connection, PROVIDER_DISABLED_PREFIX)?
            .into_iter()
            .filter(|(_, value)| value == "1")
            .filter_map(|(key, _)| {
                key.strip_prefix(PROVIDER_DISABLED_PREFIX)
                    .map(str::to_string)
            })
            .collect())
    }

    /// Switch a provider on or off. Enabling clears the persisted flag (back to
    /// the default-on state); disabling records it. Provider-agnostic — the app
    /// layer validates the id exists among installed adapters.
    pub fn set_provider_enabled(&self, provider_id: &str, enabled: bool) -> Result<()> {
        validate_identifier("provider_id", provider_id)?;
        let connection = self.connect()?;
        let key = format!("{PROVIDER_DISABLED_PREFIX}{provider_id}");
        if enabled {
            delete_setting(&connection, &key)
        } else {
            set_setting(&connection, &key, "1", &current_timestamp())
        }
    }

    // --- Personalization (user-authored prompt additions) ------------------

    /// The full personalization view (global + every provider/model override).
    pub fn personalization_settings(&self) -> Result<PersonalizationSettings> {
        let connection = self.connect()?;
        personalization_settings(&connection)
    }

    /// Store (or, when `content` is blank, clear) the instruction for one scope:
    /// global (`None`/`None`), a provider (`Some`/`None`), or a provider+model
    /// (`Some`/`Some`). Returns the refreshed full view.
    pub fn set_personalization(
        &self,
        provider_id: Option<&str>,
        model_id: Option<&str>,
        content: &str,
    ) -> Result<PersonalizationSettings> {
        let key = personalization_key(provider_id, model_id)?;
        let connection = self.connect()?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            delete_setting(&connection, &key)?;
        } else {
            set_setting(&connection, &key, trimmed, &current_timestamp())?;
        }
        personalization_settings(&connection)
    }

    /// The non-empty instruction scopes that apply to a run, ordered broad →
    /// specific (global, provider, provider+model). Each entry is a stable scope
    /// id and its content, ready to append as prompt sections.
    pub fn personalization_for(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Vec<(String, String)>> {
        let connection = self.connect()?;
        let mut out = Vec::new();
        if let Some(value) = get_setting(&connection, PERSONALIZATION_GLOBAL_KEY)? {
            if !value.trim().is_empty() {
                out.push(("global".to_string(), value));
            }
        }
        if !provider_id.trim().is_empty() {
            let provider_key = format!("{PERSONALIZATION_PROVIDER_PREFIX}{provider_id}");
            if let Some(value) = get_setting(&connection, &provider_key)? {
                if !value.trim().is_empty() {
                    out.push((format!("provider.{provider_id}"), value));
                }
            }
            if !model_id.trim().is_empty() {
                let model_key = format!("{PERSONALIZATION_MODEL_PREFIX}{provider_id}::{model_id}");
                if let Some(value) = get_setting(&connection, &model_key)? {
                    if !value.trim().is_empty() {
                        out.push((format!("model.{provider_id}/{model_id}"), value));
                    }
                }
            }
        }
        Ok(out)
    }

    // --- Tool permission policy (command allow/deny + tool toggles) ---------

    /// The persisted user allow/deny rules (sanitized into canonical form).
    pub fn tool_policy_settings(&self) -> Result<ToolPolicySettings> {
        let connection = self.connect()?;
        Ok(ToolPolicySettings {
            command_allow: read_setting_list(&connection, PERMISSIONS_COMMAND_ALLOW_KEY)?,
            command_deny: read_setting_list(&connection, PERMISSIONS_COMMAND_DENY_KEY)?,
            disabled_tools: read_setting_list(&connection, PERMISSIONS_DISABLED_TOOLS_KEY)?,
        }
        .sanitized())
    }

    /// Persist the user allow/deny rules. The caller is expected to pass an
    /// already-sanitized [`ToolPolicySettings`] (the runtime store sanitizes on
    /// the way in); this just writes the three newline-joined lists.
    pub fn set_tool_policy_settings(&self, settings: &ToolPolicySettings) -> Result<()> {
        let connection = self.connect()?;
        let now = current_timestamp();
        write_setting_list(
            &connection,
            PERMISSIONS_COMMAND_ALLOW_KEY,
            &settings.command_allow,
            &now,
        )?;
        write_setting_list(
            &connection,
            PERMISSIONS_COMMAND_DENY_KEY,
            &settings.command_deny,
            &now,
        )?;
        write_setting_list(
            &connection,
            PERMISSIONS_DISABLED_TOOLS_KEY,
            &settings.disabled_tools,
            &now,
        )?;
        Ok(())
    }

    // --- Change journal retention ----------------------------------------

    /// How many of the newest MESSAGES keep their change sets per project
    /// (`0` = unlimited). Counted in messages, not sets — one agent message can
    /// record hundreds of sets and they survive or go together. Unset or
    /// unparseable values fall back to the default.
    pub fn change_journal_retention(&self) -> Result<u32> {
        let connection = self.connect()?;
        Ok(get_setting(&connection, CHANGE_JOURNAL_RETENTION_KEY)?
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(CHANGE_JOURNAL_RETENTION_DEFAULT))
    }

    /// Persist the change-journal retention, returning the stored value.
    pub fn set_change_journal_retention(&self, value: u32) -> Result<u32> {
        let connection = self.connect()?;
        set_setting(
            &connection,
            CHANGE_JOURNAL_RETENTION_KEY,
            &value.to_string(),
            &current_timestamp(),
        )?;
        Ok(value)
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

/// The schema version a fully-migrated database sits at (the last entry of
/// [`MIGRATIONS`]). Bump when appending a migration.
const LATEST_SCHEMA_VERSION: i64 = 15;

/// One registry entry: a schema version and the step that produces it.
type Migration = (i64, fn(&Connection) -> Result<()>);

/// The ordered migration registry. Each entry runs in its own transaction and
/// records its version; [`migrate`] applies only entries newer than the highest
/// version already recorded.
///
/// Versions 1-12 predate this registry: the legacy path re-ran one big
/// idempotent DDL batch on every open and recorded versions 1..=13 with
/// `INSERT OR IGNORE`. The baseline entry therefore carries the highest legacy
/// version (13) and consists of exactly that guarded DDL, so it is safe on a
/// fresh database *and* on any database the legacy path produced (including one
/// that only recorded part of the 1..=13 range): guarded DDL never drops or
/// rewrites existing rows. Future migrations append as `(version, fn)` pairs —
/// never fold new DDL into the baseline, or databases already at v13 would
/// silently skip it.
const MIGRATIONS: &[Migration] = &[
    (13, migrate_baseline_schema),
    (14, migrate_change_file_hash_indexes),
    (15, migrate_project_appearance),
];

fn migrate(connection: &mut Connection) -> Result<()> {
    debug_assert_eq!(
        MIGRATIONS.last().map(|(version, _apply)| *version),
        Some(LATEST_SCHEMA_VERSION),
        "LATEST_SCHEMA_VERSION must match the last registry entry",
    );

    // The bookkeeping table lives outside the registry: it must exist before
    // any version can be read or recorded.
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        );
        ",
    )?;

    for (version, apply) in MIGRATIONS {
        // Each step runs in its own IMMEDIATE transaction, and the applied
        // version is re-read inside it: a second process racing the same
        // database blocks on the write lock, then sees the recorded version
        // and skips — never double-applying a step.
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let applied: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;
        if *version <= applied {
            continue; // dropping `tx` rolls back the (read-only) transaction
        }
        apply(&tx)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, current_timestamp()],
        )?;
        tx.commit()?;
    }

    Ok(())
}

/// Baseline (v13): the full schema as guarded, re-runnable DDL — the exact
/// batch the legacy migration path executed on every open.
fn migrate_baseline_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "
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
            chat_model_provider_id TEXT,
            chat_model_id TEXT,
            approval_mode TEXT,
            reasoning_option TEXT,
            fast_mode INTEGER,
            draft TEXT,
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

        CREATE TABLE IF NOT EXISTS feature_routes (
            feature TEXT NOT NULL,
            scope_kind TEXT NOT NULL DEFAULT 'global',
            scope_id TEXT NOT NULL DEFAULT '',
            provider_id TEXT NOT NULL,
            model_id TEXT NOT NULL,
            options_json TEXT NOT NULL DEFAULT '{}',
            updated_at TEXT NOT NULL,
            PRIMARY KEY (feature, scope_kind, scope_id)
        );

        CREATE INDEX IF NOT EXISTS idx_feature_routes_scope
            ON feature_routes (scope_kind, scope_id);

        -- Typed tool storage (source of truth for tool calls). The legacy
        -- chat_tool_events stream is retained as a feed/fallback; these tables
        -- carry the typed kind, semantic payload, and artifact references so the
        -- UI can render semantic cards without parsing strings. Large content
        -- (full diffs, output, search results) lives in artifacts via log_ref —
        -- never inline in these rows.
        CREATE TABLE IF NOT EXISTS tool_calls (
            tool_call_id TEXT PRIMARY KEY,
            chat_id TEXT NOT NULL,
            message_id TEXT NOT NULL,
            run_id TEXT,
            project_id TEXT,
            tool_name TEXT NOT NULL,
            tool_kind TEXT NOT NULL,
            status TEXT NOT NULL,
            permission_state TEXT NOT NULL,
            summary TEXT,
            touched_paths TEXT,
            payload_json TEXT,
            started_at TEXT NOT NULL,
            completed_at TEXT,
            FOREIGN KEY(chat_id) REFERENCES chats(id) ON DELETE CASCADE,
            FOREIGN KEY(message_id) REFERENCES chat_messages(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_tool_calls_message
            ON tool_calls (message_id);

        CREATE INDEX IF NOT EXISTS idx_tool_calls_chat
            ON tool_calls (chat_id);

        CREATE TABLE IF NOT EXISTS tool_events (
            id INTEGER PRIMARY KEY,
            tool_call_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            message_preview TEXT,
            typed_payload_json TEXT,
            artifact_refs TEXT,
            occurred_at TEXT NOT NULL,
            FOREIGN KEY(tool_call_id) REFERENCES tool_calls(tool_call_id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_tool_events_call
            ON tool_events (tool_call_id, id);

        CREATE TABLE IF NOT EXISTS tool_artifacts (
            artifact_id TEXT NOT NULL,
            tool_call_id TEXT NOT NULL,
            artifact_kind TEXT NOT NULL,
            content_type TEXT NOT NULL,
            preview TEXT,
            log_ref TEXT,
            size_bytes INTEGER NOT NULL DEFAULT 0,
            sha256 TEXT,
            truncated INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            PRIMARY KEY (tool_call_id, artifact_id),
            FOREIGN KEY(tool_call_id) REFERENCES tool_calls(tool_call_id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_tool_artifacts_call
            ON tool_artifacts (tool_call_id);

        -- Workspace Change Journal (the user-facing rollback layer). SQLite owns
        -- the metadata and relationships; the actual before/after file bytes live
        -- in the content-addressed blob store (referenced here only by hash).
        -- chat_id/message_id are plain columns (not FKs) on purpose: change-set
        -- retention/pruning is an explicit, observable later slice, not implicit
        -- cascade.
        CREATE TABLE IF NOT EXISTS change_sets (
            id TEXT PRIMARY KEY,
            project_id TEXT,
            run_id TEXT,
            chat_id TEXT,
            message_id TEXT,
            tool_call_id TEXT,
            status TEXT NOT NULL,
            tool_failed INTEGER NOT NULL DEFAULT 0,
            file_count INTEGER NOT NULL DEFAULT 0,
            additions INTEGER NOT NULL DEFAULT 0,
            deletions INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_change_sets_message
            ON change_sets (message_id, id);

        CREATE INDEX IF NOT EXISTS idx_change_sets_chat
            ON change_sets (chat_id, id);

        CREATE TABLE IF NOT EXISTS change_files (
            id TEXT PRIMARY KEY,
            change_set_id TEXT NOT NULL,
            path TEXT NOT NULL,
            old_path TEXT,
            op TEXT NOT NULL,
            additions INTEGER NOT NULL DEFAULT 0,
            deletions INTEGER NOT NULL DEFAULT 0,
            before_hash TEXT,
            after_hash TEXT,
            is_binary INTEGER NOT NULL DEFAULT 0,
            is_large INTEGER NOT NULL DEFAULT 0,
            position INTEGER NOT NULL DEFAULT 0,
            FOREIGN KEY(change_set_id) REFERENCES change_sets(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_change_files_set
            ON change_files (change_set_id, position);

        CREATE TABLE IF NOT EXISTS change_reverts (
            id TEXT PRIMARY KEY,
            change_set_id TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            completed_at TEXT,
            error TEXT,
            FOREIGN KEY(change_set_id) REFERENCES change_sets(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_change_reverts_set
            ON change_reverts (change_set_id, id);

        CREATE TABLE IF NOT EXISTS change_conflicts (
            id TEXT PRIMARY KEY,
            revert_id TEXT NOT NULL,
            path TEXT NOT NULL,
            reason TEXT NOT NULL,
            expected_hash TEXT,
            actual_hash TEXT,
            details TEXT,
            FOREIGN KEY(revert_id) REFERENCES change_reverts(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_change_conflicts_revert
            ON change_conflicts (revert_id);
        ",
    )?;

    // Fresh databases already have these columns; ALTER brings existing ones
    // up to date (guarded, so re-running is a no-op).
    add_column_if_missing(connection, "chat_messages", "provider_id", "TEXT")?;
    add_column_if_missing(connection, "chat_messages", "model_id", "TEXT")?;
    add_column_if_missing(connection, "chat_messages", "error", "TEXT")?;
    add_column_if_missing(connection, "chats", "project_id", "TEXT")?;
    add_column_if_missing(connection, "chats", "provider_state_provider_id", "TEXT")?;
    add_column_if_missing(connection, "chats", "provider_state_json", "TEXT")?;
    add_column_if_missing(connection, "chats", "chat_model_provider_id", "TEXT")?;
    add_column_if_missing(connection, "chats", "chat_model_id", "TEXT")?;
    add_column_if_missing(connection, "chats", "approval_mode", "TEXT")?;
    add_column_if_missing(connection, "chats", "reasoning_option", "TEXT")?;
    add_column_if_missing(connection, "chats", "fast_mode", "INTEGER")?;
    add_column_if_missing(connection, "chats", "draft", "TEXT")?;
    connection.execute(
        "CREATE INDEX IF NOT EXISTS idx_chats_project_updated_at ON chats (project_id, updated_at DESC)",
        [],
    )?;

    Ok(())
}

/// v14: hash-lookup indexes on `change_files`, so the change-journal retention
/// pruner's blob GC ("is this snapshot hash still referenced by any change
/// file?") doesn't scan the whole table per hash.
fn migrate_change_file_hash_indexes(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_change_files_before_hash
            ON change_files (before_hash);

        CREATE INDEX IF NOT EXISTS idx_change_files_after_hash
            ON change_files (after_hash);
        ",
    )?;
    Ok(())
}

/// v15: user-customizable project appearance (sidebar icon + accent color).
fn migrate_project_appearance(connection: &Connection) -> Result<()> {
    add_column_if_missing(connection, "projects", "icon", "TEXT")?;
    add_column_if_missing(connection, "projects", "icon_color", "TEXT")?;
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
            projects.icon,
            projects.icon_color,
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
                projects.icon,
                projects.icon_color,
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

fn delete_setting(connection: &Connection, key: &str) -> Result<()> {
    connection.execute("DELETE FROM app_settings WHERE key = ?1", params![key])?;
    Ok(())
}

/// Read a newline-joined setting back into its trimmed, non-empty lines.
fn read_setting_list(connection: &Connection, key: &str) -> Result<Vec<String>> {
    Ok(get_setting(connection, key)?
        .map(|value| {
            value
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect()
        })
        .unwrap_or_default())
}

/// Store a list as a newline-joined setting, deleting the key when empty.
fn write_setting_list(
    connection: &Connection,
    key: &str,
    items: &[String],
    updated_at: &str,
) -> Result<()> {
    if items.is_empty() {
        delete_setting(connection, key)
    } else {
        set_setting(connection, key, &items.join("\n"), updated_at)
    }
}

/// Every `app_settings` row whose key starts with `prefix`, as (key, value).
fn settings_with_prefix(connection: &Connection, prefix: &str) -> Result<Vec<(String, String)>> {
    // Escape LIKE metacharacters in the literal prefix so a key fragment that
    // happens to contain `%`/`_` can't widen the match.
    let pattern = format!(
        "{}%",
        prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let mut statement = connection.prepare(
        "SELECT key, value FROM app_settings WHERE key LIKE ?1 ESCAPE '\\' ORDER BY key",
    )?;
    let rows = statement.query_map(params![pattern], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The `app_settings` key for a personalization scope. `(None, None)` is the
/// global instruction; `(Some, None)` a provider; `(Some, Some)` a model. A
/// model id without a provider is rejected.
fn personalization_key(provider_id: Option<&str>, model_id: Option<&str>) -> Result<String> {
    match (provider_id, model_id) {
        (None, None) => Ok(PERSONALIZATION_GLOBAL_KEY.to_string()),
        (Some(provider_id), None) => {
            let provider_id = provider_id.trim();
            if provider_id.is_empty() {
                return Err(MothershipError::InvalidRequest(
                    "provider id cannot be empty".to_string(),
                ));
            }
            Ok(format!("{PERSONALIZATION_PROVIDER_PREFIX}{provider_id}"))
        }
        (Some(provider_id), Some(model_id)) => {
            let provider_id = provider_id.trim();
            let model_id = model_id.trim();
            if provider_id.is_empty() || model_id.is_empty() {
                return Err(MothershipError::InvalidRequest(
                    "provider id and model id cannot be empty".to_string(),
                ));
            }
            Ok(format!(
                "{PERSONALIZATION_MODEL_PREFIX}{provider_id}::{model_id}"
            ))
        }
        (None, Some(_)) => Err(MothershipError::InvalidRequest(
            "model-scoped personalization requires a provider id".to_string(),
        )),
    }
}

fn personalization_settings(connection: &Connection) -> Result<PersonalizationSettings> {
    let global = get_setting(connection, PERSONALIZATION_GLOBAL_KEY)?.unwrap_or_default();

    let mut providers = Vec::new();
    for (key, content) in settings_with_prefix(connection, PERSONALIZATION_PROVIDER_PREFIX)? {
        let provider_id = key[PERSONALIZATION_PROVIDER_PREFIX.len()..].to_string();
        if !provider_id.is_empty() {
            providers.push(ProviderInstruction {
                provider_id,
                content,
            });
        }
    }

    let mut models = Vec::new();
    for (key, content) in settings_with_prefix(connection, PERSONALIZATION_MODEL_PREFIX)? {
        let rest = &key[PERSONALIZATION_MODEL_PREFIX.len()..];
        if let Some((provider_id, model_id)) = rest.split_once("::") {
            if !provider_id.is_empty() && !model_id.is_empty() {
                models.push(ModelInstruction {
                    provider_id: provider_id.to_string(),
                    model_id: model_id.to_string(),
                    content,
                });
            }
        }
    }

    Ok(PersonalizationSettings {
        global,
        providers,
        models,
    })
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
            SELECT id, project_id, title, preview, message_count, created_at, updated_at,
                   chat_model_provider_id, chat_model_id,
                   approval_mode, reasoning_option, fast_mode, draft
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
        SELECT id, project_id, title, preview, message_count, created_at, updated_at,
               chat_model_provider_id, chat_model_id,
               approval_mode, reasoning_option, fast_mode, draft
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

/// The model a run should use for a chat: the chat's own model if set, else the
/// provided global fallback (default-for-new-chats).
fn chat_run_model(chat: &ChatThreadSummary, fallback: &SelectedLlmModel) -> SelectedLlmModel {
    match (chat.provider_id.as_deref(), chat.model_id.as_deref()) {
        (Some(provider_id), Some(model_id)) if !model_id.trim().is_empty() => SelectedLlmModel {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            updated_at: fallback.updated_at.clone(),
        },
        _ => fallback.clone(),
    }
}

/// Persists a chat's execution model. Called at run start to lock in the
/// resolved model, so reopening restores what the chat actually ran with rather
/// than the then-current global default.
fn persist_chat_model(
    connection: &Connection,
    chat_id: &str,
    provider_id: &str,
    model_id: &str,
) -> Result<()> {
    connection.execute(
        "UPDATE chats SET chat_model_provider_id = ?2, chat_model_id = ?3 WHERE id = ?1",
        params![chat_id, provider_id, model_id],
    )?;
    Ok(())
}

/// Reads a chat's stored execution model, if it has one.
fn chat_model(connection: &Connection, chat_id: &str) -> Result<Option<SelectedLlmModel>> {
    let row = connection
        .query_row(
            "
            SELECT chat_model_provider_id, chat_model_id, updated_at
            FROM chats
            WHERE id = ?1 AND archived = 0
            ",
            params![chat_id],
            |row| {
                let provider_id: Option<String> = row.get(0)?;
                let model_id: Option<String> = row.get(1)?;
                let updated_at: String = row.get(2)?;
                Ok(match (provider_id, model_id) {
                    (Some(provider_id), Some(model_id)) if !model_id.trim().is_empty() => {
                        Some(SelectedLlmModel {
                            provider_id,
                            model_id,
                            updated_at,
                        })
                    }
                    _ => None,
                })
            },
        )
        .optional()?;
    Ok(row.flatten())
}

fn select_chat_summary(connection: &Connection, chat_id: &str) -> Result<ChatThreadSummary> {
    connection
        .query_row(
            "
            SELECT id, project_id, title, preview, message_count, created_at, updated_at,
                   chat_model_provider_id, chat_model_id,
                   approval_mode, reasoning_option, fast_mode, draft
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
        INSERT INTO chats (id, project_id, title, preview, message_count, archived, chat_model_provider_id, chat_model_id, approval_mode, reasoning_option, fast_mode, draft, created_at, updated_at)
        VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        ",
        params![
            chat.id,
            chat.project_id.as_deref(),
            chat.title,
            chat.preview,
            chat.message_count,
            chat.provider_id.as_deref(),
            chat.model_id.as_deref(),
            chat.approval_mode.as_deref(),
            chat.reasoning.as_deref(),
            chat.fast_mode,
            chat.draft.as_deref(),
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
                    // Enriched from the typed tables below (tool_kind / payload /
                    // artifacts), once all legacy rows have been folded in.
                    tool_kind: None,
                    payload: None,
                    artifacts: Vec::new(),
                });
                index
            }
        };

        apply_tool_event_row(&mut records[record_index], row);
    }

    // Enrich each record with typed storage (tool_kind / payload / artifacts) so
    // a reloaded conversation renders semantic cards, not just text.
    let tool_call_ids: Vec<&str> = records
        .iter()
        .map(|record| record.tool_call_id.as_str())
        .collect();
    let typed = select_typed_tool_data(connection, &tool_call_ids)?;
    for record in &mut records {
        if let Some((kind, payload, artifacts)) = typed.get(&record.tool_call_id) {
            record.tool_kind = *kind;
            record.payload = payload.clone();
            record.artifacts = artifacts.clone();
        }
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

/// Copy the typed tool storage (`tool_calls`/`tool_events`/`tool_artifacts`) for a
/// branched/forked chat, remapping `tool_call_id` (via `tool_call_id_map`, built
/// by [`copy_chat_tool_events`]) and `message_id` (via `message_id_map`). Without
/// this, a branched conversation keeps only the legacy feed and loses its
/// `tool_kind`/`payload`/`artifacts`, so semantic cards would silently degrade to
/// the text fallback on reload.
fn copy_typed_tool_data(
    connection: &Connection,
    source_chat_id: &str,
    target_chat_id: &str,
    message_id_map: &HashMap<String, String>,
    tool_call_id_map: &HashMap<String, String>,
) -> Result<()> {
    if tool_call_id_map.is_empty() {
        return Ok(());
    }
    let source_ids = tool_call_id_map
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let placeholders = std::iter::repeat("?")
        .take(source_ids.len())
        .collect::<Vec<_>>()
        .join(", ");

    // 1. tool_calls (must be inserted first — the other two FK to it).
    struct CallRow {
        tool_call_id: String,
        message_id: String,
        project_id: Option<String>,
        tool_name: String,
        tool_kind: String,
        status: String,
        permission_state: String,
        summary: Option<String>,
        touched_paths: Option<String>,
        payload_json: Option<String>,
        started_at: String,
        completed_at: Option<String>,
    }
    let call_sql = format!(
        "SELECT tool_call_id, message_id, project_id, tool_name, tool_kind, status, permission_state, summary, touched_paths, payload_json, started_at, completed_at
         FROM tool_calls WHERE chat_id = ? AND tool_call_id IN ({placeholders})"
    );
    let calls = {
        let mut statement = connection.prepare(&call_sql)?;
        let params = std::iter::once(source_chat_id)
            .chain(source_ids.iter().copied())
            .collect::<Vec<_>>();
        let rows = statement.query_map(params_from_iter(params), |row| {
            Ok(CallRow {
                tool_call_id: row.get(0)?,
                message_id: row.get(1)?,
                project_id: row.get(2)?,
                tool_name: row.get(3)?,
                tool_kind: row.get(4)?,
                status: row.get(5)?,
                permission_state: row.get(6)?,
                summary: row.get(7)?,
                touched_paths: row.get(8)?,
                payload_json: row.get(9)?,
                started_at: row.get(10)?,
                completed_at: row.get(11)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for row in calls {
        let (Some(target_call_id), Some(target_message_id)) = (
            tool_call_id_map.get(&row.tool_call_id),
            message_id_map.get(&row.message_id),
        ) else {
            continue;
        };
        connection.execute(
            "INSERT INTO tool_calls (
                tool_call_id, chat_id, message_id, run_id, project_id, tool_name, tool_kind,
                status, permission_state, summary, touched_paths, payload_json, started_at, completed_at
            ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                target_call_id,
                target_chat_id,
                target_message_id,
                row.project_id,
                row.tool_name,
                row.tool_kind,
                row.status,
                row.permission_state,
                row.summary,
                row.touched_paths,
                row.payload_json,
                row.started_at,
                row.completed_at,
            ],
        )?;
    }

    // 2. tool_events
    struct EventRow {
        tool_call_id: String,
        kind: String,
        message_preview: Option<String>,
        typed_payload_json: Option<String>,
        artifact_refs: Option<String>,
        occurred_at: String,
    }
    let event_sql = format!(
        "SELECT tool_call_id, kind, message_preview, typed_payload_json, artifact_refs, occurred_at
         FROM tool_events WHERE tool_call_id IN ({placeholders}) ORDER BY id ASC"
    );
    let events = {
        let mut statement = connection.prepare(&event_sql)?;
        let rows = statement.query_map(params_from_iter(source_ids.iter().copied()), |row| {
            Ok(EventRow {
                tool_call_id: row.get(0)?,
                kind: row.get(1)?,
                message_preview: row.get(2)?,
                typed_payload_json: row.get(3)?,
                artifact_refs: row.get(4)?,
                occurred_at: row.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for row in events {
        let Some(target_call_id) = tool_call_id_map.get(&row.tool_call_id) else {
            continue;
        };
        connection.execute(
            "INSERT INTO tool_events (tool_call_id, kind, message_preview, typed_payload_json, artifact_refs, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                target_call_id,
                row.kind,
                row.message_preview,
                row.typed_payload_json,
                row.artifact_refs,
                row.occurred_at,
            ],
        )?;
    }

    // 3. tool_artifacts
    struct ArtifactRow {
        artifact_id: String,
        tool_call_id: String,
        artifact_kind: String,
        content_type: String,
        preview: Option<String>,
        log_ref: Option<String>,
        size_bytes: i64,
        sha256: Option<String>,
        truncated: i64,
        created_at: String,
    }
    let artifact_sql = format!(
        "SELECT artifact_id, tool_call_id, artifact_kind, content_type, preview, log_ref, size_bytes, sha256, truncated, created_at
         FROM tool_artifacts WHERE tool_call_id IN ({placeholders})"
    );
    let artifacts = {
        let mut statement = connection.prepare(&artifact_sql)?;
        let rows = statement.query_map(params_from_iter(source_ids.iter().copied()), |row| {
            Ok(ArtifactRow {
                artifact_id: row.get(0)?,
                tool_call_id: row.get(1)?,
                artifact_kind: row.get(2)?,
                content_type: row.get(3)?,
                preview: row.get(4)?,
                log_ref: row.get(5)?,
                size_bytes: row.get(6)?,
                sha256: row.get(7)?,
                truncated: row.get(8)?,
                created_at: row.get(9)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for row in artifacts {
        let Some(target_call_id) = tool_call_id_map.get(&row.tool_call_id) else {
            continue;
        };
        connection.execute(
            "INSERT INTO tool_artifacts (artifact_id, tool_call_id, artifact_kind, content_type, preview, log_ref, size_bytes, sha256, truncated, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                row.artifact_id,
                target_call_id,
                row.artifact_kind,
                row.content_type,
                row.preview,
                row.log_ref,
                row.size_bytes,
                row.sha256,
                row.truncated,
                row.created_at,
            ],
        )?;
    }

    Ok(())
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

fn select_chat_message_ids_after_position(
    connection: &Connection,
    chat_id: &str,
    position: i64,
) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "
        SELECT id
        FROM chat_messages
        WHERE chat_id = ?1
          AND rowid > ?2
        ORDER BY rowid ASC
        ",
    )?;

    let rows = statement.query_map(params![chat_id, position], |row| row.get(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
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

fn feature_routes(connection: &Connection) -> Result<Vec<FeatureRoute>> {
    let mut statement = connection.prepare(
        "
        SELECT feature, provider_id, model_id, options_json, updated_at
        FROM feature_routes
        WHERE scope_kind = 'global' AND scope_id = ''
        ORDER BY feature
        ",
    )?;
    let rows = statement.query_map([], feature_route_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn feature_route(connection: &Connection, feature: &str) -> Result<FeatureRoute> {
    connection
        .query_row(
            "
            SELECT feature, provider_id, model_id, options_json, updated_at
            FROM feature_routes
            WHERE feature = ?1 AND scope_kind = 'global' AND scope_id = ''
            ",
            params![feature],
            feature_route_from_row,
        )
        .map_err(Into::into)
}

fn feature_route_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FeatureRoute> {
    let options_json: String = row.get(3)?;
    let options = serde_json::from_str(&options_json).unwrap_or(serde_json::Value::Null);
    Ok(FeatureRoute {
        feature: row.get(0)?,
        provider_id: row.get(1)?,
        model_id: row.get(2)?,
        options,
        updated_at: row.get(4)?,
    })
}

fn validate_feature_id(feature: &str) -> Result<()> {
    let trimmed = feature.trim();
    if trimmed.is_empty()
        || trimmed.len() > 128
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(MothershipError::InvalidRequest(format!(
            "invalid feature id: {feature}"
        )));
    }
    Ok(())
}

fn chat_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatThreadSummary> {
    Ok(ChatThreadSummary {
        id: row.get(0)?,
        project_id: row.get(1)?,
        title: row.get(2)?,
        preview: row.get(3)?,
        message_count: row.get(4)?,
        provider_id: row.get(7)?,
        model_id: row.get(8)?,
        approval_mode: row.get(9)?,
        reasoning: row.get(10)?,
        fast_mode: row.get(11)?,
        draft: row.get(12)?,
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
        icon: row.get(4)?,
        icon_color: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        last_opened_at: row.get(8)?,
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

/// Build the typed payload + artifacts for a tool event. File/search tools carry
/// their semantic payload on the event directly; `run_command` has it (and an
/// output artifact) synthesized from `command` + `result` so the supervisor stays
/// untouched.
fn typed_payload_and_artifacts(
    event: &ToolExecutionEvent,
) -> (Option<serde_json::Value>, Vec<ToolArtifact>) {
    if let Some(payload) = &event.payload {
        return (Some(payload.clone()), Vec::new());
    }

    if event.tool_kind == Some(ToolKind::RunCommand) {
        if let (Some(command), Some(result)) = (&event.command, &event.result) {
            let (payload, artifacts) = crate::tools::run_command_typed_payload(command, result);
            return (Some(payload), artifacts);
        }
    }

    (None, Vec::new())
}

/// Map an event kind to the persisted `tool_calls.status`.
fn tool_call_status_for_event(kind: ToolExecutionEventKind) -> &'static str {
    match kind {
        ToolExecutionEventKind::Queued => "queued",
        ToolExecutionEventKind::PermissionRequested => "awaiting_approval",
        ToolExecutionEventKind::PermissionDenied => "denied",
        ToolExecutionEventKind::WaitingForResource => "waiting",
        ToolExecutionEventKind::Started | ToolExecutionEventKind::Output => "running",
        ToolExecutionEventKind::Completed => "completed",
        ToolExecutionEventKind::Failed => "failed",
        ToolExecutionEventKind::Cancelled => "cancelled",
        ToolExecutionEventKind::TimedOut => "timed_out",
        ToolExecutionEventKind::LoopBlocked => "blocked",
    }
}

/// The permission-state transition implied by an event kind, or `None` to leave
/// the stored value unchanged.
fn permission_state_for_event(kind: ToolExecutionEventKind) -> Option<&'static str> {
    match kind {
        ToolExecutionEventKind::PermissionRequested => Some("requested"),
        ToolExecutionEventKind::PermissionDenied => Some("denied"),
        ToolExecutionEventKind::Started => Some("allowed"),
        _ => None,
    }
}

fn is_terminal_event(kind: ToolExecutionEventKind) -> bool {
    matches!(
        kind,
        ToolExecutionEventKind::Completed
            | ToolExecutionEventKind::Failed
            | ToolExecutionEventKind::Cancelled
            | ToolExecutionEventKind::TimedOut
            | ToolExecutionEventKind::PermissionDenied
            | ToolExecutionEventKind::LoopBlocked
    )
}

/// Bound a per-call summary so the typed rows never carry a huge message (full
/// diffs/output live in artifacts via `log_ref`, not here).
fn truncate_summary(summary: &str) -> String {
    const MAX_SUMMARY_BYTES: usize = 2 * 1024;
    if summary.len() <= MAX_SUMMARY_BYTES {
        return summary.to_string();
    }
    let mut end = MAX_SUMMARY_BYTES;
    while end > 0 && !summary.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = summary[..end].to_string();
    bounded.push_str(" …[truncated]");
    bounded
}

/// Read the typed data (tool_kind, latest payload, artifacts) for a set of tool
/// calls, keyed by `tool_call_id`. Used to enrich the legacy read path so a
/// reloaded conversation still renders semantic cards.
#[allow(clippy::type_complexity)]
fn select_typed_tool_data(
    connection: &Connection,
    tool_call_ids: &[&str],
) -> Result<
    HashMap<
        String,
        (
            Option<ToolKind>,
            Option<serde_json::Value>,
            Vec<ToolArtifact>,
        ),
    >,
> {
    let mut out: HashMap<
        String,
        (
            Option<ToolKind>,
            Option<serde_json::Value>,
            Vec<ToolArtifact>,
        ),
    > = HashMap::new();
    if tool_call_ids.is_empty() {
        return Ok(out);
    }

    let placeholders = std::iter::repeat("?")
        .take(tool_call_ids.len())
        .collect::<Vec<_>>()
        .join(", ");

    let calls_sql = format!(
        "SELECT tool_call_id, tool_kind, payload_json FROM tool_calls WHERE tool_call_id IN ({placeholders})"
    );
    let mut statement = connection.prepare(&calls_sql)?;
    let rows = statement.query_map(params_from_iter(tool_call_ids.iter().copied()), |row| {
        let id: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let payload_json: Option<String> = row.get(2)?;
        Ok((id, kind, payload_json))
    })?;
    for row in rows {
        let (id, kind, payload_json) = row?;
        let payload = json_value::<serde_json::Value>(payload_json, 2)?;
        out.insert(id, (ToolKind::from_name(&kind), payload, Vec::new()));
    }

    let artifacts_sql = format!(
        "SELECT tool_call_id, artifact_id, artifact_kind, content_type, preview, log_ref, size_bytes, sha256, truncated
         FROM tool_artifacts WHERE tool_call_id IN ({placeholders}) ORDER BY tool_call_id, artifact_id"
    );
    let mut statement = connection.prepare(&artifacts_sql)?;
    let rows = statement.query_map(params_from_iter(tool_call_ids.iter().copied()), |row| {
        let tool_call_id: String = row.get(0)?;
        let artifact = ToolArtifact {
            artifact_id: row.get(1)?,
            kind: row.get(2)?,
            content_type: row.get(3)?,
            preview: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            log_ref: row.get(5)?,
            size_bytes: row.get::<_, i64>(6)?.max(0) as u64,
            sha256: row.get(7)?,
            truncated: row.get::<_, i64>(8)? != 0,
        };
        Ok((tool_call_id, artifact))
    })?;
    for row in rows {
        let (tool_call_id, artifact) = row?;
        out.entry(tool_call_id)
            .or_insert((None, None, Vec::new()))
            .2
            .push(artifact);
    }

    Ok(out)
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

/// Trims a user-entered display name (chat title / project name) and caps it
/// at `max_chars`, rejecting blank input. Char-boundary safe.
fn sanitize_user_label(value: &str, max_chars: usize) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(MothershipError::InvalidRequest(
            "name cannot be empty".to_string(),
        ));
    }
    Ok(trimmed.chars().take(max_chars).collect())
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
    fn provider_enabled_defaults_on_and_round_trips() {
        let database_path = temp_database_path("provider_enabled");
        let database = Database::open(database_path).expect("open database");

        // Untouched providers are enabled by default (nothing persisted).
        assert!(database.disabled_provider_ids().expect("read").is_empty());

        database
            .set_provider_enabled("codex", false)
            .expect("disable codex");
        let disabled = database.disabled_provider_ids().expect("read");
        assert!(disabled.contains("codex"));
        assert_eq!(disabled.len(), 1);

        // Re-enabling clears the flag back to the default-on state.
        database
            .set_provider_enabled("codex", true)
            .expect("enable codex");
        assert!(database.disabled_provider_ids().expect("read").is_empty());
    }

    #[test]
    fn feature_routes_persist_and_validate_identifiers() {
        let database_path = temp_database_path("feature_routes");
        let database = Database::open(database_path.clone()).expect("open database");

        let route = database
            .set_feature_route(
                "media.image.generate",
                "codex",
                "gpt-image-2",
                &serde_json::json!({ "size": "1024x1024" }),
            )
            .expect("set route");
        assert_eq!(route.feature, "media.image.generate");
        assert_eq!(route.provider_id, "codex");
        assert_eq!(route.model_id, "gpt-image-2");
        assert_eq!(route.options["size"], "1024x1024");

        drop(database);
        let reopened = Database::open(database_path.clone()).expect("reopen database");
        let routes = reopened.feature_routes().expect("load routes");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].feature, "media.image.generate");
        assert_eq!(routes[0].options["size"], "1024x1024");

        let bad_feature = reopened
            .set_feature_route(
                "media image",
                "codex",
                "gpt-image-2",
                &serde_json::json!({}),
            )
            .unwrap_err();
        assert!(bad_feature.to_string().contains("feature"));

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn personalization_round_trips_and_layers_scopes() {
        let database_path = temp_database_path("personalization_round_trips");
        let database = Database::open(database_path.clone()).expect("open database");

        database
            .set_personalization(None, None, "global text")
            .expect("set global");
        database
            .set_personalization(Some("codex"), None, "provider text")
            .expect("set provider");
        database
            .set_personalization(Some("codex"), Some("gpt-5.5"), "model text")
            .expect("set model");

        let settings = database.personalization_settings().expect("settings");
        assert_eq!(settings.global, "global text");
        assert_eq!(settings.providers.len(), 1);
        assert_eq!(settings.providers[0].provider_id, "codex");
        assert_eq!(settings.providers[0].content, "provider text");
        assert_eq!(settings.models.len(), 1);
        assert_eq!(settings.models[0].provider_id, "codex");
        assert_eq!(settings.models[0].model_id, "gpt-5.5");

        // A run for codex/gpt-5.5 layers all three scopes, broad → specific.
        let layered = database
            .personalization_for("codex", "gpt-5.5")
            .expect("layered");
        assert_eq!(
            layered.iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(),
            vec!["global text", "provider text", "model text"]
        );

        // A different provider only sees the global scope.
        let other = database
            .personalization_for("openrouter", "whatever")
            .expect("other");
        assert_eq!(
            other.iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(),
            vec!["global text"]
        );

        // Saving blank content clears the scope.
        database
            .set_personalization(Some("codex"), None, "   ")
            .expect("clear provider");
        assert!(database
            .personalization_settings()
            .expect("settings")
            .providers
            .is_empty());

        let _ = std::fs::remove_file(database_path);
    }

    #[test]
    fn tool_policy_settings_persist() {
        let database_path = temp_database_path("tool_policy_settings_persist");
        let database = Database::open(database_path.clone()).expect("open database");

        let settings = crate::ToolPolicySettings {
            command_allow: vec!["npm".to_string()],
            command_deny: vec!["rm".to_string()],
            disabled_tools: vec!["read_file".to_string()],
        };
        database
            .set_tool_policy_settings(&settings)
            .expect("save tool policy");

        let loaded = database.tool_policy_settings().expect("load tool policy");
        assert_eq!(loaded.command_allow, vec!["npm".to_string()]);
        assert_eq!(loaded.command_deny, vec!["rm".to_string()]);
        assert_eq!(loaded.disabled_tools, vec!["read_file".to_string()]);

        let _ = std::fs::remove_file(database_path);
    }

    #[test]
    fn chat_state_round_trips_and_new_chat_copies_settings() {
        let database_path = temp_database_path("chat_state_round_trips");
        let database = Database::open(database_path.clone()).expect("open database");
        let project = create_project(&database, &database_path, "chat_state");

        let chat = database
            .create_chat(&project.id, None)
            .expect("create chat")
            .chat;
        assert_eq!(chat.approval_mode, None);
        assert_eq!(chat.draft, None);

        // Per-chat session state round-trips.
        let updated = database
            .set_chat_state(
                &chat.id,
                Some("yolo"),
                Some("high"),
                Some(true),
                Some("draft text"),
            )
            .expect("set state");
        assert_eq!(updated.approval_mode.as_deref(), Some("yolo"));
        assert_eq!(updated.reasoning.as_deref(), Some("high"));
        assert_eq!(updated.fast_mode, Some(true));
        assert_eq!(updated.draft.as_deref(), Some("draft text"));

        database
            .set_chat_model(&chat.id, "openai", "test-model")
            .expect("set model");

        // A new chat copies model + approval + reasoning, but NOT the draft.
        let copy = database
            .create_chat(&project.id, Some(&chat.id))
            .expect("copy chat")
            .chat;
        assert_eq!(copy.approval_mode.as_deref(), Some("yolo"));
        assert_eq!(copy.reasoning.as_deref(), Some("high"));
        assert_eq!(copy.fast_mode, Some(true));
        assert_eq!(copy.provider_id.as_deref(), Some("openai"));
        assert_eq!(copy.model_id.as_deref(), Some("test-model"));
        assert_eq!(copy.draft, None);

        // Blank/whitespace clears a field to NULL.
        let cleared = database
            .set_chat_state(&chat.id, Some(""), None, None, Some("   "))
            .expect("clear state");
        assert_eq!(cleared.approval_mode, None);
        assert_eq!(cleared.reasoning, None);
        assert_eq!(cleared.fast_mode, None);
        assert_eq!(cleared.draft, None);

        // The state survives a reopen (persisted, not just returned).
        drop(database);
        let reopened = Database::open(database_path.clone()).expect("reopen database");
        let restored = reopened
            .create_chat(&project.id, Some(&copy.id))
            .expect("copy from reopened")
            .chat;
        assert_eq!(restored.approval_mode.as_deref(), Some("yolo"));
        assert_eq!(restored.reasoning.as_deref(), Some("high"));
        assert_eq!(restored.fast_mode, Some(true));

        let _ = std::fs::remove_file(database_path);
    }

    #[test]
    fn sending_a_message_clears_the_chat_draft() {
        let database_path = temp_database_path("send_clears_draft");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let project = create_project(&database, &database_path, "send_clears_draft");

        let chat = database
            .create_chat(&project.id, None)
            .expect("create chat")
            .chat;
        database
            .set_chat_state(&chat.id, None, None, None, Some("half-typed draft"))
            .expect("set draft");

        // Sending consumes the composer draft — it must not survive the send.
        let result = database
            .send_chat_message(Some(&chat.id), Some(&project.id), "actual message")
            .expect("send");
        assert_eq!(result.chat.draft, None, "returned summary clears the draft");

        let reread = database.get_chat(&chat.id, 10).expect("get chat").chat;
        assert_eq!(reread.draft, None, "draft is cleared in the DB on reopen");

        let _ = std::fs::remove_file(database_path);
    }

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
    fn retry_rolls_back_failed_tail_and_starts_new_attempt() {
        let database_path =
            temp_database_path("retry_rolls_back_failed_tail_and_starts_new_attempt");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let project = create_project(&database, &database_path, "retry");

        let run = database
            .begin_chat_run(None, Some(&project.id), "Retry me", None, false)
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
                    ..Default::default()
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

        let legacy_failed_2 = ChatMessage {
            id: "chat_message_legacy_failed_2".to_string(),
            chat_id: run.chat.id.clone(),
            position: 0,
            role: ChatMessageRole::Assistant,
            content: "Failed retry 2".to_string(),
            status: ChatMessageStatus::Failed,
            created_at: current_timestamp(),
            error: Some("boom 2".to_string()),
            provider_id: Some("openai".to_string()),
            model_id: Some("test-model".to_string()),
        };
        let legacy_failed_3 = ChatMessage {
            id: "chat_message_legacy_failed_3".to_string(),
            chat_id: run.chat.id.clone(),
            position: 0,
            role: ChatMessageRole::Assistant,
            content: "Failed retry 3".to_string(),
            status: ChatMessageStatus::Failed,
            created_at: current_timestamp(),
            error: Some("boom 3".to_string()),
            provider_id: Some("openai".to_string()),
            model_id: Some("test-model".to_string()),
        };
        let connection = database.connect().expect("connect database");
        insert_chat_message(&connection, &legacy_failed_2).expect("insert legacy failed retry 2");
        insert_chat_message(&connection, &legacy_failed_3).expect("insert legacy failed retry 3");
        drop(connection);

        let retry = database.begin_retry_run(&run.chat.id).expect("begin retry");
        assert_ne!(retry.assistant_message.id, run.assistant_message.id);
        assert_eq!(retry.assistant_message.status, ChatMessageStatus::Sending);
        assert!(retry.assistant_message.content.is_empty());
        // The original prompt is reused, not duplicated, and the failed tail is
        // removed so retry behaves like the user message was sent again.
        assert_eq!(retry.user_message.content, "Retry me");
        assert_eq!(
            retry.removed_message_ids,
            vec![
                run.assistant_message.id.clone(),
                legacy_failed_2.id,
                legacy_failed_3.id,
            ]
        );
        let conversation = database.get_chat(&run.chat.id, 200).expect("get chat");
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(conversation.messages[0].id, retry.user_message.id);
        assert_eq!(conversation.messages[1].id, retry.assistant_message.id);
        assert!(conversation
            .messages
            .iter()
            .all(|message| message.id != run.assistant_message.id));
        assert!(conversation.tool_executions.is_empty());

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
            .begin_chat_run(None, Some(&project.id), "Continue me", None, false)
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
            .begin_chat_run(
                None,
                Some(&project.id),
                "Show the current folder",
                None,
                false,
            )
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
                    ..Default::default()
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
                    ..Default::default()
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
                    ..Default::default()
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
            .begin_chat_run(
                None,
                Some(&project.id),
                "Inspect the workspace",
                None,
                false,
            )
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
                    ..Default::default()
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
            .begin_chat_run(None, Some(&project.id), "Original prompt", None, false)
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
                false,
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
                    ..Default::default()
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
            .begin_chat_run(None, Some(&project.id), "First prompt", None, false)
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
                false,
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
                    ..Default::default()
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
        let conversation = database
            .create_chat(&project.id, None)
            .expect("create chat");

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
            .begin_chat_run(None, Some(&project.id), "Hello", None, false)
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
    fn rename_delete_and_appearance_round_trip() {
        let database_path = temp_database_path("rename_delete_appearance");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");

        let project = create_project(&database, &database_path, "alpha");
        let run = database
            .begin_chat_run(None, Some(&project.id), "Hello there", None, false)
            .expect("begin run");
        let chat_id = run.chat.id.clone();

        // Rename chat: trimmed, persisted, summary returned.
        let renamed = database
            .rename_chat(&chat_id, "  Renamed chat  ")
            .expect("rename chat");
        assert_eq!(renamed.title, "Renamed chat");
        assert!(database.rename_chat(&chat_id, "   ").is_err());

        // Project appearance: persisted + validated.
        let snapshot = database
            .set_project_appearance(&project.id, Some("emoji:🚀"), Some("#f0b748"))
            .expect("set appearance");
        let stored = snapshot
            .projects
            .iter()
            .find(|item| item.id == project.id)
            .expect("project present");
        assert_eq!(stored.icon.as_deref(), Some("emoji:🚀"));
        assert_eq!(stored.icon_color.as_deref(), Some("#f0b748"));
        assert!(database
            .set_project_appearance(&project.id, None, Some("not-a-color"))
            .is_err());

        // Rename project.
        let snapshot = database
            .rename_project(&project.id, "Beta")
            .expect("rename project");
        assert_eq!(
            snapshot
                .projects
                .iter()
                .find(|item| item.id == project.id)
                .map(|item| item.name.as_str()),
            Some("Beta")
        );

        // Delete chat: rows + cascaded children gone.
        database.delete_chat(&chat_id).expect("delete chat");
        assert!(database.get_chat(&chat_id, 10).is_err());
        assert!(database.delete_chat(&chat_id).is_err());

        // Delete project: project + its chats gone, active hint cleared.
        let run = database
            .begin_chat_run(None, Some(&project.id), "Another", None, false)
            .expect("begin run");
        let snapshot = database.delete_project(&project.id).expect("delete project");
        assert!(snapshot.projects.iter().all(|item| item.id != project.id));
        assert!(snapshot.active_project_id.is_none());
        assert!(database.get_chat(&run.chat.id, 10).is_err());

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
            .begin_chat_run(
                None,
                Some(&first_project.id),
                "First project prompt",
                None,
                false,
            )
            .expect("begin first project run");
        database
            .begin_chat_run(
                None,
                Some(&second_project.id),
                "Second project prompt",
                None,
                false,
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

    #[test]
    fn typed_tool_storage_migration_creates_tables() {
        let database_path = temp_database_path("typed_tool_tables");
        let database = Database::open(database_path.clone()).expect("open database");
        let connection = database.connect().expect("connect");
        for table in ["tool_calls", "tool_events", "tool_artifacts"] {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("query table");
            assert_eq!(count, 1, "typed table `{table}` should exist");
        }
        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .expect("query version");
        assert!(version >= 11, "schema should be >= v11, got {version}");
        drop(connection);
        drop(database);
        let _ = fs::remove_file(database_path);
    }

    fn applied_versions(database: &Database) -> Vec<i64> {
        let connection = database.connect().expect("connect");
        let mut statement = connection
            .prepare("SELECT version FROM schema_migrations ORDER BY version ASC")
            .expect("prepare versions");
        let rows = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .expect("query versions");
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect versions")
    }

    fn table_exists(database: &Database, table: &str) -> bool {
        let connection = database.connect().expect("connect");
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .expect("query table");
        count == 1
    }

    #[test]
    fn fresh_database_starts_empty() {
        let database_path = temp_database_path("fresh_starts_empty");
        let database = Database::open(database_path.clone()).expect("open database");

        // No demo seed: a fresh dashboard has no rows until something happens.
        let snapshot = database.snapshot().expect("snapshot");
        assert!(snapshot.workspace_items.is_empty());
        assert!(snapshot.activity_events.is_empty());

        database
            .append_activity_event("first real event")
            .expect("append event");
        let snapshot = database.snapshot().expect("snapshot");
        assert!(snapshot.workspace_items.is_empty());
        assert_eq!(snapshot.activity_events.len(), 1);
        assert_eq!(snapshot.activity_events[0].message, "first real event");

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn migration_registry_is_strictly_ordered_and_ends_at_latest() {
        let mut previous = 0_i64;
        for (version, _apply) in MIGRATIONS {
            assert!(
                *version > previous,
                "migration versions must be strictly increasing: {version} after {previous}"
            );
            previous = *version;
        }
        assert_eq!(previous, LATEST_SCHEMA_VERSION);
    }

    #[test]
    fn fresh_database_migrates_to_latest_version_with_full_schema() {
        let database_path = temp_database_path("fresh_migrates_to_latest");
        let database = Database::open(database_path.clone()).expect("open database");

        // Only registry versions are recorded (no legacy 1..=12 backfill), in
        // registry order, ending at the latest.
        assert_eq!(
            applied_versions(&database),
            MIGRATIONS
                .iter()
                .map(|(version, _)| *version)
                .collect::<Vec<_>>()
        );

        for table in ["chats", "change_sets", "feature_routes", "tool_calls"] {
            assert!(table_exists(&database, table), "missing table `{table}`");
        }
        let connection = database.connect().expect("connect");
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index'
                 AND name IN ('idx_change_files_before_hash', 'idx_change_files_after_hash')",
                [],
                |row| row.get(0),
            )
            .expect("query indexes");
        assert_eq!(index_count, 2, "v14 hash indexes should exist");
        drop(connection);

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn legacy_database_with_old_migration_rows_upgrades_cleanly() {
        let database_path = temp_database_path("legacy_full_upgrade");
        let database = Database::open(database_path.clone()).expect("open database");
        let project = create_project(&database, &database_path, "legacy_full");
        let chat = database
            .create_chat(&project.id, None)
            .expect("create chat")
            .chat;

        // Simulate a database written by the legacy migration path: same schema
        // (the baseline IS the legacy DDL), but schema_migrations holds the full
        // 1..=13 row set it used to insert.
        {
            let connection = database.connect().expect("connect");
            connection
                .execute("DELETE FROM schema_migrations", [])
                .expect("clear versions");
            for version in 1..=13_i64 {
                connection
                    .execute(
                        "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                        params![version, current_timestamp()],
                    )
                    .expect("insert legacy version");
            }
        }
        drop(database);

        // Reopen: only versions newer than 13 run; data is untouched.
        let reopened = Database::open(database_path.clone()).expect("reopen database");
        let versions = applied_versions(&reopened);
        assert_eq!(versions, (1..=15_i64).collect::<Vec<_>>());
        let projects = reopened.list_projects().expect("projects").projects;
        assert!(projects.iter().any(|entry| entry.id == project.id));
        let restored = reopened.get_chat(&chat.id, 50).expect("chat survives");
        assert_eq!(restored.chat.id, chat.id);
        drop(reopened);

        // A second reopen is a pure no-op.
        let reopened = Database::open(database_path.clone()).expect("reopen again");
        assert_eq!(
            applied_versions(&reopened),
            (1..=15_i64).collect::<Vec<_>>()
        );

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn partially_migrated_legacy_database_is_brought_up_to_date() {
        let database_path = temp_database_path("legacy_partial_upgrade");
        let database = Database::open(database_path.clone()).expect("open database");
        let project = create_project(&database, &database_path, "legacy_partial");

        // Simulate an older install: only versions 1..=5 recorded, and a table
        // (plus the v14 indexes) from later versions missing entirely.
        {
            let connection = database.connect().expect("connect");
            connection
                .execute_batch(
                    "
                    DELETE FROM schema_migrations;
                    DROP TABLE feature_routes;
                    DROP INDEX idx_change_files_before_hash;
                    DROP INDEX idx_change_files_after_hash;
                    ",
                )
                .expect("rewind schema");
            for version in 1..=5_i64 {
                connection
                    .execute(
                        "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                        params![version, current_timestamp()],
                    )
                    .expect("insert legacy version");
            }
        }
        assert!(!table_exists(&database, "feature_routes"));
        drop(database);

        // Reopen: the baseline re-runs (guarded DDL restores the missing
        // table), then v14/v15 — and the existing rows survive.
        let reopened = Database::open(database_path.clone()).expect("reopen database");
        assert_eq!(applied_versions(&reopened), vec![1, 2, 3, 4, 5, 13, 14, 15]);
        assert!(table_exists(&reopened, "feature_routes"));
        let projects = reopened.list_projects().expect("projects").projects;
        assert!(projects.iter().any(|entry| entry.id == project.id));

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn change_journal_retention_defaults_and_round_trips() {
        let database_path = temp_database_path("change_journal_retention");
        let database = Database::open(database_path.clone()).expect("open database");

        // Unset → the default (counted in messages, not change sets).
        assert_eq!(database.change_journal_retention().expect("default"), 10);

        assert_eq!(
            database
                .set_change_journal_retention(7)
                .expect("set retention"),
            7
        );
        assert_eq!(database.change_journal_retention().expect("read"), 7);

        // 0 (= unlimited) is a valid stored value, and it survives a reopen.
        database
            .set_change_journal_retention(0)
            .expect("set unlimited");
        drop(database);
        let reopened = Database::open(database_path.clone()).expect("reopen database");
        assert_eq!(reopened.change_journal_retention().expect("read"), 0);

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn typed_tool_event_round_trips_payload_and_artifact() {
        let database_path = temp_database_path("typed_tool_roundtrip");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let project = create_project(&database, &database_path, "typed_roundtrip");
        let run = database
            .begin_chat_run(None, Some(&project.id), "edit a file", None, false)
            .expect("begin run");

        // Mirror the production sink: legacy feed + typed storage for one event.
        let completed = ToolExecutionEvent {
            tool_call_id: "tc_edit_1".to_string(),
            run_id: Some(run.run_id.clone()),
            project_id: Some(project.id.clone()),
            command: None,
            kind: ToolExecutionEventKind::Completed,
            message: Some("modified a.txt".to_string()),
            tool_kind: Some(ToolKind::EditFile),
            payload: Some(serde_json::json!({
                "path": "a.txt", "status": "modified", "sha256": "abc123"
            })),
            touched_paths: vec!["a.txt".to_string()],
            artifacts: vec![ToolArtifact {
                artifact_id: "diff".to_string(),
                kind: "diff".to_string(),
                content_type: "text/x-diff".to_string(),
                preview: "@@ -1 +1 @@\n-old\n+new\n".to_string(),
                log_ref: Some("spill://full-diff".to_string()),
                size_bytes: 4096,
                sha256: None,
                truncated: true,
            }],
            ..Default::default()
        };
        database
            .record_chat_tool_execution_event(&run.chat.id, &run.assistant_message.id, &completed)
            .expect("legacy feed");
        database
            .record_typed_tool_event(&run.chat.id, &run.assistant_message.id, &completed)
            .expect("typed storage");

        drop(database);
        let reopened = Database::open(database_path.clone()).expect("reopen database");
        let conversation = reopened.get_chat(&run.chat.id, 200).expect("restore");

        assert_eq!(conversation.tool_executions.len(), 1);
        let tool = &conversation.tool_executions[0];
        assert_eq!(tool.tool_kind, Some(ToolKind::EditFile));
        let payload = tool
            .payload
            .as_ref()
            .expect("typed payload survives reload");
        assert_eq!(payload["path"], "a.txt");
        assert_eq!(payload["status"], "modified");
        assert_eq!(tool.artifacts.len(), 1);
        let artifact = &tool.artifacts[0];
        assert_eq!(artifact.artifact_id, "diff");
        assert_eq!(artifact.log_ref.as_deref(), Some("spill://full-diff"));
        assert!(artifact.truncated);
        assert_eq!(artifact.size_bytes, 4096);

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn run_command_typed_payload_is_synthesized_from_result() {
        let database_path = temp_database_path("typed_run_command");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let project = create_project(&database, &database_path, "typed_cmd");
        let run = database
            .begin_chat_run(None, Some(&project.id), "check status", None, false)
            .expect("begin run");
        let command = ToolCommand::new("git", ["status"]);

        // run_command carries no explicit payload; the storage layer synthesizes
        // it (and the output artifact) from command + result.
        let completed = ToolExecutionEvent {
            tool_call_id: "tc_cmd_1".to_string(),
            run_id: Some(run.run_id.clone()),
            project_id: Some(project.id.clone()),
            command: Some(command.clone()),
            kind: ToolExecutionEventKind::Completed,
            result: Some(ToolExecutionResult {
                tool_call_id: "tc_cmd_1".to_string(),
                status: ToolExecutionStatus::Completed,
                exit_code: Some(0),
                stdout_preview: "nothing to commit\n".to_string(),
                stderr_preview: String::new(),
                stdout_tail: "nothing to commit\n".to_string(),
                stderr_tail: String::new(),
                stdout_bytes: 18,
                stderr_bytes: 0,
                truncated_for_display: false,
                truncated_for_agent: false,
                log_ref: Some("spill://stdout".to_string()),
                message: None,
            }),
            tool_kind: Some(ToolKind::RunCommand),
            ..Default::default()
        };
        database
            .record_chat_tool_execution_event(&run.chat.id, &run.assistant_message.id, &completed)
            .expect("legacy feed");
        database
            .record_typed_tool_event(&run.chat.id, &run.assistant_message.id, &completed)
            .expect("typed storage");

        drop(database);
        let reopened = Database::open(database_path.clone()).expect("reopen");
        let conversation = reopened.get_chat(&run.chat.id, 200).expect("restore");
        let tool = &conversation.tool_executions[0];
        assert_eq!(tool.tool_kind, Some(ToolKind::RunCommand));
        let payload = tool.payload.as_ref().expect("synthesized payload");
        assert_eq!(payload["program"], "git");
        assert_eq!(payload["exitCode"], 0);
        // The output is referenced as an artifact (logRef), never inlined.
        assert_eq!(tool.artifacts.len(), 1);
        assert_eq!(tool.artifacts[0].artifact_id, "stdout");
        assert_eq!(tool.artifacts[0].log_ref.as_deref(), Some("spill://stdout"));

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn branch_preserves_typed_tool_data() {
        let database_path = temp_database_path("branch_typed");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .set_selected_llm_model("openai", "test-model")
            .expect("select model");
        let project = create_project(&database, &database_path, "branch_typed");
        let run = database
            .begin_chat_run(None, Some(&project.id), "edit a file", None, false)
            .expect("begin run");

        let completed = ToolExecutionEvent {
            tool_call_id: "tool_typed_branch".to_string(),
            run_id: Some(run.run_id.clone()),
            project_id: Some(project.id.clone()),
            kind: ToolExecutionEventKind::Completed,
            message: Some("modified a.txt".to_string()),
            tool_kind: Some(ToolKind::EditFile),
            payload: Some(serde_json::json!({ "path": "a.txt", "status": "modified" })),
            artifacts: vec![ToolArtifact {
                artifact_id: "diff".to_string(),
                kind: "diff".to_string(),
                content_type: "text/x-diff".to_string(),
                preview: "@@\n-old\n+new\n".to_string(),
                log_ref: None,
                size_bytes: 12,
                sha256: None,
                truncated: false,
            }],
            ..Default::default()
        };
        database
            .record_chat_tool_execution_event(&run.chat.id, &run.assistant_message.id, &completed)
            .expect("legacy feed");
        database
            .record_typed_tool_event(&run.chat.id, &run.assistant_message.id, &completed)
            .expect("typed storage");
        database
            .complete_chat_run(&run.run_id, &run.chat.id, &run.assistant_message.id)
            .expect("complete run");

        let branch = database
            .branch_chat_from_message(&run.chat.id, &run.assistant_message.id)
            .expect("branch chat");

        assert_eq!(branch.tool_executions.len(), 1);
        let tool = &branch.tool_executions[0];
        assert_ne!(
            tool.tool_call_id, "tool_typed_branch",
            "branch remaps tool_call_id"
        );
        // The whole point: typed data survives the branch, not just the legacy feed.
        assert_eq!(tool.tool_kind, Some(ToolKind::EditFile));
        let payload = tool
            .payload
            .as_ref()
            .expect("typed payload survives branch");
        assert_eq!(payload["path"], "a.txt");
        assert_eq!(tool.artifacts.len(), 1);
        assert_eq!(tool.artifacts[0].artifact_id, "diff");

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
