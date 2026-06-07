//! A [`FileSystem`] decorator that records the pre-mutation content of every
//! workspace path a file tool touches.
//!
//! This is the capture seam for the change journal. The file tools resolve a path
//! and call `write_atomic` / `remove_file` / `rename` on the injected filesystem;
//! wrapping that filesystem lets Core record the *before* bytes of each touched
//! path uniformly — no per-tool argument parsing, and it stays correct for future
//! mutating tools. All reads and the writes themselves are delegated unchanged to
//! the wrapped filesystem; the decorator only observes.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::tools::{FileMetadata, FileSystem};

/// Upper bound on bytes captured per file side. A file larger than this is
/// tracked (so its change is still visible) but its content is not snapshotted,
/// which disables revert for that file. Generous: only multi-tens-of-MB files hit
/// it in practice.
const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

/// The content of one side (before or after) of a captured file.
#[derive(Debug, Clone, Default)]
pub struct CapturedContent {
    /// Whether the file existed on this side.
    pub existed: bool,
    /// The file bytes, or `None` when absent or too large to capture.
    pub bytes: Option<Vec<u8>>,
    /// The file existed but exceeded [`MAX_CAPTURE_BYTES`].
    pub too_large: bool,
}

/// A single workspace path that a tool mutated, with its before/after content.
#[derive(Debug, Clone)]
pub struct CapturedPath {
    /// Workspace-relative path, forward-slash normalized.
    pub rel_path: String,
    pub before: CapturedContent,
    pub after: CapturedContent,
}

/// Pre-mutation snapshot of one path, recorded the first time the tool touches
/// it within a capture session.
#[derive(Debug, Clone)]
struct BeforeState {
    content: CapturedContent,
}

/// Wraps an inner [`FileSystem`], recording before-state on the first mutating
/// touch of each path and delegating every operation to the inner filesystem.
pub struct CaptureFileSystem {
    inner: Arc<dyn FileSystem>,
    root: PathBuf,
    before: Mutex<HashMap<PathBuf, BeforeState>>,
}

impl CaptureFileSystem {
    /// Wrap `inner`, recording paths relative to the canonical workspace `root`.
    pub fn new(inner: Arc<dyn FileSystem>, root: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            root: root.into(),
            before: Mutex::new(HashMap::new()),
        }
    }

    /// Read the current content of `path` and remember it as the before-state,
    /// unless this path was already recorded in this session.
    fn record_before(&self, path: &Path) {
        let mut before = match self.before.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if before.contains_key(path) {
            return;
        }
        let content = read_content(self.inner.as_ref(), path);
        before.insert(path.to_path_buf(), BeforeState { content });
    }

    /// Finalize the capture: for every path the tool touched, pair its recorded
    /// before-state with its current (after) content read from the inner
    /// filesystem. Drains the recorded set.
    pub fn finalize(&self) -> Vec<CapturedPath> {
        let recorded = {
            let mut before = match self.before.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *before)
        };

        let mut captured = Vec::with_capacity(recorded.len());
        for (abs, state) in recorded {
            let after = read_content(self.inner.as_ref(), &abs);
            let rel_path = self.relative_path(&abs);
            captured.push(CapturedPath {
                rel_path,
                before: state.content,
                after,
            });
        }
        captured
    }

    /// The workspace-relative, forward-slash path for an absolute resolved path.
    ///
    /// The fast path is `strip_prefix`, which hits whenever the tool resolved a
    /// relative argument (the resolved path is then rooted at the same canonical
    /// root). The fallback covers the verbatim-prefix / case mismatch that arises
    /// on Windows when a tool resolves an *absolute* argument inside the root
    /// (`E:\proj\x` vs a canonical `\\?\E:\proj` root).
    fn relative_path(&self, abs: &Path) -> String {
        if let Ok(relative) = abs.strip_prefix(&self.root) {
            return relative.to_string_lossy().replace('\\', "/");
        }

        let abs_plain = strip_verbatim(&abs.to_string_lossy().replace('\\', "/"));
        let root_plain = strip_verbatim(&self.root.to_string_lossy().replace('\\', "/"));
        let take = root_plain.len();
        let matches = if cfg!(windows) {
            abs_plain.len() >= take && abs_plain[..take].eq_ignore_ascii_case(&root_plain)
        } else {
            abs_plain.starts_with(&root_plain)
        };
        if matches && abs_plain.is_char_boundary(take) {
            return abs_plain[take..].trim_start_matches('/').to_string();
        }
        abs_plain
    }
}

/// Drop a Windows `\\?\` / `\\?\UNC\` verbatim prefix from an already
/// forward-slash-normalized path string.
fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else if let Some(rest) = path.strip_prefix("//?/") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

/// Read the current content of a path into a [`CapturedContent`], honoring the
/// capture size cap.
fn read_content(fs: &dyn FileSystem, path: &Path) -> CapturedContent {
    if !fs.exists(path) {
        return CapturedContent {
            existed: false,
            bytes: None,
            too_large: false,
        };
    }
    // Read one byte past the cap so we can tell "exactly at cap" from "over cap".
    match fs.read_capped(path, MAX_CAPTURE_BYTES) {
        Ok((bytes, truncated)) => {
            if truncated {
                CapturedContent {
                    existed: true,
                    bytes: None,
                    too_large: true,
                }
            } else {
                CapturedContent {
                    existed: true,
                    bytes: Some(bytes),
                    too_large: false,
                }
            }
        }
        // Unreadable despite existing (permissions, race): track presence without
        // content so revert treats it as a missing snapshot rather than crashing.
        Err(_) => CapturedContent {
            existed: true,
            bytes: None,
            too_large: true,
        },
    }
}

impl FileSystem for CaptureFileSystem {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_capped(&self, path: &Path, max: usize) -> io::Result<(Vec<u8>, bool)> {
        self.inner.read_capped(path, max)
    }

    fn hash_file_sha256(&self, path: &Path) -> io::Result<String> {
        self.inner.hash_file_sha256(path)
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.record_before(path);
        self.inner.write_atomic(path, bytes)
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.inner.metadata(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.record_before(from);
        self.record_before(to);
        self.inner.rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.record_before(path);
        self.inner.remove_file(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir(path)
    }
}
