//! The change-journal application service: capture → diff → persist, plus
//! conflict-aware revert and the read queries.
//!
//! This is where the *domain behavior* lives (the "key design risk" the plan
//! calls out): status transitions, the per-op revert rules, and conflict
//! detection. Snapshot storage ([`SnapshotBlobStore`]) is injected and swappable;
//! the policy here does not change if the backend does.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;

use crate::tools::{FileSystem, Workspace};
use crate::{Database, MothershipError, Result};

use super::blob_store::SnapshotBlobStore;
use super::capture::{CaptureFileSystem, CapturedPath};
use super::diff;
use super::model::{
    ChangeConflict, ChangeContext, ChangeFileDiff, ChangeFileSummary, ChangeOp, ChangeSetEvent,
    ChangeSetEventKind, ChangeSetStatus, ChangeSetSummary, ConflictReason, NewChangeFile,
    RevertOutcome, RevertStatus, StoredChangeFile,
};
use super::repository as repo;

/// A file larger than this on either side is tracked but not text-diffed (its
/// `+N -M` shows as 0 and the per-file view reports it as unavailable). It is
/// still snapshotted up to the capture cap, so revert remains possible.
const DIFF_MAX_BYTES: usize = 1_000_000;
const PARALLEL_RECORD_MIN_FILES: usize = 4;
const PARALLEL_RECORD_MAX_THREADS: usize = 4;
const CHANGE_RECORD_QUEUE_CAPACITY: usize = 16;

/// Captures, persists, queries, and reverts workspace change sets. Cheap to
/// clone (a [`Database`] handle plus an `Arc` to the blob store).
#[derive(Clone)]
pub struct ChangesService {
    db: Database,
    blobs: Arc<dyn SnapshotBlobStore>,
}

impl ChangesService {
    pub fn new(db: Database, blobs: Arc<dyn SnapshotBlobStore>) -> Self {
        Self { db, blobs }
    }

    /// Build a change set from a finished capture session. Stores before/after
    /// blobs, computes per-file diff stats, and persists the set. Returns `None`
    /// when nothing actually changed (so no empty change set is shown).
    pub fn record(
        &self,
        ctx: &ChangeContext,
        capture: &CaptureFileSystem,
        tool_failed: bool,
    ) -> Result<Option<ChangeSetSummary>> {
        let captured = capture.finalize();
        self.record_captured(ctx, captured, tool_failed)
    }

    /// Build a change set from an already-finalized capture. This is the heavy
    /// half of recording and is safe to run off the tool execution path because
    /// the before/after bytes are owned by `captured`.
    pub fn record_captured(
        &self,
        ctx: &ChangeContext,
        captured: Vec<CapturedPath>,
        tool_failed: bool,
    ) -> Result<Option<ChangeSetSummary>> {
        let files = self.build_change_files(captured)?;
        if files.is_empty() {
            return Ok(None);
        }
        let summary = repo::create_change_set(&self.db, ctx, tool_failed, files)?;
        // Retention runs after every successful record. A pruning hiccup must
        // not fail the journal entry that was just persisted.
        if let Err(error) = self.prune_project(ctx.project_id.as_deref()) {
            eprintln!("change journal: failed to prune old change sets: {error}");
        }
        Ok(Some(summary))
    }

    /// Enforce the retention policy for one project scope: keep every change
    /// set belonging to the newest N MESSAGES (N = the persisted
    /// `change_journal_retention` setting; 0 = unlimited — counted in messages
    /// because one agent message can record hundreds of sets), delete the
    /// rest, then garbage-collect snapshot blobs no longer referenced by any
    /// remaining change file. Blob removal is best-effort per hash — a leaked
    /// blob is recoverable noise, a failed prune is not.
    pub fn prune_project(&self, project_id: Option<&str>) -> Result<()> {
        let retention = self.db.change_journal_retention()?;
        if retention == 0 {
            return Ok(());
        }
        let orphaned = repo::prune_change_sets(&self.db, project_id, retention)?;
        for hash in orphaned {
            if let Err(error) = self.blobs.remove(&hash) {
                eprintln!(
                    "change journal: failed to remove orphaned snapshot blob {hash}: {error}"
                );
            }
        }
        Ok(())
    }

