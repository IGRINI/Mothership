//! SQLite persistence for the change journal.
//!
//! Co-located with the rest of the `changes` module (rather than swelling the
//! main `database.rs`) but consistent with it: every function borrows a fresh
//! [`Connection`] from [`Database::connect`] and SQLite owns the metadata and
//! relationships. Large payloads (the actual before/after bytes) live in the blob
//! store, referenced here only by content hash.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};

use crate::id::generate_id;
use crate::{Database, Result};

use super::model::{
    summary_from, ChangeConflict, ChangeContext, ChangeFileSummary, ChangeOp, ChangeSetRow,
    ChangeSetStatus, ChangeSetSummary, NewChangeFile, RevertStatus, StoredChangeFile,
};

/// How many files a change-set *summary* carries inline (the preview). The rest
/// are paged via [`list_change_files`] so opening a chat with large patches stays
/// bounded — a summary never loads an unbounded file list.
const FILE_PREVIEW_LIMIT: i64 = 10;

fn now_ts() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
        .to_string()
}

/// Persist a new change set and its files in one transaction, returning the
/// client summary. `additions`/`deletions`/`file_count` are derived from `files`.
pub fn create_change_set(
    db: &Database,
    ctx: &ChangeContext,
    tool_failed: bool,
    files: Vec<NewChangeFile>,
) -> Result<ChangeSetSummary> {
    let file_count = files.len() as u32;
    let additions: u32 = files.iter().map(|file| file.additions).sum();
    let deletions: u32 = files.iter().map(|file| file.deletions).sum();
    let now = now_ts();
    let change_set_id = generate_id("change_set")?;

    let mut connection = db.connect()?;
    let tx = connection.transaction()?;
    tx.execute(
        "INSERT INTO change_sets (
            id, project_id, run_id, chat_id, message_id, tool_call_id,
            status, tool_failed, file_count, additions, deletions, created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            change_set_id,
            ctx.project_id.as_deref(),
            ctx.run_id.as_deref(),
            ctx.chat_id.as_str(),
            ctx.message_id.as_str(),
            ctx.tool_call_id.as_str(),
            ChangeSetStatus::Active.as_str(),
            tool_failed as i64,
            file_count as i64,
            additions as i64,
            deletions as i64,
            now,
            now,
        ],
    )?;

    let mut stored = Vec::with_capacity(files.len());
    for (position, file) in files.into_iter().enumerate() {
        let id = generate_id("change_file")?;
        tx.execute(
            "INSERT INTO change_files (
                id, change_set_id, path, old_path, op, additions, deletions,
                before_hash, after_hash, is_binary, is_large, position
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                id,
                change_set_id,
                file.path,
                file.old_path,
                file.op.as_str(),
                file.additions as i64,
                file.deletions as i64,
                file.before_hash,
                file.after_hash,
                file.is_binary as i64,
                file.is_large as i64,
                position as i64,
            ],
        )?;
        stored.push(StoredChangeFile {
            id,
            path: file.path,
            old_path: file.old_path,
            op: file.op,
            additions: file.additions,
            deletions: file.deletions,
            before_hash: file.before_hash,
            after_hash: file.after_hash,
            is_binary: file.is_binary,
            is_large: file.is_large,
        });
    }
    tx.commit()?;

    let row = ChangeSetRow {
        id: change_set_id,
        project_id: ctx.project_id.clone(),
        run_id: ctx.run_id.clone(),
        chat_id: Some(ctx.chat_id.clone()),
        message_id: Some(ctx.message_id.clone()),
        tool_call_id: Some(ctx.tool_call_id.clone()),
        status: ChangeSetStatus::Active,
        tool_failed,
        file_count,
        additions,
        deletions,
        created_at: now.clone(),
        updated_at: now,
    };
    // Return the same bounded preview the read paths use, so the `created` event
    // and a later hydration agree on the inline file list.
    let preview = &stored[..stored.len().min(FILE_PREVIEW_LIMIT as usize)];
    Ok(summary_from(&row, preview))
}

/// All change sets attributed to a single (assistant) message, oldest first.
pub fn summaries_for_message(db: &Database, message_id: &str) -> Result<Vec<ChangeSetSummary>> {
    let connection = db.connect()?;
    let rows = select_change_set_rows(
        &connection,
        "WHERE message_id = ?1 ORDER BY rowid ASC",
        params![message_id],
    )?;
    rows_to_summaries(&connection, rows)
}

