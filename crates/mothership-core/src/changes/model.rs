//! Domain model for the Workspace Change Journal.
//!
//! These are Core-owned types: the source of truth for *what the agent changed*
//! and *how to revert it*. They are independent of the model tool-output channel
//! and of any frontend payload. The wire DTOs (the `*Summary` types, the diff
//! window, and the event) serialize to `camelCase` so the thin desktop/phone
//! clients consume them directly — mirroring the rest of [`crate::ipc`].

use serde::{Deserialize, Serialize};

/// What happened to a single file inside a change set. Serialized as the single
/// letters the UI contract uses (`A`/`M`/`D`/`R`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeOp {
    #[serde(rename = "A")]
    Added,
    #[serde(rename = "M")]
    Modified,
    #[serde(rename = "D")]
    Deleted,
    #[serde(rename = "R")]
    Renamed,
}

impl ChangeOp {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeOp::Added => "A",
            ChangeOp::Modified => "M",
            ChangeOp::Deleted => "D",
            ChangeOp::Renamed => "R",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "A" => Some(ChangeOp::Added),
            "M" => Some(ChangeOp::Modified),
            "D" => Some(ChangeOp::Deleted),
            "R" => Some(ChangeOp::Renamed),
            _ => None,
        }
    }
}

/// Lifecycle status of a change set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSetStatus {
    /// The change is applied and present in the workspace.
    Active,
    /// The change was rolled back to its before-state.
    Reverted,
    /// A previously reverted change was re-applied.
    Restored,
    /// A revert/restore could not run safely because the workspace diverged.
    Conflicted,
    /// The change references history that no longer exists.
    Stale,
}

impl ChangeSetStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeSetStatus::Active => "active",
            ChangeSetStatus::Reverted => "reverted",
            ChangeSetStatus::Restored => "restored",
            ChangeSetStatus::Conflicted => "conflicted",
            ChangeSetStatus::Stale => "stale",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "active" => Some(ChangeSetStatus::Active),
            "reverted" => Some(ChangeSetStatus::Reverted),
            "restored" => Some(ChangeSetStatus::Restored),
            "conflicted" => Some(ChangeSetStatus::Conflicted),
            "stale" => Some(ChangeSetStatus::Stale),
            _ => None,
        }
    }
}

/// Why a single file could not be reverted/restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictReason {
    /// The file on disk no longer matches the state the agent produced — the
    /// user (or another process) edited it after the change. Reverting would
    /// silently discard that edit, so it is refused.
    CurrentHashMismatch,
    /// The file the revert expected to find is gone.
    MissingFile,
    /// A file the revert expected to be absent is present.
    UnexpectedFile,
    /// The OS refused the write/delete.
    PermissionDenied,
    /// The path resolved outside the project workspace.
    OutsideWorkspace,
    /// The before/after snapshot blob needed for restore is unavailable
    /// (e.g. the file was too large to capture).
    MissingSnapshot,
}

impl ConflictReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictReason::CurrentHashMismatch => "current_hash_mismatch",
            ConflictReason::MissingFile => "missing_file",
            ConflictReason::UnexpectedFile => "unexpected_file",
            ConflictReason::PermissionDenied => "permission_denied",
            ConflictReason::OutsideWorkspace => "outside_workspace",
            ConflictReason::MissingSnapshot => "missing_snapshot",
        }
    }
}

/// Status of a single revert attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevertStatus {
    Started,
    Completed,
    Conflicted,
    Failed,
}

impl RevertStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RevertStatus::Started => "started",
            RevertStatus::Completed => "completed",
            RevertStatus::Conflicted => "conflicted",
            RevertStatus::Failed => "failed",
        }
    }
}

/// Per-file summary as seen by clients (the expanded change-set view).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeFileSummary {
    pub id: String,
    pub path: String,
    pub old_path: Option<String>,
    pub op: ChangeOp,
    pub additions: u32,
    pub deletions: u32,
    pub is_binary: bool,
    pub is_large: bool,
}

/// A change-set summary (collapsed headline + expanded file list) for clients.
/// This is what the Dashboard renders — never reconstructed from tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSetSummary {
    pub id: String,
    pub status: ChangeSetStatus,
    pub chat_id: Option<String>,
    pub message_id: Option<String>,
    pub run_id: Option<String>,
    pub tool_call_id: Option<String>,
    /// The originating tool reported a failure but still left changes on disk.
    pub tool_failed: bool,
    pub file_count: u32,
    pub additions: u32,
    pub deletions: u32,
    pub files: Vec<ChangeFileSummary>,
    pub created_at: String,
    pub updated_at: String,
}

