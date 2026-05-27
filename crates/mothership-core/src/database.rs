use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, types::Type, Connection, OptionalExtension};

use crate::llm::{LlmModelCatalogCache, LlmModelCatalogRepository};
use crate::{
    id::generate_id, ActivityEvent, ChatConversation, ChatMessage, ChatMessageRole,
    ChatMessageStatus, ChatRunEvent, ChatRunEventKind, ChatThreadSummary, DashboardMetric,
    DashboardSnapshot, LlmChatMessage, LlmChatRole, LlmModel, MothershipError, Result,
    SelectedLlmModel, SendChatMessageResult, SidecarStatus, WorkspaceItem,
};

const WORKSPACE_LIMIT: i64 = 2_500;
const EVENT_LIMIT: i64 = 5_000;
const CHAT_LIST_LIMIT: i64 = 100;
const CHAT_MESSAGE_LIMIT: i64 = 200;
const CHAT_MESSAGE_MAX_BYTES: usize = 20_000;
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

        Ok(ChatConversation { chat, messages })
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
        };
        let mut assistant_message = ChatMessage {
            id: generate_id("chat_message")?,
            chat_id: chat.id.clone(),
            position: 0,
            role: ChatMessageRole::Assistant,
            content: String::new(),
            status: ChatMessageStatus::Sending,
            created_at: now.clone(),
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

    pub fn list_llm_models(&self) -> Result<Vec<LlmModel>> {
        let connection = self.connect()?;
        let registry = crate::llm::default_llm_registry();
        let bundled = registry.list_bundled_models();
        let provider_ids = bundled
            .iter()
            .map(|model| model.provider_id.clone())
            .collect::<BTreeSet<_>>();
        let mut models = Vec::with_capacity(bundled.len());

        for provider_id in provider_ids {
            let model_management = registry
                .settings_schema(&provider_id)
                .map(|schema| schema.model_management.kind)
                .unwrap_or(crate::llm::ConnectorModelManagementKind::FixedCatalog);

            match model_management {
                crate::llm::ConnectorModelManagementKind::RemoteCatalog => {
                    if let Some(cache) = select_llm_model_catalog_cache(&connection, &provider_id)?
                        .filter(|cache| cache.is_fresh() && !cache.models.is_empty())
                    {
                        models.extend(cache.models);
                    }
                }
                crate::llm::ConnectorModelManagementKind::FixedCatalog
                | crate::llm::ConnectorModelManagementKind::EditableList => {
                    models.extend(
                        bundled
                            .iter()
                            .filter(|model| model.provider_id == provider_id)
                            .cloned(),
                    );
                }
            }
        }

        Ok(models)
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

impl LlmModelCatalogRepository for Database {
    fn load_llm_model_catalog_cache(
        &self,
        provider_id: &str,
    ) -> Result<Option<LlmModelCatalogCache>> {
        validate_identifier("provider_id", provider_id)?;

        let connection = self.connect()?;
        select_llm_model_catalog_cache(&connection, provider_id)
    }

    fn save_llm_model_catalog_cache(&self, cache: &LlmModelCatalogCache) -> Result<()> {
        validate_identifier("provider_id", &cache.provider_id)?;

        let connection = self.connect()?;
        connection.execute(
            "
            INSERT INTO llm_model_catalog_cache (
                provider_id,
                models_json,
                etag,
                fetched_at,
                expires_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(provider_id) DO UPDATE SET
                models_json = excluded.models_json,
                etag = excluded.etag,
                fetched_at = excluded.fetched_at,
                expires_at = excluded.expires_at
            ",
            params![
                cache.provider_id,
                serde_json::to_string(&cache.models)?,
                cache.etag.as_deref(),
                cache.fetched_at,
                cache.expires_at,
            ],
        )?;

        Ok(())
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

        CREATE TABLE IF NOT EXISTS auth_sessions (
            id TEXT PRIMARY KEY,
            provider_id TEXT NOT NULL,
            auth_method_id TEXT NOT NULL,
            mode TEXT NOT NULL,
            status TEXT NOT NULL,
            authorization_url TEXT,
            user_code TEXT,
            verification_uri TEXT,
            message TEXT,
            expires_at TEXT,
            provider_metadata_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_auth_sessions_provider_status
            ON auth_sessions (provider_id, status);

        CREATE TABLE IF NOT EXISTS provider_connections (
            id TEXT PRIMARY KEY,
            provider_id TEXT NOT NULL,
            auth_method_id TEXT NOT NULL,
            status TEXT NOT NULL,
            account_label TEXT,
            account_email TEXT,
            scopes_json TEXT NOT NULL,
            capabilities_json TEXT NOT NULL,
            credential_record_id TEXT NOT NULL,
            vault_handle TEXT NOT NULL,
            expires_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_provider_connections_provider_status
            ON provider_connections (provider_id, status);

        CREATE TABLE IF NOT EXISTS credential_records (
            id TEXT PRIMARY KEY,
            connection_id TEXT NOT NULL,
            credential_kind TEXT NOT NULL,
            vault_handle TEXT NOT NULL,
            expires_at TEXT,
            fingerprint_hash TEXT,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_credential_records_connection
            ON credential_records (connection_id);

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
            FOREIGN KEY(chat_id) REFERENCES chats(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_chat_messages_chat
            ON chat_messages (chat_id);

        CREATE TABLE IF NOT EXISTS llm_model_preferences (
            scope TEXT PRIMARY KEY,
            provider_id TEXT NOT NULL,
            model_id TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS llm_model_catalog_cache (
            provider_id TEXT PRIMARY KEY,
            models_json TEXT NOT NULL,
            etag TEXT,
            fetched_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
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

fn select_chat_messages(
    connection: &Connection,
    chat_id: &str,
    limit: i64,
) -> Result<Vec<ChatMessage>> {
    let mut statement = connection.prepare(
        "
        SELECT id, chat_id, rowid, role, content, status, created_at
        FROM (
            SELECT rowid, id, chat_id, role, content, status, created_at
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

fn insert_chat_message(connection: &Connection, message: &ChatMessage) -> Result<i64> {
    connection.execute(
        "
        INSERT INTO chat_messages (id, chat_id, role, content, status, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ",
        params![
            message.id,
            message.chat_id,
            chat_role_to_db(message.role),
            message.content,
            chat_status_to_db(message.status),
            message.created_at
        ],
    )?;

    Ok(connection.last_insert_rowid())
}

fn select_chat_message_by_id(connection: &Connection, message_id: &str) -> Result<ChatMessage> {
    connection
        .query_row(
            "
            SELECT id, chat_id, rowid, role, content, status, created_at
            FROM chat_messages
            WHERE id = ?1
            ",
            params![message_id],
            |row| {
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
                })
            },
        )
        .optional()?
        .ok_or_else(|| {
            MothershipError::InvalidRequest(format!("chat message not found: {message_id}"))
        })
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

fn select_llm_model_catalog_cache(
    connection: &Connection,
    provider_id: &str,
) -> Result<Option<LlmModelCatalogCache>> {
    let raw = connection
        .query_row(
            "
            SELECT provider_id, models_json, etag, fetched_at, expires_at
            FROM llm_model_catalog_cache
            WHERE provider_id = ?1
            ",
            params![provider_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;

    let Some((provider_id, models_json, etag, fetched_at, expires_at)) = raw else {
        return Ok(None);
    };

    let models = match serde_json::from_str(&models_json) {
        Ok(models) => models,
        Err(_) => return Ok(None),
    };

    Ok(Some(LlmModelCatalogCache {
        provider_id,
        models,
        etag,
        fetched_at,
        expires_at,
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
        ChatMessageStatus::Failed => "failed",
        ChatMessageStatus::Sending => "sending",
    }
}

fn chat_status_from_db(value: &str) -> rusqlite::Result<ChatMessageStatus> {
    match value {
        "complete" => Ok(ChatMessageStatus::Complete),
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

    #[test]
    fn builtin_model_list_is_empty() {
        let database_path = temp_database_path("remote_catalog_models_are_empty_without_cache");
        let database = Database::open(database_path.clone()).expect("open database");

        assert!(database.list_llm_models().expect("list models").is_empty());

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
