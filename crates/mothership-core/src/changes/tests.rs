//! Focused Core tests for the change journal: capture → diff → persist, the
//! per-op revert rules, and conflict detection. These use real temp dirs and the
//! `StdFileSystem`, exercising the same path resolution the live tools use.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::tools::{FileMetadata, FileSystem, StdFileSystem, Workspace};
use crate::Database;

use super::blob_store::{sha256_hex, FileBlobStore, SnapshotBlobStore};
use super::{
    CaptureFileSystem, ChangeContext, ChangeEventSink, ChangeOp, ChangeRecorder, ChangeSetStatus,
    ChangesService, ConflictReason, NoopChangeEventSink,
};

struct Harness {
    root: PathBuf,
    db: Database,
    blobs: Arc<FileBlobStore>,
    service: ChangesService,
    workspace: Workspace,
    fs: StdFileSystem,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn harness(label: &str) -> Harness {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("mothership_changes_{label}_{nanos}"));
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let database = Database::open(root.join("app.db")).unwrap();
    let blobs = Arc::new(FileBlobStore::new(root.join("blobs")));
    let service = ChangesService::new(database.clone(), blobs.clone());
    let workspace = Workspace::new(&project).unwrap();
    Harness {
        root,
        db: database,
        blobs,
        service,
        workspace,
        fs: StdFileSystem::new(),
    }
}

fn ctx(message_id: &str, tool_call_id: &str) -> ChangeContext {
    ctx_in(Some("project"), message_id, tool_call_id)
}

fn ctx_in(project_id: Option<&str>, message_id: &str, tool_call_id: &str) -> ChangeContext {
    ChangeContext {
        project_id: project_id.map(str::to_string),
        run_id: Some("run".to_string()),
        chat_id: "chat".to_string(),
        message_id: message_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
    }
}

/// Wrap the std filesystem, run `mutate` against the capture wrapper (as a tool
/// would), and return the wrapper for the service to record.
fn capture(h: &Harness, mutate: impl FnOnce(&CaptureFileSystem)) -> CaptureFileSystem {
    let wrapper = CaptureFileSystem::new(Arc::new(h.fs), h.workspace.root());
    mutate(&wrapper);
    wrapper
}

#[test]
fn recorder_flushes_background_jobs_on_drop() {
    let h = harness("recorder_flush");
    let path = h.workspace.resolve("async.txt").unwrap();
    let capture = capture(&h, |fs| {
        fs.write_atomic(&path, b"async\n").unwrap();
    });

    {
        let events: Arc<dyn ChangeEventSink> = Arc::new(NoopChangeEventSink);
        let recorder = ChangeRecorder::new(
            h.service.clone(),
            events,
            Some("project".to_string()),
            "chat".to_string(),
            "msg".to_string(),
        );
        recorder.record(Some("run"), "tool", &capture, false);
    }

    let summaries = h.service.message_summaries("msg").unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].file_count, 1);
    assert_eq!(summaries[0].files[0].path, "async.txt");
}

#[test]
fn modify_then_revert_restores_previous_content() {
    let h = harness("modify");
    let path = h.workspace.resolve("src/main.rs").unwrap();
    h.fs.write_atomic(&path, b"line1\nline2\n").unwrap();

    let cap = capture(&h, |fs| {
        fs.write_atomic(&path, b"line1\nCHANGED\nline3\n").unwrap();
    });
    let summary = h
        .service
        .record(&ctx("m1", "t1"), &cap, false)
        .unwrap()
        .expect("a change set is created");

    assert_eq!(summary.file_count, 1);
    assert_eq!(summary.files[0].op, ChangeOp::Modified);
    assert!(summary.additions >= 1 && summary.deletions >= 1);

    let outcome = h.service.revert(&summary.id, &h.workspace, &h.fs).unwrap();
    assert!(outcome.reverted);
    assert_eq!(outcome.change_set.status, ChangeSetStatus::Reverted);
    assert_eq!(h.fs.read(&path).unwrap(), b"line1\nline2\n");
}