/// A lazily-loaded window of a single file's unified diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeFileDiff {
    pub change_file_id: String,
    pub path: String,
    pub op: ChangeOp,
    pub is_binary: bool,
    pub is_large: bool,
    /// Unified-diff lines (hunk headers + `+`/`-`/` ` prefixed lines) for the
    /// requested window.
    pub lines: Vec<String>,
    /// Index of the first returned line within the full diff.
    pub offset: u64,
    /// Total number of lines in the full diff.
    pub total_lines: u64,
    pub additions: u32,
    pub deletions: u32,
    /// True when no textual diff can be shown (binary, too large, or the
    /// snapshot blobs are unavailable).
    pub unavailable: bool,
}

/// A single conflict surfaced by a refused revert/restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeConflict {
    pub path: String,
    pub reason: ConflictReason,
    pub expected_hash: Option<String>,
    pub actual_hash: Option<String>,
    pub details: Option<String>,
}

/// The outcome of a revert command: the updated change-set summary plus any
/// conflicts. When `reverted` is false the workspace was left untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevertOutcome {
    pub change_set: ChangeSetSummary,
    pub conflicts: Vec<ChangeConflict>,
    pub reverted: bool,
}

/// Kinds of change-set lifecycle event pushed to subscribed clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSetEventKind {
    Created,
    Updated,
    Reverted,
    Restored,
    Conflicted,
}

/// A streamed change-set update carrying the full summary so clients can render
/// without a follow-up query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSetEvent {
    pub kind: ChangeSetEventKind,
    pub summary: ChangeSetSummary,
}

/// The run/chat/tool identity a change set is attributed to. Internal (not a
/// wire type): captured when a mutating tool runs.
#[derive(Debug, Clone)]
pub struct ChangeContext {
    pub project_id: Option<String>,
    pub run_id: Option<String>,
    pub chat_id: String,
    pub message_id: String,
    pub tool_call_id: String,
}

/// A persisted change-set header row (internal projection used to build a
/// [`ChangeSetSummary`]).
#[derive(Debug, Clone)]
pub struct ChangeSetRow {
    pub id: String,
    pub project_id: Option<String>,
    pub run_id: Option<String>,
    pub chat_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub status: ChangeSetStatus,
    pub tool_failed: bool,
    pub file_count: u32,
    pub additions: u32,
    pub deletions: u32,
    pub created_at: String,
    pub updated_at: String,
}

/// A persisted change-file row, including the snapshot hashes the revert engine
/// needs (these are deliberately kept out of [`ChangeFileSummary`], which the UI
/// receives).
#[derive(Debug, Clone)]
pub struct StoredChangeFile {
    pub id: String,
    pub path: String,
    pub old_path: Option<String>,
    pub op: ChangeOp,
    pub additions: u32,
    pub deletions: u32,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub is_binary: bool,
    pub is_large: bool,
}

impl StoredChangeFile {
    pub fn summary(&self) -> ChangeFileSummary {
        ChangeFileSummary {
            id: self.id.clone(),
            path: self.path.clone(),
            old_path: self.old_path.clone(),
            op: self.op,
            additions: self.additions,
            deletions: self.deletions,
            is_binary: self.is_binary,
            is_large: self.is_large,
        }
    }
}

/// A change-file ready to be persisted (built by the service from a capture).
#[derive(Debug, Clone)]
pub struct NewChangeFile {
    pub path: String,
    pub old_path: Option<String>,
    pub op: ChangeOp,
    pub additions: u32,
    pub deletions: u32,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub is_binary: bool,
    pub is_large: bool,
}

/// Build a client summary from a header row and its files.
pub(crate) fn summary_from(row: &ChangeSetRow, files: &[StoredChangeFile]) -> ChangeSetSummary {
    ChangeSetSummary {
        id: row.id.clone(),
        status: row.status,
        chat_id: row.chat_id.clone(),
        message_id: row.message_id.clone(),
        run_id: row.run_id.clone(),
        tool_call_id: row.tool_call_id.clone(),
        tool_failed: row.tool_failed,
        file_count: row.file_count,
        additions: row.additions,
        deletions: row.deletions,
        files: files.iter().map(StoredChangeFile::summary).collect(),
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
    }
}
