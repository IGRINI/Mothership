//! Workspace Change Journal.
//!
//! A Core-owned product capability: it records *what the agent changed* in the
//! project workspace, lets the user review per-file diffs, and reverts changes
//! with conflict checks — independent of the model tool-output channel.
//!
//! Source of truth is Core, not frontend payloads or `tool_artifacts`. The flow
//! per mutating tool is:
//!
//! 1. [`ChangeRecorder::begin_capture`] wraps the tool's filesystem in a
//!    [`CaptureFileSystem`] that records the before-bytes of every touched path.
//! 2. The tool runs and mutates files as usual.
//! 3. [`ChangeRecorder::record`] finalizes the before/after snapshot immediately,
//!    then a bounded background worker diffs before→after, stores blobs in the
//!    [`SnapshotBlobStore`], persists a [`ChangeSetSummary`], and emits a
//!    `change_set.created` event.
//!
//! Revert ([`ChangesService::revert`]) is conflict-aware: it refuses to overwrite
//! a file the user edited after the agent's change. Storage is swappable behind
//! [`SnapshotBlobStore`]; the conflict rules and status transitions stay in Core.

mod blob_store;
mod capture;
pub(crate) mod diff;
mod model;
mod repository;
mod service;

pub use blob_store::{sha256_hex, FileBlobStore, SnapshotBlobStore};
pub use capture::{CaptureFileSystem, CapturedContent, CapturedPath};
pub use model::{
    ChangeConflict, ChangeContext, ChangeFileDiff, ChangeFileSummary, ChangeOp, ChangeSetEvent,
    ChangeSetEventKind, ChangeSetStatus, ChangeSetSummary, ConflictReason, RevertOutcome,
    RevertStatus,
};
pub use service::{ChangeEventSink, ChangeRecorder, ChangesService, NoopChangeEventSink};

#[cfg(test)]
mod tests;