    /// Convert one captured path into a persistable change file, or `None` when
    /// it is a no-op (created-then-deleted, or modified back to identical bytes).
    fn build_change_file(&self, path: CapturedPath) -> Result<Option<NewChangeFile>> {
        build_change_file_with_blobs(self.blobs.as_ref(), path)
    }

    /// Convert captured paths into persistable files. For broad patches this is
    /// the expensive part (hash/blob writes + line counts), so it runs through a
    /// small bounded thread fan-out before the single SQLite transaction.
    fn build_change_files(&self, mut captured: Vec<CapturedPath>) -> Result<Vec<NewChangeFile>> {
        captured.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));

        if captured.len() < PARALLEL_RECORD_MIN_FILES {
            let mut files = Vec::with_capacity(captured.len());
            for path in captured {
                if let Some(file) = self.build_change_file(path)? {
                    files.push(file);
                }
            }
            return Ok(files);
        }

        let workers = thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .min(PARALLEL_RECORD_MAX_THREADS)
            .min(captured.len());
        if workers <= 1 {
            let mut files = Vec::with_capacity(captured.len());
            for path in captured {
                if let Some(file) = self.build_change_file(path)? {
                    files.push(file);
                }
            }
            return Ok(files);
        }

        let mut chunks = (0..workers).map(|_| Vec::new()).collect::<Vec<_>>();
        for (index, path) in captured.into_iter().enumerate() {
            chunks[index % workers].push((index, path));
        }

        let blobs = Arc::clone(&self.blobs);
        let mut indexed = thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for chunk in chunks {
                let blobs = Arc::clone(&blobs);
                handles.push(scope.spawn(move || {
                    let mut out = Vec::with_capacity(chunk.len());
                    for (index, path) in chunk {
                        if let Some(file) = build_change_file_with_blobs(blobs.as_ref(), path)? {
                            out.push((index, file));
                        }
                    }
                    Ok::<_, MothershipError>(out)
                }));
            }

            let mut merged = Vec::new();
            for handle in handles {
                let files = handle.join().map_err(|_| {
                    MothershipError::Runtime(
                        "change journal worker panicked while recording files".to_string(),
                    )
                })??;
                merged.extend(files);
            }
            Ok::<_, MothershipError>(merged)
        })?;

        indexed.sort_by_key(|(index, _file)| *index);
        Ok(indexed.into_iter().map(|(_index, file)| file).collect())
    }

    /// Change sets attributed to one assistant message.
    pub fn message_summaries(&self, message_id: &str) -> Result<Vec<ChangeSetSummary>> {
        repo::summaries_for_message(&self.db, message_id)
    }

    /// Change sets across a whole chat (to hydrate a reopened conversation).
    pub fn chat_summaries(&self, chat_id: &str) -> Result<Vec<ChangeSetSummary>> {
        repo::summaries_for_chat(&self.db, chat_id)
    }

    /// A page of a change set's files (the "show more" beyond a summary's inline
    /// preview). `limit = 0` returns all files from `offset`.
    pub fn list_change_set_files(
        &self,
        change_set_id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<ChangeFileSummary>> {
        repo::list_change_files(&self.db, change_set_id, offset, limit)
    }

    /// The project a change set belongs to. The revert command needs this to
    /// resolve the workspace root (and its containment policy) before touching
    /// any file. `None` when the set is unknown or has no associated project.
    pub fn change_set_project_id(&self, change_set_id: &str) -> Result<Option<String>> {
        Ok(repo::load_change_set_detail(&self.db, change_set_id)?
            .and_then(|(row, _files)| row.project_id))
    }

    /// A lazily-loaded window of a single file's unified diff. With `full`, the
    /// diff is rendered with whole-file context (every unchanged line kept) so the
    /// caller can show the entire file with its edits in place, rather than just
    /// the changed hunks.
    pub fn file_diff(
        &self,
        change_file_id: &str,
        offset: u64,
        limit: u64,
        full: bool,
    ) -> Result<ChangeFileDiff> {
        let file = repo::load_change_file(&self.db, change_file_id)?.ok_or_else(|| {
            MothershipError::InvalidRequest(format!("unknown change file: {change_file_id}"))
        })?;

        let unavailable_template = |file: &StoredChangeFile| ChangeFileDiff {
            change_file_id: file.id.clone(),
            path: file.path.clone(),
            op: file.op,
            is_binary: file.is_binary,
            is_large: file.is_large,
            lines: Vec::new(),
            offset: 0,
            total_lines: 0,
            additions: file.additions,
            deletions: file.deletions,
            unavailable: true,
        };

        if file.is_binary || file.is_large {
            return Ok(unavailable_template(&file));
        }

        let before = self.load_optional_blob(file.before_hash.as_deref())?;
        let after = self.load_optional_blob(file.after_hash.as_deref())?;
        let snapshots_present = file.before_hash.as_ref().map_or(true, |_| before.is_some())
            && file.after_hash.as_ref().map_or(true, |_| after.is_some());
        if !snapshots_present {
            return Ok(unavailable_template(&file));
        }

        let before_text = content_string(before.as_deref());
        let after_text = content_string(after.as_deref());
        // `full` keeps every unchanged line as context (whole file); otherwise the
        // standard 3-line-context hunks.
        let unified = if full {
            diff::unified_diff_with_context(&before_text, &after_text, usize::MAX)
        } else {
            diff::unified_diff(&before_text, &after_text)
        };
        let total = unified.lines.len() as u64;
        let start = offset.min(total) as usize;
        let end = if limit == 0 {
            unified.lines.len()
        } else {
            (start + limit as usize).min(unified.lines.len())
        };

        Ok(ChangeFileDiff {
            change_file_id: file.id,
            path: file.path,
            op: file.op,
            is_binary: file.is_binary,
            is_large: file.is_large,
            lines: unified.lines[start..end].to_vec(),
            offset: start as u64,
            total_lines: total,
            additions: unified.additions,
            deletions: unified.deletions,
            unavailable: false,
        })
    }

    /// Revert a change set, conflict-aware. Every affected file is checked
    /// against the state the agent produced *before any write happens*; if any
    /// file diverged (the user edited it since), nothing is overwritten and the
    /// conflicts are recorded. Otherwise the before-state is restored.
    pub fn revert(
        &self,
        change_set_id: &str,
        workspace: &Workspace,
        fs: &dyn FileSystem,
    ) -> Result<RevertOutcome> {
        let (row, files) =
            repo::load_change_set_detail(&self.db, change_set_id)?.ok_or_else(|| {
                MothershipError::InvalidRequest(format!("unknown change set: {change_set_id}"))
            })?;

        // Idempotent: reverting an already-reverted set changes nothing.
        if row.status == ChangeSetStatus::Reverted {
            return Ok(RevertOutcome {
                change_set: self.summary_or_err(change_set_id)?,
                conflicts: Vec::new(),
                reverted: true,
            });
        }

        // Phase 1: validate all files and gather restore plans (with bytes). A
        // file already at its before-state yields no plan (idempotent).
        let mut conflicts = Vec::new();
        let mut plans = Vec::new();
        for file in &files {
            match self.plan_revert(file, workspace, fs) {
                Ok(Some(plan)) => plans.push(plan),
                Ok(None) => {}
                Err(conflict) => conflicts.push(conflict),
            }
        }

        if !conflicts.is_empty() {
            let revert_id = repo::insert_revert(
                &self.db,
                change_set_id,
                RevertStatus::Conflicted,
                Some("revert refused: workspace diverged from the recorded change"),
            )?;
            repo::insert_conflicts(&self.db, &revert_id, &conflicts)?;
            repo::update_change_set_status(&self.db, change_set_id, ChangeSetStatus::Conflicted)?;
            return Ok(RevertOutcome {
                change_set: self.summary_or_err(change_set_id)?,
                conflicts,
                reverted: false,
            });
        }

        // Phase 2: apply with all-or-nothing semantics. Before mutating each file
        // we capture an undo action from its current on-disk state; if any
        // write/delete fails, the already-applied plans are rolled back so the
        // workspace is left exactly as it was before the revert, and the change
        // set stays `active`. (`write_atomic` is atomic, so a failed write leaves
        // its own target unchanged — only prior, succeeded plans need undoing.)
        let revert_id = repo::insert_revert(&self.db, change_set_id, RevertStatus::Started, None)?;
        let mut undo: Vec<RestorePlan> = Vec::with_capacity(plans.len());
        for plan in &plans {
            let undo_action = match capture_undo(plan.path(), plan.rel_path(), fs) {
                Ok(action) => action,
                Err(error) => {
                    rollback(&undo, fs);
                    repo::complete_revert(&self.db, &revert_id, RevertStatus::Failed)?;
                    return Err(MothershipError::Runtime(format!(
                        "revert aborted before {}: {error}; workspace left unchanged",
                        plan.rel_path()
                    )));
                }
            };
            if let Err(error) = plan.apply(fs) {
                rollback(&undo, fs);
                repo::complete_revert(&self.db, &revert_id, RevertStatus::Failed)?;
                return Err(MothershipError::Runtime(format!(
                    "revert failed at {}: {error}; rolled back",
                    plan.rel_path()
                )));
            }
            undo.push(undo_action);
        }
        repo::complete_revert(&self.db, &revert_id, RevertStatus::Completed)?;
        repo::update_change_set_status(&self.db, change_set_id, ChangeSetStatus::Reverted)?;

        Ok(RevertOutcome {
            change_set: self.summary_or_err(change_set_id)?,
            conflicts: Vec::new(),
            reverted: true,
        })
    }

    /// Plan the revert of one file. Returns `Ok(None)` when the file is already
    /// at its before-state (idempotent — no write needed), `Ok(Some(plan))` to
    /// apply, or the conflict that blocks it. A three-way current-state read keeps
    /// an existing-but-unreadable file from being mistaken for an absent one.
    fn plan_revert(
        &self,
        file: &StoredChangeFile,
        workspace: &Workspace,
        fs: &dyn FileSystem,
    ) -> std::result::Result<Option<RestorePlan>, ChangeConflict> {
        let resolved = match workspace.resolve(&file.path) {
            Ok(path) => path,
            Err(_) => {
                return Err(conflict(
                    file,
                    ConflictReason::OutsideWorkspace,
                    None,
                    None,
                    Some("path resolves outside the workspace"),
                ))
            }
        };
        let state = current_state(&resolved, fs);

        match file.op {
            ChangeOp::Added => match &state {
                // The agent-added file is already gone — deleting it is a no-op.
                CurrentState::Missing => Ok(None),
                CurrentState::Present(hash)
                    if Some(hash.as_str()) == file.after_hash.as_deref() =>
                {
                    Ok(Some(RestorePlan::Delete {
                        path: resolved,
                        rel: file.path.clone(),
                    }))
                }
                CurrentState::Unreadable => Err(conflict(
                    file,
                    ConflictReason::PermissionDenied,
                    file.after_hash.clone(),
                    None,
                    Some("current file could not be read to verify the change"),
                )),
                CurrentState::Present(_) => Err(conflict(
                    file,
                    ConflictReason::CurrentHashMismatch,
                    file.after_hash.clone(),
                    state.hash(),
                    None,
                )),
            },
            ChangeOp::Modified | ChangeOp::Renamed => {
                let Some(before_hash) = file.before_hash.clone() else {
                    return Err(conflict(
                        file,
                        ConflictReason::MissingSnapshot,
                        None,
                        state.hash(),
                        Some("before snapshot unavailable"),
                    ));
                };
                match &state {
                    // Already at before-state — nothing to do.
                    CurrentState::Present(hash) if *hash == before_hash => Ok(None),
                    CurrentState::Present(hash)
                        if Some(hash.as_str()) == file.after_hash.as_deref() =>
                    {
                        let bytes = self.load_blob(&before_hash).map_err(|_| {
                            conflict(
                                file,
                                ConflictReason::MissingSnapshot,
                                Some(before_hash.clone()),
                                state.hash(),
                                Some("before snapshot blob missing"),
                            )
                        })?;
                        Ok(Some(RestorePlan::Write {
                            path: resolved,
                            rel: file.path.clone(),
                            bytes,
                        }))
                    }
                    CurrentState::Missing => Err(conflict(
                        file,
                        ConflictReason::MissingFile,
                        file.after_hash.clone(),
                        None,
                        Some("the modified file is gone"),
                    )),
                    CurrentState::Unreadable => Err(conflict(
                        file,
                        ConflictReason::PermissionDenied,
                        file.after_hash.clone(),
                        None,
                        Some("current file could not be read to verify the change"),
                    )),
                    CurrentState::Present(_) => Err(conflict(
                        file,
                        ConflictReason::CurrentHashMismatch,
                        file.after_hash.clone(),
                        state.hash(),
                        None,
                    )),
                }
            }
            ChangeOp::Deleted => {
                let Some(before_hash) = file.before_hash.clone() else {
                    return Err(conflict(
                        file,
                        ConflictReason::MissingSnapshot,
                        None,
                        state.hash(),
                        Some("before snapshot unavailable"),
                    ));
                };
                match &state {
                    // The deleted file is still absent — recreate it.
                    CurrentState::Missing => {
                        let bytes = self.load_blob(&before_hash).map_err(|_| {
                            conflict(
                                file,
                                ConflictReason::MissingSnapshot,
                                Some(before_hash.clone()),
                                None,
                                Some("before snapshot blob missing"),
                            )
                        })?;
                        Ok(Some(RestorePlan::Write {
                            path: resolved,
                            rel: file.path.clone(),
                            bytes,
                        }))
                    }
                    // Already back at its before-content — nothing to do.
                    CurrentState::Present(hash) if *hash == before_hash => Ok(None),
                    CurrentState::Unreadable => Err(conflict(
                        file,
                        ConflictReason::UnexpectedFile,
                        None,
                        None,
                        Some("an unreadable file is present where the change deleted one"),
                    )),
                    CurrentState::Present(_) => Err(conflict(
                        file,
                        ConflictReason::UnexpectedFile,
                        None,
                        state.hash(),
                        Some("a file reappeared where the change deleted one"),
                    )),
                }
            }
        }
    }

    fn summary_or_err(&self, change_set_id: &str) -> Result<ChangeSetSummary> {
        repo::load_change_set_summary(&self.db, change_set_id)?.ok_or_else(|| {
            MothershipError::InvalidRequest(format!("unknown change set: {change_set_id}"))
        })
    }

    fn load_blob(&self, hash: &str) -> Result<Vec<u8>> {
        self.blobs
            .get(hash)?
            .ok_or_else(|| MothershipError::Runtime(format!("snapshot blob missing: {hash}")))
    }

    fn load_optional_blob(&self, hash: Option<&str>) -> Result<Option<Vec<u8>>> {
        match hash {
            Some(hash) => Ok(self.blobs.get(hash)?),
            None => Ok(None),
        }
    }
}