#[test]
fn add_then_revert_deletes_the_file() {
    let h = harness("add");
    let path = h.workspace.resolve("new.txt").unwrap();

    let cap = capture(&h, |fs| {
        fs.write_atomic(&path, b"brand new\n").unwrap();
    });
    let summary = h
        .service
        .record(&ctx("m", "t"), &cap, false)
        .unwrap()
        .unwrap();
    assert_eq!(summary.files[0].op, ChangeOp::Added);
    assert!(h.fs.exists(&path));

    let outcome = h.service.revert(&summary.id, &h.workspace, &h.fs).unwrap();
    assert!(outcome.reverted);
    assert!(!h.fs.exists(&path));
}

#[test]
fn delete_then_revert_recreates_the_file() {
    let h = harness("delete");
    let path = h.workspace.resolve("notes.txt").unwrap();
    h.fs.write_atomic(&path, b"keep me\n").unwrap();

    let cap = capture(&h, |fs| {
        fs.remove_file(&path).unwrap();
    });
    let summary = h
        .service
        .record(&ctx("m", "t"), &cap, false)
        .unwrap()
        .unwrap();
    assert_eq!(summary.files[0].op, ChangeOp::Deleted);
    assert!(!h.fs.exists(&path));

    let outcome = h.service.revert(&summary.id, &h.workspace, &h.fs).unwrap();
    assert!(outcome.reverted);
    assert_eq!(h.fs.read(&path).unwrap(), b"keep me\n");
}

#[test]
fn revert_conflicts_when_file_changed_after_the_agent() {
    let h = harness("conflict");
    let path = h.workspace.resolve("a.txt").unwrap();
    h.fs.write_atomic(&path, b"v1\n").unwrap();

    let cap = capture(&h, |fs| {
        fs.write_atomic(&path, b"v2\n").unwrap();
    });
    let summary = h
        .service
        .record(&ctx("m", "t"), &cap, false)
        .unwrap()
        .unwrap();

    // The user edits the file after the agent's change.
    h.fs.write_atomic(&path, b"v3 from user\n").unwrap();

    let outcome = h.service.revert(&summary.id, &h.workspace, &h.fs).unwrap();
    assert!(!outcome.reverted);
    assert_eq!(outcome.conflicts.len(), 1);
    assert_eq!(
        outcome.conflicts[0].reason,
        ConflictReason::CurrentHashMismatch
    );
    assert_eq!(outcome.change_set.status, ChangeSetStatus::Conflicted);
    // The user's edit is preserved — nothing was overwritten.
    assert_eq!(h.fs.read(&path).unwrap(), b"v3 from user\n");
}

#[test]
fn identical_write_records_no_change_set() {
    let h = harness("nochange");
    let path = h.workspace.resolve("same.txt").unwrap();
    h.fs.write_atomic(&path, b"same\n").unwrap();

    let cap = capture(&h, |fs| {
        fs.write_atomic(&path, b"same\n").unwrap();
    });
    assert!(h
        .service
        .record(&ctx("m", "t"), &cap, false)
        .unwrap()
        .is_none());
}

#[test]
fn message_summaries_and_lazy_file_diff() {
    let h = harness("diff");
    let path = h.workspace.resolve("x.rs").unwrap();
    h.fs.write_atomic(&path, b"a\nb\nc\n").unwrap();

    let cap = capture(&h, |fs| {
        fs.write_atomic(&path, b"a\nB\nc\n").unwrap();
    });
    let summary = h
        .service
        .record(&ctx("msg-1", "t"), &cap, false)
        .unwrap()
        .unwrap();

    let list = h.service.message_summaries("msg-1").unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, summary.id);

    let diff = h
        .service
        .file_diff(&summary.files[0].id, 0, 1000, false)
        .unwrap();
    assert!(!diff.unavailable);
    assert!(diff.lines.iter().any(|line| line == "+B"));
    assert!(diff.lines.iter().any(|line| line == "-b"));

    // Whole-file context keeps every unchanged line as context, not just the hunk.
    let full = h
        .service
        .file_diff(&summary.files[0].id, 0, 0, true)
        .unwrap();
    assert!(full.lines.iter().any(|line| line == "+B"));
    assert!(full.lines.iter().any(|line| line == " a"));
    assert!(full.lines.iter().any(|line| line == " c"));
}

