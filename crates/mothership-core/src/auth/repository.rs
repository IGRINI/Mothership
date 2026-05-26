use rusqlite::{params, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};

use crate::{Database, MothershipError, Result};

use super::{
    AuthMethodId, AuthNextAction, AuthSession, AuthSessionId, CredentialRecord, CredentialRecordId,
    CredentialRef, ProviderAuthRepository, ProviderConnection, ProviderConnectionId, ProviderId,
    VaultHandle,
};

impl ProviderAuthRepository for Database {
    fn save_auth_session(&self, session: &AuthSession) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "
            INSERT INTO auth_sessions (
                id,
                provider_id,
                auth_method_id,
                mode,
                status,
                authorization_url,
                user_code,
                verification_uri,
                message,
                expires_at,
                provider_metadata_json,
                created_at,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                authorization_url = excluded.authorization_url,
                user_code = excluded.user_code,
                verification_uri = excluded.verification_uri,
                message = excluded.message,
                expires_at = excluded.expires_at,
                provider_metadata_json = excluded.provider_metadata_json,
                updated_at = excluded.updated_at
            ",
            params![
                session.id.as_str(),
                session.provider_id.as_str(),
                session.auth_method_id.as_str(),
                to_db_string(&session.mode)?,
                to_db_string(&session.status)?,
                session.next_action.authorization_url.as_deref(),
                session.next_action.user_code.as_deref(),
                session.next_action.verification_uri.as_deref(),
                session.next_action.message.as_deref(),
                session.expires_at.as_deref(),
                serde_json::to_string(&session.provider_metadata)?,
                session.created_at,
                session.updated_at,
            ],
        )?;
        Ok(())
    }

    fn get_auth_session(&self, id: &AuthSessionId) -> Result<AuthSession> {
        let connection = self.connect()?;
        connection
            .query_row(
                "
                SELECT
                    id,
                    provider_id,
                    auth_method_id,
                    mode,
                    status,
                    authorization_url,
                    user_code,
                    verification_uri,
                    message,
                    expires_at,
                    provider_metadata_json,
                    created_at,
                    updated_at
                FROM auth_sessions
                WHERE id = ?1
                ",
                params![id.as_str()],
                read_auth_session,
            )
            .optional()?
            .ok_or_else(|| not_found("auth session", id.as_str()))
    }

    fn save_provider_connection(&self, connection: &ProviderConnection) -> Result<()> {
        let sqlite = self.connect()?;
        sqlite.execute(
            "
            INSERT INTO provider_connections (
                id,
                provider_id,
                auth_method_id,
                status,
                account_label,
                account_email,
                scopes_json,
                capabilities_json,
                credential_record_id,
                vault_handle,
                expires_at,
                created_at,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                account_label = excluded.account_label,
                account_email = excluded.account_email,
                scopes_json = excluded.scopes_json,
                capabilities_json = excluded.capabilities_json,
                credential_record_id = excluded.credential_record_id,
                vault_handle = excluded.vault_handle,
                expires_at = excluded.expires_at,
                updated_at = excluded.updated_at
            ",
            params![
                connection.id.as_str(),
                connection.provider_id.as_str(),
                connection.auth_method_id.as_str(),
                to_db_string(&connection.status)?,
                connection.account_label.as_deref(),
                connection.account_email.as_deref(),
                serde_json::to_string(&connection.scopes)?,
                serde_json::to_string(&connection.capabilities)?,
                connection.credential_ref.record_id.as_str(),
                connection.credential_ref.vault_handle.as_str(),
                connection.expires_at.as_deref(),
                connection.created_at,
                connection.updated_at,
            ],
        )?;
        Ok(())
    }

    fn save_credential_record(&self, record: &CredentialRecord) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "
            INSERT INTO credential_records (
                id,
                connection_id,
                credential_kind,
                vault_handle,
                expires_at,
                fingerprint_hash,
                status,
                created_at,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(id) DO UPDATE SET
                credential_kind = excluded.credential_kind,
                vault_handle = excluded.vault_handle,
                expires_at = excluded.expires_at,
                fingerprint_hash = excluded.fingerprint_hash,
                status = excluded.status,
                updated_at = excluded.updated_at
            ",
            params![
                record.id.as_str(),
                record.connection_id.as_str(),
                to_db_string(&record.credential_kind)?,
                record.vault_handle.as_str(),
                record.expires_at.as_deref(),
                record.fingerprint_hash.as_deref(),
                to_db_string(&record.status)?,
                record.created_at,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    fn list_provider_connections(&self) -> Result<Vec<ProviderConnection>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "
            SELECT
                id,
                provider_id,
                auth_method_id,
                status,
                account_label,
                account_email,
                scopes_json,
                capabilities_json,
                credential_record_id,
                vault_handle,
                expires_at,
                created_at,
                updated_at
            FROM provider_connections
            WHERE status <> 'disconnected'
            ORDER BY updated_at DESC, id ASC
            ",
        )?;

        let rows = statement.query_map([], read_provider_connection)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn get_provider_connection(&self, id: &ProviderConnectionId) -> Result<ProviderConnection> {
        let connection = self.connect()?;
        connection
            .query_row(
                "
                SELECT
                    id,
                    provider_id,
                    auth_method_id,
                    status,
                    account_label,
                    account_email,
                    scopes_json,
                    capabilities_json,
                    credential_record_id,
                    vault_handle,
                    expires_at,
                    created_at,
                    updated_at
                FROM provider_connections
                WHERE id = ?1
                ",
                params![id.as_str()],
                read_provider_connection,
            )
            .optional()?
            .ok_or_else(|| not_found("provider connection", id.as_str()))
    }

    fn mark_provider_connection_disconnected(&self, id: &ProviderConnectionId) -> Result<()> {
        let connection = self.connect()?;
        let changed = connection.execute(
            "
            UPDATE provider_connections
            SET status = 'disconnected',
                updated_at = strftime('%s', 'now')
            WHERE id = ?1
            ",
            params![id.as_str()],
        )?;

        if changed == 0 {
            return Err(not_found("provider connection", id.as_str()));
        }

        connection.execute(
            "
            UPDATE credential_records
            SET status = 'deleted',
                updated_at = strftime('%s', 'now')
            WHERE connection_id = ?1
            ",
            params![id.as_str()],
        )?;

        Ok(())
    }
}

fn read_auth_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuthSession> {
    let provider_metadata_json: String = row.get(10)?;
    Ok(AuthSession {
        id: AuthSessionId::from(row.get::<_, String>(0)?),
        provider_id: ProviderId::from(row.get::<_, String>(1)?),
        auth_method_id: AuthMethodId::from(row.get::<_, String>(2)?),
        mode: from_db_string(row.get::<_, String>(3)?)?,
        status: from_db_string(row.get::<_, String>(4)?)?,
        next_action: AuthNextAction {
            authorization_url: row.get(5)?,
            user_code: row.get(6)?,
            verification_uri: row.get(7)?,
            message: row.get(8)?,
        },
        expires_at: row.get(9)?,
        provider_metadata: serde_json::from_str(&provider_metadata_json).map_err(to_sql_error)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

fn read_provider_connection(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProviderConnection> {
    let scopes_json: String = row.get(6)?;
    let capabilities_json: String = row.get(7)?;
    Ok(ProviderConnection {
        id: ProviderConnectionId::from(row.get::<_, String>(0)?),
        provider_id: ProviderId::from(row.get::<_, String>(1)?),
        auth_method_id: AuthMethodId::from(row.get::<_, String>(2)?),
        status: from_db_string(row.get::<_, String>(3)?)?,
        account_label: row.get(4)?,
        account_email: row.get(5)?,
        scopes: serde_json::from_str(&scopes_json).map_err(to_sql_error)?,
        capabilities: serde_json::from_str(&capabilities_json).map_err(to_sql_error)?,
        credential_ref: CredentialRef {
            record_id: CredentialRecordId::from(row.get::<_, String>(8)?),
            vault_handle: VaultHandle::from(row.get::<_, String>(9)?),
        },
        expires_at: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

fn to_db_string<T: Serialize>(value: &T) -> Result<String> {
    let value = serde_json::to_value(value)?;
    value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
        MothershipError::InvalidRequest("expected string enum serialization".to_string())
    })
}

fn from_db_string<T: DeserializeOwned>(value: String) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(value)).map_err(to_sql_error)
}

fn to_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

fn not_found(entity: &str, id: &str) -> MothershipError {
    MothershipError::InvalidRequest(format!("{entity} not found: {id}"))
}