/// All change sets in a chat, oldest first (used to hydrate a reopened chat).
pub fn summaries_for_chat(db: &Database, chat_id: &str) -> Result<Vec<ChangeSetSummary>> {
    let connection = db.connect()?;
    let rows = select_change_set_rows(
        &connection,
        "WHERE chat_id = ?1 ORDER BY rowid ASC",
        params![chat_id],
    )?;
    rows_to_summaries(&connection, rows)
}

/// A single change-set summary by id.
pub fn load_change_set_summary(
    db: &Database,
    change_set_id: &str,
) -> Result<Option<ChangeSetSummary>> {
    let connection = db.connect()?;
    let Some(row) = select_one_change_set_row(&connection, change_set_id)? else {
        return Ok(None);
    };
    let files = select_change_files_preview(&connection, change_set_id, FILE_PREVIEW_LIMIT)?;
    Ok(Some(summary_from(&row, &files)))
}

/// A page of a change set's files (offset/limit), for the UI "show more". A
/// `limit` of 0 returns all files from `offset`. Returns client summaries (no
/// snapshot hashes).
pub fn list_change_files(
    db: &Database,
    change_set_id: &str,
    offset: u64,
    limit: u64,
) -> Result<Vec<ChangeFileSummary>> {
    let connection = db.connect()?;
    let sql_limit: i64 = if limit == 0 { -1 } else { limit as i64 };
    let mut statement = connection.prepare(
        "SELECT id, change_set_id, path, old_path, op, additions, deletions,
                before_hash, after_hash, is_binary, is_large
         FROM change_files WHERE change_set_id = ?1 ORDER BY position ASC LIMIT ?2 OFFSET ?3",
    )?;
    let rows = statement.query_map(
        params![change_set_id, sql_limit, offset as i64],
        change_file_from_row,
    )?;
    let files = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(files.iter().map(StoredChangeFile::summary).collect())
}

/// A change set with its files including snapshot hashes (for the revert engine).
pub fn load_change_set_detail(
    db: &Database,
    change_set_id: &str,
) -> Result<Option<(ChangeSetRow, Vec<StoredChangeFile>)>> {
    let connection = db.connect()?;
    let Some(row) = select_one_change_set_row(&connection, change_set_id)? else {
        return Ok(None);
    };
    let files = select_change_files(&connection, change_set_id)?;
    Ok(Some((row, files)))
}

/// A single change file (with its parent set id) for the lazy diff view.
pub fn load_change_file(db: &Database, change_file_id: &str) -> Result<Option<StoredChangeFile>> {
    let connection = db.connect()?;
    connection
        .query_row(
            "SELECT id, change_set_id, path, old_path, op, additions, deletions,
                    before_hash, after_hash, is_binary, is_large
             FROM change_files WHERE id = ?1",
            params![change_file_id],
            change_file_from_row,
        )
        .optional()
        .map_err(Into::into)
}

/// Record a revert attempt, returning its id.
pub fn insert_revert(
    db: &Database,
    change_set_id: &str,
    status: RevertStatus,
    error: Option<&str>,
) -> Result<String> {
    let connection = db.connect()?;
    let id = generate_id("change_revert")?;
    let now = now_ts();
    let completed_at = matches!(
        status,
        RevertStatus::Completed | RevertStatus::Conflicted | RevertStatus::Failed
    )
    .then(|| now.clone());
    connection.execute(
        "INSERT INTO change_reverts (id, change_set_id, status, created_at, completed_at, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, change_set_id, status.as_str(), now, completed_at, error],
    )?;
    Ok(id)
}

/// Mark an existing revert attempt terminal.
pub fn complete_revert(db: &Database, revert_id: &str, status: RevertStatus) -> Result<()> {
    let connection = db.connect()?;
    connection.execute(
        "UPDATE change_reverts SET status = ?2, completed_at = ?3 WHERE id = ?1",
        params![revert_id, status.as_str(), now_ts()],
    )?;
    Ok(())
}