#[test]
fn binary_file_is_tracked_without_a_text_diff() {
    let h = harness("binary");
    let path = h.workspace.resolve("blob.bin").unwrap();

    let cap = capture(&h, |fs| {
        fs.write_atomic(&path, &[0u8, 1, 2, 3, 0, 9, 9]).unwrap();
    });
    let summary = h
        .service
        .record(&ctx("m", "t"), &cap, false)
        .unwrap()
        .unwrap();
    assert!(summary.files[0].is_binary);
    assert_eq!(summary.files[0].additions, 0);

    let diff = h
        .service
        .file_diff(&summary.files[0].id, 0, 100, false)
        .unwrap();
    assert!(diff.unavailable);
}

#[test]
fn failed_tool_with_partial_writes_still_records_a_change_set() {
    let h = harness("failed");
    let path = h.workspace.resolve("partial.txt").unwrap();

    let cap = capture(&h, |fs| {
        fs.write_atomic(&path, b"partial\n").unwrap();
    });
    let summary = h
        .service
        .record(&ctx("m", "t"), &cap, true)
        .unwrap()
        .unwrap();
    assert!(summary.tool_failed);
    assert_eq!(summary.file_count, 1);
}

/// A [`FileSystem`] that delegates to the real one but can simulate a write
/// failure or an existing-but-unreadable file at a given path suffix, so we can
/// exercise the revert rollback and tri-state detection paths deterministically.
struct ProbeFs {
    inner: StdFileSystem,
    fail_write: Option<String>,
    unreadable: Option<String>,
}

impl ProbeFs {
    fn matches(suffix: &Option<String>, path: &Path) -> bool {
        match suffix {
            Some(suffix) => path.to_string_lossy().replace('\\', "/").ends_with(suffix),
            None => false,
        }
    }
}

impl FileSystem for ProbeFs {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if Self::matches(&self.fail_write, path) {
            return Err(io::Error::other("simulated write failure"));
        }
        self.inner.write_atomic(path, bytes)
    }
    fn hash_file_sha256(&self, path: &Path) -> io::Result<String> {
        if Self::matches(&self.unreadable, path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "simulated unreadable file",
            ));
        }
        self.inner.hash_file_sha256(path)
    }
    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.inner.metadata(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir(path)
    }
}

#[test]
fn revert_rolls_back_when_a_later_file_write_fails() {
    let h = harness("rollback");
    let a = h.workspace.resolve("a.txt").unwrap();
    let b = h.workspace.resolve("b.txt").unwrap();
    h.fs.write_atomic(&a, b"a-before\n").unwrap();
    h.fs.write_atomic(&b, b"b-before\n").unwrap();

    let cap = capture(&h, |fs| {
        fs.write_atomic(&a, b"a-after\n").unwrap();
        fs.write_atomic(&b, b"b-after\n").unwrap();
    });
    let summary = h
        .service
        .record(&ctx("m", "t"), &cap, false)
        .unwrap()
        .unwrap();
    assert_eq!(summary.file_count, 2);

    // Revert with a filesystem that fails the write to b.txt. Whichever file is
    // applied first, the failure must leave BOTH files at their after-state.
    let probe = ProbeFs {
        inner: StdFileSystem::new(),
        fail_write: Some("b.txt".to_string()),
        unreadable: None,
    };
    let result = h.service.revert(&summary.id, &h.workspace, &probe);
    assert!(
        result.is_err(),
        "revert should fail when a file write fails"
    );

    assert_eq!(h.fs.read(&a).unwrap(), b"a-after\n");
    assert_eq!(h.fs.read(&b).unwrap(), b"b-after\n");

    // The change set stays active (nothing was reverted).
    let after = h.service.message_summaries("m").unwrap();
    assert_eq!(after[0].status, ChangeSetStatus::Active);
}

#[test]
fn deleted_revert_conflicts_when_path_is_present_but_unreadable() {
    let h = harness("unreadable");
    let path = h.workspace.resolve("x.txt").unwrap();
    h.fs.write_atomic(&path, b"original\n").unwrap();

    let cap = capture(&h, |fs| {
        fs.remove_file(&path).unwrap();
    });
    let summary = h
        .service
        .record(&ctx("m", "t"), &cap, false)
        .unwrap()
        .unwrap();
    assert_eq!(summary.files[0].op, ChangeOp::Deleted);

    // A file reappears where the change deleted one, but it cannot be read to
    // verify. Revert must refuse (not silently recreate over it).
    h.fs.write_atomic(&path, b"reappeared by someone else\n")
        .unwrap();
    let probe = ProbeFs {
        inner: StdFileSystem::new(),
        fail_write: None,
        unreadable: Some("x.txt".to_string()),
    };
    let outcome = h.service.revert(&summary.id, &h.workspace, &probe).unwrap();
    assert!(!outcome.reverted);
    assert_eq!(outcome.conflicts.len(), 1);
    assert_eq!(outcome.conflicts[0].reason, ConflictReason::UnexpectedFile);
    // The reappeared file was left untouched.
    assert_eq!(h.fs.read(&path).unwrap(), b"reappeared by someone else\n");
}