fn build_change_file_with_blobs(
    blobs: &dyn SnapshotBlobStore,
    path: CapturedPath,
) -> Result<Option<NewChangeFile>> {
    let CapturedPath {
        rel_path,
        before,
        after,
    } = path;

    let op = match (before.existed, after.existed) {
        (false, true) => ChangeOp::Added,
        (true, true) => ChangeOp::Modified,
        (true, false) => ChangeOp::Deleted,
        // Created and removed within the same tool: net no change.
        (false, false) => return Ok(None),
    };

    let before_hash = match &before.bytes {
        Some(bytes) => Some(blobs.put(bytes)?),
        None => None,
    };
    let after_hash = match &after.bytes {
        Some(bytes) => Some(blobs.put(bytes)?),
        None => None,
    };

    // A "modify" whose content is byte-identical is not a change.
    if op == ChangeOp::Modified {
        if let (Some(before_hash), Some(after_hash)) = (&before_hash, &after_hash) {
            if before_hash == after_hash {
                return Ok(None);
            }
        }
    }

    let is_binary = before.bytes.as_deref().is_some_and(looks_binary)
        || after.bytes.as_deref().is_some_and(looks_binary);
    let too_large_bytes = before.too_large
        || after.too_large
        || before
            .bytes
            .as_ref()
            .is_some_and(|bytes| bytes.len() > DIFF_MAX_BYTES)
        || after
            .bytes
            .as_ref()
            .is_some_and(|bytes| bytes.len() > DIFF_MAX_BYTES);

    // Compute counts only for text files within the size *and* line caps; a
    // file over either cap is marked large (counts skipped, diff not rendered)
    // so neither the LCS table nor a naive whole-file diff is ever built.
    let (additions, deletions, is_large) = if is_binary || too_large_bytes {
        (0, 0, too_large_bytes)
    } else {
        let before_text = content_string(before.bytes.as_deref());
        let after_text = content_string(after.bytes.as_deref());
        if diff::exceeds_line_cap(&before_text) || diff::exceeds_line_cap(&after_text) {
            (0, 0, true)
        } else {
            let (additions, deletions) = diff::diff_counts(&before_text, &after_text);
            (additions, deletions, false)
        }
    };

    Ok(Some(NewChangeFile {
        path: rel_path,
        old_path: None,
        op,
        additions,
        deletions,
        before_hash,
        after_hash,
        is_binary,
        is_large,
    }))
}