/// Persist the conflicts that blocked a revert.
pub fn insert_conflicts(
    db: &Database,
    revert_id: &str,
    conflicts: &[ChangeConflict],
) -> Result<()> {
    let mut connection = db.connect()?;
    let tx = connection.transaction()?;
    for conflict in conflicts {
        let id = generate_id("change_conflict")?;
        tx.execute(
            "INSERT INTO change_conflicts (id, revert_id, path, reason, expected_hash, actual_hash, details)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                revert_id,
                conflict.path,
                conflict.reason.as_str(),
                conflict.expected_hash,
                conflict.actual_hash,
                conflict.details,
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Enforce per-project retention: keep every change set belonging to the
/// newest `keep` MESSAGES for one project scope and delete the rest. An agent
/// run can record hundreds of sets for a single message, so retention counts
/// messages, not sets — all of a kept message's sets survive together. Sets
/// without a message id each count as their own unit. File rows, reverts, and
/// conflicts cascade with their set; returns the snapshot hashes no longer
/// referenced by ANY remaining change file — i.e. the blobs now safe to remove
/// from the store (content-addressed and shared across sets, so the orphan
/// check is global, not per set).
pub fn prune_change_sets(
    db: &Database,
    project_id: Option<&str>,
    keep: u32,
) -> Result<Vec<String>> {
    let mut connection = db.connect()?;
    let tx = connection.transaction()?;

    // Bucket sets by message (a NULL message id makes the set its own bucket),
    // rank buckets by their newest set, keep the newest `keep` buckets, and
    // make victims of everything in the older buckets.
    let victims: Vec<String> = {
        let mut statement = tx.prepare(
            "WITH scoped AS (
                 SELECT id, rowid,
                        COALESCE(message_id, 'set:' || id) AS bucket
                 FROM change_sets
                 WHERE (?1 IS NULL AND project_id IS NULL) OR project_id = ?1
             ),
             kept AS (
                 SELECT bucket FROM scoped
                 GROUP BY bucket
                 ORDER BY MAX(rowid) DESC
                 LIMIT ?2
             )
             SELECT id FROM scoped
             WHERE bucket NOT IN (SELECT bucket FROM kept)",
        )?;
        let rows = statement.query_map(params![project_id, keep as i64], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    if victims.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = (0..victims.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let victim_values = victims.into_iter().map(Value::Text).collect::<Vec<_>>();

    // Gather the snapshot hashes the victims reference BEFORE the delete (their
    // file rows cascade away with the sets).
    let mut candidates: HashSet<String> = HashSet::new();
    {
        let sql = format!(
            "SELECT before_hash, after_hash FROM change_files
             WHERE change_set_id IN ({placeholders})"
        );
        let mut statement = tx.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(victim_values.iter().cloned()), |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })?;
        for row in rows {
            let (before, after) = row?;
            candidates.extend(before);
            candidates.extend(after);
        }
    }

    tx.execute(
        &format!("DELETE FROM change_sets WHERE id IN ({placeholders})"),
        params_from_iter(victim_values),
    )?;

    // A candidate blob survives if any remaining file row still references it.
    let mut orphaned = Vec::with_capacity(candidates.len());
    {
        let mut statement = tx.prepare(
            "SELECT EXISTS(
                 SELECT 1 FROM change_files WHERE before_hash = ?1 OR after_hash = ?1
             )",
        )?;
        for hash in candidates {
            let referenced: i64 = statement.query_row(params![hash], |row| row.get(0))?;
            if referenced == 0 {
                orphaned.push(hash);
            }
        }
    }

    tx.commit()?;
    Ok(orphaned)
}

/// Transition a change set's status and bump `updated_at`.
pub fn update_change_set_status(
    db: &Database,
    change_set_id: &str,
    status: ChangeSetStatus,
) -> Result<()> {
    let connection = db.connect()?;
    connection.execute(
        "UPDATE change_sets SET status = ?2, updated_at = ?3 WHERE id = ?1",
        params![change_set_id, status.as_str(), now_ts()],
    )?;
    Ok(())
}

// --- row helpers -----------------------------------------------------------

const CHANGE_SET_COLUMNS: &str = "id, project_id, run_id, chat_id, message_id, tool_call_id,
     status, tool_failed, file_count, additions, deletions, created_at, updated_at";

fn select_change_set_rows(
    connection: &Connection,
    where_clause: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<ChangeSetRow>> {
    let sql = format!("SELECT {CHANGE_SET_COLUMNS} FROM change_sets {where_clause}");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params, change_set_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn select_one_change_set_row(
    connection: &Connection,
    change_set_id: &str,
) -> Result<Option<ChangeSetRow>> {
    let sql = format!("SELECT {CHANGE_SET_COLUMNS} FROM change_sets WHERE id = ?1");
    connection
        .query_row(&sql, params![change_set_id], change_set_from_row)
        .optional()
        .map_err(Into::into)
}

/// All files of a set, with snapshot hashes — used by the revert engine
/// ([`load_change_set_detail`]), which must see every file.
fn select_change_files(
    connection: &Connection,
    change_set_id: &str,
) -> Result<Vec<StoredChangeFile>> {
    let mut statement = connection.prepare(
        "SELECT id, change_set_id, path, old_path, op, additions, deletions,
                before_hash, after_hash, is_binary, is_large
         FROM change_files WHERE change_set_id = ?1 ORDER BY position ASC",
    )?;
    let rows = statement.query_map(params![change_set_id], change_file_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// First `limit` files of a set, for the summary preview. The rest are paged via
/// [`list_change_files`].
fn select_change_files_preview(
    connection: &Connection,
    change_set_id: &str,
    limit: i64,
) -> Result<Vec<StoredChangeFile>> {
    let mut statement = connection.prepare(
        "SELECT id, change_set_id, path, old_path, op, additions, deletions,
                before_hash, after_hash, is_binary, is_large
         FROM change_files WHERE change_set_id = ?1 ORDER BY position ASC LIMIT ?2",
    )?;
    let rows = statement.query_map(params![change_set_id, limit], change_file_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn rows_to_summaries(
    connection: &Connection,
    rows: Vec<ChangeSetRow>,
) -> Result<Vec<ChangeSetSummary>> {
    let mut previews = select_change_files_preview_for_sets(connection, &rows)?;
    let mut summaries = Vec::with_capacity(rows.len());
    for row in rows {
        let files = previews.remove(&row.id).unwrap_or_default();
        summaries.push(summary_from(&row, &files));
    }
    Ok(summaries)
}

/// Preview files for many change sets in one query. `position` is per-set, so a
/// simple `position < FILE_PREVIEW_LIMIT` predicate gives each set its own top-N
/// preview without an N+1 loop.
fn select_change_files_preview_for_sets(
    connection: &Connection,
    rows: &[ChangeSetRow],
) -> Result<HashMap<String, Vec<StoredChangeFile>>> {
    if rows.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = (0..rows.len()).map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id, change_set_id, path, old_path, op, additions, deletions,
                before_hash, after_hash, is_binary, is_large
         FROM change_files
         WHERE position < ? AND change_set_id IN ({placeholders})
         ORDER BY change_set_id ASC, position ASC"
    );

    let mut values = Vec::with_capacity(rows.len() + 1);
    values.push(Value::Integer(FILE_PREVIEW_LIMIT));
    values.extend(rows.iter().map(|row| Value::Text(row.id.clone())));

    let mut statement = connection.prepare(&sql)?;
    let files = statement.query_map(params_from_iter(values), change_file_with_set_from_row)?;
    let mut grouped: HashMap<String, Vec<StoredChangeFile>> = HashMap::with_capacity(rows.len());
    for file in files {
        let (change_set_id, file) = file?;
        grouped.entry(change_set_id).or_default().push(file);
    }
    Ok(grouped)
}

fn change_set_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChangeSetRow> {
    let status: String = row.get(6)?;
    Ok(ChangeSetRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        run_id: row.get(2)?,
        chat_id: row.get(3)?,
        message_id: row.get(4)?,
        tool_call_id: row.get(5)?,
        status: ChangeSetStatus::from_str(&status).unwrap_or(ChangeSetStatus::Active),
        tool_failed: row.get::<_, i64>(7)? != 0,
        file_count: row.get::<_, i64>(8)? as u32,
        additions: row.get::<_, i64>(9)? as u32,
        deletions: row.get::<_, i64>(10)? as u32,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

fn change_file_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredChangeFile> {
    let op: String = row.get(4)?;
    Ok(StoredChangeFile {
        id: row.get(0)?,
        // column 1 (change_set_id) is selected for query shape but not surfaced.
        path: row.get(2)?,
        old_path: row.get(3)?,
        op: ChangeOp::from_str(&op).unwrap_or(ChangeOp::Modified),
        additions: row.get::<_, i64>(5)? as u32,
        deletions: row.get::<_, i64>(6)? as u32,
        before_hash: row.get(7)?,
        after_hash: row.get(8)?,
        is_binary: row.get::<_, i64>(9)? != 0,
        is_large: row.get::<_, i64>(10)? != 0,
    })
}

fn change_file_with_set_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, StoredChangeFile)> {
    Ok((row.get(1)?, change_file_from_row(row)?))
}