#[test]
fn summary_previews_files_and_pages_the_rest() {
    let h = harness("paging");
    let cap = capture(&h, |fs| {
        for index in 0..14 {
            let path = h.workspace.resolve(&format!("f{index:02}.txt")).unwrap();
            fs.write_atomic(&path, format!("file {index}\n").as_bytes())
                .unwrap();
        }
    });
    let summary = h
        .service
        .record(&ctx("m", "t"), &cap, false)
        .unwrap()
        .unwrap();

    // The summary reports the true count but only carries a bounded preview.
    assert_eq!(summary.file_count, 14);
    assert_eq!(summary.files.len(), 10);
    assert_eq!(summary.files[0].path, "f00.txt");
    assert_eq!(summary.files[9].path, "f09.txt");

    // Chat/message hydration is likewise bounded.
    let list = h.service.message_summaries("m").unwrap();
    assert_eq!(list[0].file_count, 14);
    assert_eq!(list[0].files.len(), 10);

    // The rest are paged on demand (offset past the preview, then a bounded page).
    let rest = h.service.list_change_set_files(&summary.id, 10, 0).unwrap();
    assert_eq!(rest.len(), 4);
    assert_eq!(rest[0].path, "f10.txt");
    assert_eq!(rest[3].path, "f13.txt");
    let page = h.service.list_change_set_files(&summary.id, 0, 5).unwrap();
    assert_eq!(page.len(), 5);
    assert_eq!(page[4].path, "f04.txt");

    // Revert still sees every file (detail load is unbounded by design).
    let outcome = h.service.revert(&summary.id, &h.workspace, &h.fs).unwrap();
    assert!(outcome.reverted);
    for index in 0..14 {
        let path = h.workspace.resolve(&format!("f{index:02}.txt")).unwrap();
        assert!(!h.fs.exists(&path));
    }
}

/// Record one added file with `content` for `message_id`/`tool_call_id` in the
/// given project scope, returning the stored after-hash.
fn record_added_file(
    h: &Harness,
    project_id: Option<&str>,
    message_id: &str,
    rel_path: &str,
    content: &[u8],
) -> String {
    let path = h.workspace.resolve(rel_path).unwrap();
    let cap = capture(h, |fs| {
        fs.write_atomic(&path, content).unwrap();
    });
    let summary = h
        .service
        .record(&ctx_in(project_id, message_id, message_id), &cap, false)
        .unwrap()
        .unwrap();
    assert_eq!(summary.files[0].op, ChangeOp::Added);
    sha256_hex(content)
}

#[test]
fn retention_prunes_oldest_sets_per_project() {
    let h = harness("retention_per_project");
    h.db.set_change_journal_retention(2).unwrap();

    let hash_a = record_added_file(&h, Some("project"), "m1", "a.txt", b"alpha\n");
    let hash_b = record_added_file(&h, Some("project"), "m2", "b.txt", b"beta\n");
    // Another project's only set must not count against (or be touched by)
    // this project's retention.
    let hash_other = record_added_file(&h, Some("other"), "m3", "o.txt", b"other\n");
    let hash_d = record_added_file(&h, Some("project"), "m4", "d.txt", b"delta\n");

    // "project" exceeded N=2: its oldest set (m1) is gone, the newest two stay.
    assert!(h.service.message_summaries("m1").unwrap().is_empty());
    assert_eq!(h.service.message_summaries("m2").unwrap().len(), 1);
    assert_eq!(h.service.message_summaries("m4").unwrap().len(), 1);
    assert_eq!(h.service.message_summaries("m3").unwrap().len(), 1);

    // The pruned set's blob is gone from disk; everything still referenced stays.
    assert!(!h.blobs.has(&hash_a));
    assert!(h.blobs.has(&hash_b));
    assert!(h.blobs.has(&hash_other));
    assert!(h.blobs.has(&hash_d));
}