/// One concrete restore action, with the bytes already loaded so phase 2 is
/// pure IO.
enum RestorePlan {
    Write {
        path: PathBuf,
        rel: String,
        bytes: Vec<u8>,
    },
    Delete {
        path: PathBuf,
        rel: String,
    },
}

impl RestorePlan {
    fn apply(&self, fs: &dyn FileSystem) -> io::Result<()> {
        match self {
            RestorePlan::Write { path, bytes, .. } => fs.write_atomic(path, bytes),
            RestorePlan::Delete { path, .. } => {
                if fs.exists(path) {
                    fs.remove_file(path)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn rel_path(&self) -> &str {
        match self {
            RestorePlan::Write { rel, .. } | RestorePlan::Delete { rel, .. } => rel,
        }
    }

    fn path(&self) -> &Path {
        match self {
            RestorePlan::Write { path, .. } | RestorePlan::Delete { path, .. } => path,
        }
    }
}

/// The three-way on-disk state of a path: a present-but-unreadable file must not
/// be confused with an absent one (an `.ok()` on the hash would do exactly that).
enum CurrentState {
    Missing,
    Present(String),
    Unreadable,
}

impl CurrentState {
    /// The content hash, if the file is present and readable.
    fn hash(&self) -> Option<String> {
        match self {
            CurrentState::Present(hash) => Some(hash.clone()),
            _ => None,
        }
    }
}

fn current_state(path: &Path, fs: &dyn FileSystem) -> CurrentState {
    if !fs.exists(path) {
        return CurrentState::Missing;
    }
    match fs.hash_file_sha256(path) {
        Ok(hash) => CurrentState::Present(hash),
        Err(_) => CurrentState::Unreadable,
    }
}

/// Capture an inverse action for a restore plan from the target's *current*
/// on-disk state, so a failed multi-file revert can be rolled back to exactly
/// where it started. Read before mutating, so it reflects the pre-apply state.
fn capture_undo(path: &Path, rel: &str, fs: &dyn FileSystem) -> io::Result<RestorePlan> {
    if fs.exists(path) {
        let bytes = fs.read(path)?;
        Ok(RestorePlan::Write {
            path: path.to_path_buf(),
            rel: rel.to_string(),
            bytes,
        })
    } else {
        Ok(RestorePlan::Delete {
            path: path.to_path_buf(),
            rel: rel.to_string(),
        })
    }
}

/// Best-effort replay of undo actions in reverse, to unwind a partially-applied
/// revert. Errors here are unrecoverable and intentionally swallowed — the call
/// site has already reported the original failure.
fn rollback(undo: &[RestorePlan], fs: &dyn FileSystem) {
    for action in undo.iter().rev() {
        let _ = action.apply(fs);
    }
}

fn conflict(
    file: &StoredChangeFile,
    reason: ConflictReason,
    expected_hash: Option<String>,
    actual_hash: Option<String>,
    details: Option<&str>,
) -> ChangeConflict {
    ChangeConflict {
        path: file.path.clone(),
        reason,
        expected_hash,
        actual_hash,
        details: details.map(str::to_string),
    }
}

fn content_string(bytes: Option<&[u8]>) -> String {
    match bytes {
        Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        None => String::new(),
    }
}

/// Heuristic binary detection: a NUL byte in the first 8 KiB.
fn looks_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(8192)];
    sample.contains(&0)
}

/// Sink for change-set lifecycle events, mirroring [`crate::ChatRunEventSink`].
/// The sidecar implements this to forward events over the wire protocol.
pub trait ChangeEventSink: Send + Sync {
    fn emit(&self, event: ChangeSetEvent);
}

/// A [`ChangeEventSink`] that drops events (tests, headless flows).
pub struct NoopChangeEventSink;

impl ChangeEventSink for NoopChangeEventSink {
    fn emit(&self, _event: ChangeSetEvent) {}
}

/// Per-run binding of a [`ChangesService`] to a chat/message context plus an
/// event sink. Constructed once per chat run; the file-tool runtime asks it to
/// wrap the filesystem and, after a mutating tool finishes, to record the result.
pub struct ChangeRecorder {
    service: ChangesService,
    events: Arc<dyn ChangeEventSink>,
    sender: Mutex<Option<SyncSender<ChangeRecordJob>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    project_id: Option<String>,
    chat_id: String,
    message_id: String,
}

struct ChangeRecordJob {
    ctx: ChangeContext,
    captured: Vec<CapturedPath>,
    tool_failed: bool,
}

impl ChangeRecordJob {
    fn new(ctx: ChangeContext, captured: Vec<CapturedPath>, tool_failed: bool) -> Self {
        Self {
            ctx,
            captured,
            tool_failed,
        }
    }
}

fn record_change_job(
    service: ChangesService,
    events: Arc<dyn ChangeEventSink>,
    job: ChangeRecordJob,
) {
    match service.record_captured(&job.ctx, job.captured, job.tool_failed) {
        Ok(Some(summary)) => events.emit(ChangeSetEvent {
            kind: ChangeSetEventKind::Created,
            summary,
        }),
        Ok(None) => {}
        Err(error) => {
            eprintln!("change journal: failed to record change set: {error}");
        }
    }
}

impl ChangeRecorder {
    pub fn new(
        service: ChangesService,
        events: Arc<dyn ChangeEventSink>,
        project_id: Option<String>,
        chat_id: String,
        message_id: String,
    ) -> Self {
        let (sender, receiver) = sync_channel::<ChangeRecordJob>(CHANGE_RECORD_QUEUE_CAPACITY);
        let worker_service = service.clone();
        let worker_events = Arc::clone(&events);
        let worker = match thread::Builder::new()
            .name("mothership-change-recorder".to_string())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    record_change_job(worker_service.clone(), Arc::clone(&worker_events), job);
                }
            }) {
            Ok(worker) => Some(worker),
            Err(error) => {
                eprintln!("change journal: failed to start background recorder: {error}");
                None
            }
        };
        let sender = if worker.is_some() { Some(sender) } else { None };

        Self {
            service,
            events,
            sender: Mutex::new(sender),
            worker: Mutex::new(worker),
            project_id,
            chat_id,
            message_id,
        }
    }

    /// Wrap a filesystem so the next tool's mutations are captured. `root` is the
    /// canonical workspace root (paths are journaled relative to it).
    pub fn begin_capture(
        &self,
        inner: Arc<dyn FileSystem>,
        root: impl Into<PathBuf>,
    ) -> CaptureFileSystem {
        CaptureFileSystem::new(inner, root)
    }

    /// Snapshot the change set produced by a finished mutating tool and enqueue
    /// the heavy persistence/diff work. Snapshotting stays synchronous so the
    /// captured "after" bytes cannot be invalidated by a later tool; persistence
    /// and event emission happen on a bounded background worker. Failures are
    /// logged, never propagated — a journal hiccup must not fail the tool.
    pub fn record(
        &self,
        run_id: Option<&str>,
        tool_call_id: &str,
        capture: &CaptureFileSystem,
        tool_failed: bool,
    ) {
        let ctx = ChangeContext {
            project_id: self.project_id.clone(),
            run_id: run_id.map(str::to_string),
            chat_id: self.chat_id.clone(),
            message_id: self.message_id.clone(),
            tool_call_id: tool_call_id.to_string(),
        };
        let job = ChangeRecordJob::new(ctx, capture.finalize(), tool_failed);
        let sender = {
            let guard = match self.sender.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.as_ref().cloned()
        };

        match sender {
            Some(sender) => {
                if let Err(error) = sender.send(job) {
                    record_change_job(self.service.clone(), Arc::clone(&self.events), error.0);
                }
            }
            None => record_change_job(self.service.clone(), Arc::clone(&self.events), job),
        }
    }
}

impl Drop for ChangeRecorder {
    fn drop(&mut self) {
        {
            let mut sender = match self.sender.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            sender.take();
        }

        let worker = {
            let mut worker = match self.worker.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            worker.take()
        };
        if let Some(worker) = worker {
            if worker.join().is_err() {
                eprintln!("change journal: background recorder panicked");
            }
        }
    }
}