#[test]
fn retention_keeps_blobs_shared_with_surviving_sets() {
    let h = harness("retention_shared_blob");
    h.db.set_change_journal_retention(1).unwrap();

    // Set 1 stores the shared content and a unique one; set 2 stores the same
    // shared bytes under another path (content-addressing dedups them into one
    // blob) plus its own unique file.
    let shared_path = h.workspace.resolve("shared_one.txt").unwrap();
    let unique_path = h.workspace.resolve("unique_one.txt").unwrap();
    let cap = capture(&h, |fs| {
        fs.write_atomic(&shared_path, b"SHARED\n").unwrap();
        fs.write_atomic(&unique_path, b"only in set one\n").unwrap();
    });
    h.service
        .record(&ctx("m1", "t1"), &cap, false)
        .unwrap()
        .unwrap();

    let hash_shared = sha256_hex(b"SHARED\n");
    let hash_unique = sha256_hex(b"only in set one\n");
    assert!(h.blobs.has(&hash_shared));
    assert!(h.blobs.has(&hash_unique));

    let shared_two = h.workspace.resolve("shared_two.txt").unwrap();
    let unique_two = h.workspace.resolve("unique_two.txt").unwrap();
    let cap = capture(&h, |fs| {
        fs.write_atomic(&shared_two, b"SHARED\n").unwrap();
        fs.write_atomic(&unique_two, b"only in set two\n").unwrap();
    });
    h.service
        .record(&ctx("m2", "t2"), &cap, false)
        .unwrap()
        .unwrap();

    // Set 1 was pruned (N=1). The blob it shared with the surviving set lives
    // on; the blob only it referenced was garbage-collected.
    assert!(h.service.message_summaries("m1").unwrap().is_empty());
    assert_eq!(h.service.message_summaries("m2").unwrap().len(), 1);
    assert!(h.blobs.has(&hash_shared));
    assert!(!h.blobs.has(&hash_unique));
    assert!(h.blobs.has(&sha256_hex(b"only in set two\n")));
}

#[test]
fn retention_zero_disables_pruning() {
    let h = harness("retention_unlimited");
    h.db.set_change_journal_retention(0).unwrap();

    let hash_a = record_added_file(&h, Some("project"), "m1", "a.txt", b"one\n");
    record_added_file(&h, Some("project"), "m2", "b.txt", b"two\n");
    record_added_file(&h, Some("project"), "m3", "c.txt", b"three\n");

    assert_eq!(h.service.message_summaries("m1").unwrap().len(), 1);
    assert_eq!(h.service.message_summaries("m2").unwrap().len(), 1);
    assert_eq!(h.service.message_summaries("m3").unwrap().len(), 1);
    assert!(h.blobs.has(&hash_a));
}

/// Retention counts MESSAGES, not change sets: one agent message may record
/// hundreds of sets (one per tool call) and they must survive — or be pruned —
/// together as a unit.
#[test]
fn retention_counts_messages_not_sets() {
    let h = harness("retention_message_buckets");
    h.db.set_change_journal_retention(1).unwrap();

    // One message, three sets (three tool calls). With set-counting semantics
    // N=1 would have pruned two of them; with message-counting all three stay.
    for (index, name) in ["m1_a.txt", "m1_b.txt", "m1_c.txt"].iter().enumerate() {
        let path = h.workspace.resolve(name).unwrap();
        let cap = capture(&h, |fs| {
            fs.write_atomic(&path, format!("m1 file {index}\n").as_bytes())
                .unwrap();
        });
        h.service
            .record(
                &ctx_in(Some("project"), "m1", &format!("t1-{index}")),
                &cap,
                false,
            )
            .unwrap()
            .unwrap();
    }
    assert_eq!(h.service.message_summaries("m1").unwrap().len(), 3);

    // A newer message displaces the whole m1 bucket at once.
    record_added_file(&h, Some("project"), "m2", "m2.txt", b"newer\n");
    assert!(h.service.message_summaries("m1").unwrap().is_empty());
    assert_eq!(h.service.message_summaries("m2").unwrap().len(), 1);
    assert!(!h.blobs.has(&sha256_hex(b"m1 file 0\n")));
    assert!(h.blobs.has(&sha256_hex(b"newer\n")));
}
