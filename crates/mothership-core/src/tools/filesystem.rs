//! Project filesystem port and path resolver for the typed file tools.
//!
//! Two concerns live here:
//!
//! * [`Workspace`] — a canonicalized project root plus the path *policy*: every
//!   tool-supplied path is resolved against the root and rejected if it escapes
//!   the root (via `..`, an absolute path elsewhere, or a symlink that points
//!   outside). This is the path-containment logic the sidecar previously owned
//!   for `run_command`, lifted into Core so the file tools share it. The
//!   resolver also flags *sensitive* paths (`.env`, `.ssh`, private keys, cloud
//!   credentials, `.git/config`, vaults, secrets) so the policy layer can refuse
//!   to read or mutate them.
//! * [`FileSystem`] — the injected byte-IO port (mirroring how `ProcessSandbox`
//!   injects process spawning). Core never calls `std::fs` directly for tool IO;
//!   the sidecar composition root supplies [`StdFileSystem`]. Tests can supply an
//!   in-memory implementation.
//!
//! The [`StdFileSystem`] write path is **atomic**: the new bytes are written to a
//! temporary file in the *same directory* as the target, flushed/fsynced, and
//! then `std::fs::rename`d over the target. Rust's `std::fs::rename` replaces an
//! existing destination on every supported platform (including Windows, where it
//! maps to `MoveFileEx`/`SetFileInformationByHandle` with replace semantics), so
//! a reader either sees the old file or the fully written new file, never a
//! partial write.

#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// A canonicalized project root and the path policy enforced against it.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

/// Why a tool-supplied path was rejected by [`Workspace::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// The path resolved to a location outside the workspace root.
    OutsideWorkspace { path: String },
    /// The path could not be made relative-safe (e.g. empty, or a Windows
    /// drive/UNC prefix that does not belong to the root).
    Invalid { path: String, detail: String },
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::OutsideWorkspace { path } => {
                write!(f, "path `{path}` is outside the project workspace")
            }
            PathError::Invalid { path, detail } => {
                write!(f, "path `{path}` is invalid: {detail}")
            }
        }
    }
}

impl std::error::Error for PathError {}

impl Workspace {
    /// Build a workspace from a project root, canonicalizing it. The root must
    /// already exist and be a directory.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = fs::canonicalize(root.as_ref())?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "workspace root must be a directory",
            ));
        }
        Ok(Self { root })
    }

    /// Construct a workspace from an already-canonical root without touching the
    /// filesystem. Intended for tests with a known-good path.
    pub fn from_canonical_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The canonical project root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a tool-supplied path against the workspace root, enforcing
    /// containment.
    ///
    /// Relative paths are joined onto the root; absolute paths are required to be
    /// inside the root. The result is lexically normalized (so `..` cannot climb
    /// above the root) and, when the target (or its nearest existing ancestor)
    /// exists, canonicalized so a symlink that points outside the root is caught.
    /// The returned path is absolute but is **not** required to exist — callers
    /// creating new files resolve a not-yet-existing path whose parent is inside
    /// the workspace.
    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf, PathError> {
        let raw = path.as_ref();
        let display = raw.display().to_string();
        if display.trim().is_empty() {
            return Err(PathError::Invalid {
                path: display,
                detail: "path is empty".to_string(),
            });
        }

        // Join relative paths onto the root; keep absolute paths as-is for the
        // containment check below.
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };

        // Lexically normalize so `..` segments are resolved without touching the
        // filesystem (this catches `../../etc/passwd` before any IO).
        //
        // Containment is checked with a platform-aware comparison rather than a
        // raw `starts_with`: on Windows a canonicalized verbatim root
        // (`\\?\E:\proj`) must still contain a normal `E:\proj\src\x.rs` input,
        // and the match must be case-insensitive. `path_contains` strips the
        // verbatim prefixes, folds case (on Windows), and normalizes separators
        // on both sides.
        let normalized = lexically_normalize(&joined);
        if !path_contains(&self.root, &normalized) {
            return Err(PathError::OutsideWorkspace { path: display });
        }

        // If the target or an ancestor exists, canonicalize the deepest existing
        // prefix to defeat symlink escapes, then re-check containment. A target
        // that does not exist yet (new file) is allowed as long as its lexical
        // form and its nearest existing ancestor stay inside the root.
        let canonical_prefix = canonicalize_existing_prefix(&normalized);
        if let Some(prefix) = canonical_prefix {
            if !path_contains(&self.root, &prefix) {
                return Err(PathError::OutsideWorkspace { path: display });
            }
        }

        Ok(normalized)
    }

    /// Whether a resolved path is *sensitive* (credentials, private keys, VCS
    /// internals, vaults). Matched against the path relative to the root so a
    /// project legitimately named e.g. `secrets-manager` at the root is not the
    /// trigger — the check looks at individual path components and a few
    /// well-known filenames.
    pub fn is_sensitive(&self, resolved: &Path) -> bool {
        let relative = resolved.strip_prefix(&self.root).unwrap_or(resolved);
        is_sensitive_relative(relative)
    }
}

/// Component- and filename-based sensitive-path detection over a path that has
/// already been confined to the workspace (so it is examined relative to the
/// root). Matching is case-insensitive to cover Windows and mixed-case repos.
pub fn is_sensitive_relative(relative: &Path) -> bool {
    // Whole-path needles that imply a credential store regardless of position.
    let lower = relative.to_string_lossy().to_ascii_lowercase().replace('\\', "/");
    if lower.contains("/.git/config") || lower == ".git/config" {
        return true;
    }

    for component in relative.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        let name = part.to_string_lossy().to_ascii_lowercase();
        if is_sensitive_component(&name) {
            return true;
        }
    }
    false
}

/// True for a single (lower-cased) path component that names a secret-bearing
/// file or directory.
fn is_sensitive_component(name: &str) -> bool {
    // Exact, well-known credential files / directories.
    const EXACT: &[&str] = &[
        ".env",
        ".ssh",
        ".aws",
        ".gnupg",
        ".npmrc",
        ".netrc",
        ".pgpass",
        ".htpasswd",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        "secrets",
        "secret",
        "vault",
        "credentials",
        "credential",
    ];
    if EXACT.contains(&name) {
        return true;
    }

    // `.env`, `.env.local`, `.env.production`, etc.
    if name == ".env" || name.starts_with(".env.") {
        return true;
    }

    // Private key material by extension.
    if name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.ends_with("_rsa")
        || name.ends_with("_dsa")
        || name.ends_with("_ed25519")
    {
        return true;
    }

    false
}

/// Whether `candidate` is `root` itself or lives inside it, using a
/// platform-aware string comparison so a canonicalized verbatim Windows root
/// (`\\?\E:\proj`) still contains a normal `E:\proj\...` input and the match is
/// case-insensitive on Windows. Mirrors the sidecar's `run_command` cwd
/// containment (`normalize_for_compare`/`path_within`) but additionally strips
/// the `\\?\` / `\\?\UNC\` verbatim prefixes that `fs::canonicalize` produces on
/// Windows, since the root is canonicalized but tool-supplied inputs are not.
fn path_contains(root: &Path, candidate: &Path) -> bool {
    let root = normalize_for_compare(root);
    let candidate = normalize_for_compare(candidate);
    if candidate == root {
        return true;
    }
    // `root` ends without a trailing slash; require the next char in `candidate`
    // to be a separator so `E:/proj` does not falsely contain `E:/project`.
    match candidate.strip_prefix(&root) {
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// Normalize a path to a comparable string: drop a `\\?\` / `\\?\UNC\` verbatim
/// prefix, replace `\` with `/`, and (on Windows) fold ASCII case so paths that
/// differ only by case or separator style compare equal.
fn normalize_for_compare(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    // Strip Windows verbatim prefixes left by canonicalization. `\\?\UNC\` maps a
    // UNC share; re-add the leading `//` so the share path stays absolute.
    if let Some(rest) = text.strip_prefix("//?/UNC/") {
        text = format!("//{rest}");
    } else if let Some(rest) = text.strip_prefix("//?/") {
        text = rest.to_string();
    }
    // Drop a single trailing separator so `E:/proj/` compares equal to `E:/proj`.
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }

    #[cfg(windows)]
    {
        text.make_ascii_lowercase();
    }

    text
}

/// Lexically normalize a path: collapse `.` and resolve `..` against earlier
/// `Normal` components, without consulting the filesystem. The root/prefix
/// components are preserved so an absolute path stays absolute.
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop the last `Normal` segment if there is one; never pop a
                // prefix/root (so we cannot escape above an absolute anchor
                // lexically — the containment check then rejects it anyway).
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(Component::ParentDir.as_os_str());
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Canonicalize the deepest existing ancestor of `path` (including `path`
/// itself if it exists). Returns `None` when not even the root portion exists
/// (extremely unlikely for a workspace path, but handled defensively). Used to
/// defeat symlink escapes: if any existing component is a symlink that points
/// outside the workspace, the canonical prefix reveals it.
fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if let Ok(canonical) = fs::canonicalize(current) {
            // If we canonicalized an ancestor (not the full path), re-attach the
            // remaining components so the caller compares the full intended path.
            let suffix = path.strip_prefix(current).ok()?;
            return Some(canonical.join(suffix));
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => return None,
        }
    }
}

/// File metadata the tools care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMetadata {
    pub len: u64,
    pub is_dir: bool,
}

/// Injected byte-IO port. Core uses only these operations for file tools; the
/// sidecar supplies [`StdFileSystem`] at the composition root.
pub trait FileSystem: Send + Sync {
    /// Read the entire file as bytes.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Read at most `max` bytes from `path`, returning the bytes read and whether
    /// the file had more than `max` bytes (i.e. the read was truncated). This lets
    /// `read_file` bound its allocation up front for a huge file instead of
    /// reading the whole thing and capping afterwards. The default implementation
    /// reads the whole file via [`FileSystem::read`] and then truncates; the
    /// [`StdFileSystem`] override streams through a capped reader so it never
    /// allocates the full file.
    fn read_capped(&self, path: &Path, max: usize) -> io::Result<(Vec<u8>, bool)> {
        let bytes = self.read(path)?;
        if bytes.len() > max {
            let mut capped = bytes;
            capped.truncate(max);
            Ok((capped, true))
        } else {
            Ok((bytes, false))
        }
    }
    /// Atomically write `bytes` to `path`, replacing any existing file.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Stat the path.
    fn metadata(&self, path: &Path) -> io::Result<FileMetadata>;
    /// Whether the path exists.
    fn exists(&self, path: &Path) -> bool;
    /// Rename/move a file.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Remove a file.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Create a directory and all missing parents.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
}

/// `std::fs`-backed [`FileSystem`] with an atomic, same-directory write.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFileSystem;

impl StdFileSystem {
    pub fn new() -> Self {
        Self
    }
}

impl FileSystem for StdFileSystem {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn read_capped(&self, path: &Path, max: usize) -> io::Result<(Vec<u8>, bool)> {
        use std::io::Read as _;
        let file = fs::File::open(path)?;
        // Read up to `max + 1` bytes: the extra byte (if present) tells us the
        // file was longer than `max` without us ever buffering the whole thing.
        let mut buf = Vec::new();
        file.take(max as u64 + 1).read_to_end(&mut buf)?;
        let truncated = buf.len() > max;
        if truncated {
            buf.truncate(max);
        }
        Ok((buf, truncated))
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "target path has no parent")
        })?;
        fs::create_dir_all(parent)?;

        // Write to a uniquely-named temp file beside the target so the final
        // rename is on the same volume (a cross-volume rename is not atomic and
        // would error on Windows).
        let temp = unique_temp_path(parent, path);
        {
            use std::io::Write as _;
            let mut file = fs::File::create(&temp)?;
            file.write_all(bytes)?;
            file.flush()?;
            // Best-effort durability before the rename; ignore platforms/handles
            // that do not support sync.
            let _ = file.sync_all();
        }

        match fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Clean up the temp file so a failed write leaves no debris.
                let _ = fs::remove_file(&temp);
                Err(error)
            }
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        let meta = fs::metadata(path)?;
        Ok(FileMetadata {
            len: meta.len(),
            is_dir: meta.is_dir(),
        })
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }
}

/// Build a unique temp path beside `target` in `parent`, incorporating the
/// target's file name plus a process id / nanosecond stamp so concurrent writes
/// to different targets never collide on the temp file.
fn unique_temp_path(parent: &Path, target: &Path) -> PathBuf {
    let stem = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tool".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    parent.join(format!(".{stem}.{pid}.{nanos}.mtmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mothership_ws_{label}_{nanos}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn resolve_relative_path_inside_workspace() {
        let dir = unique_temp_dir("inside");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), b"fn main() {}\n").unwrap();
        let ws = Workspace::new(&dir).unwrap();

        let resolved = ws.resolve("src/main.rs").expect("resolve");
        assert!(resolved.ends_with("main.rs"));
        assert!(resolved.starts_with(ws.root()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_rejects_parent_escape() {
        let dir = unique_temp_dir("escape");
        let ws = Workspace::new(&dir).unwrap();

        let err = ws.resolve("../../etc/passwd").unwrap_err();
        assert!(matches!(err, PathError::OutsideWorkspace { .. }), "{err:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_rejects_absolute_outside() {
        let dir = unique_temp_dir("abs_outside");
        let outside = unique_temp_dir("abs_target");
        let ws = Workspace::new(&dir).unwrap();

        let err = ws.resolve(outside.join("secret.txt")).unwrap_err();
        assert!(matches!(err, PathError::OutsideWorkspace { .. }), "{err:?}");

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn resolve_allows_new_file_with_contained_parent() {
        let dir = unique_temp_dir("newfile");
        fs::create_dir_all(dir.join("src")).unwrap();
        let ws = Workspace::new(&dir).unwrap();

        // The file does not exist yet but its parent is inside the workspace.
        let resolved = ws.resolve("src/created.rs").expect("resolve new");
        assert!(resolved.starts_with(ws.root()));
        assert!(!resolved.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sensitive_paths_are_flagged() {
        assert!(is_sensitive_relative(Path::new(".env")));
        assert!(is_sensitive_relative(Path::new(".env.production")));
        assert!(is_sensitive_relative(Path::new("config/.ssh/id_rsa")));
        assert!(is_sensitive_relative(Path::new("deploy/id_ed25519")));
        assert!(is_sensitive_relative(Path::new(".aws/credentials")));
        assert!(is_sensitive_relative(Path::new(".git/config")));
        assert!(is_sensitive_relative(Path::new("certs/server.pem")));
        assert!(is_sensitive_relative(Path::new("keys/private.key")));
        assert!(is_sensitive_relative(Path::new("ops/vault/data.json")));

        assert!(!is_sensitive_relative(Path::new("src/main.rs")));
        assert!(!is_sensitive_relative(Path::new("README.md")));
        assert!(!is_sensitive_relative(Path::new("environment.ts")));
    }

    #[test]
    fn atomic_write_round_trip() {
        let dir = unique_temp_dir("atomic");
        let fs_port = StdFileSystem::new();
        let target = dir.join("nested/out.txt");

        fs_port.write_atomic(&target, b"first\n").unwrap();
        assert_eq!(fs_port.read(&target).unwrap(), b"first\n");

        // Overwrite replaces the existing file atomically.
        fs_port.write_atomic(&target, b"second contents\n").unwrap();
        assert_eq!(fs_port.read(&target).unwrap(), b"second contents\n");

        // No temp files left behind in the target's directory.
        let leftovers: Vec<_> = fs::read_dir(dir.join("nested"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".mtmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");

        let meta = fs_port.metadata(&target).unwrap();
        assert_eq!(meta.len, b"second contents\n".len() as u64);
        assert!(!meta.is_dir);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_capped_truncates_large_file_without_reading_all() {
        let dir = unique_temp_dir("read_capped");
        let fs_port = StdFileSystem::new();
        let target = dir.join("big.bin");
        // 1 MiB on disk; we cap the read at 1 KiB.
        let big = vec![b'a'; 1024 * 1024];
        fs::write(&target, &big).unwrap();

        let (bytes, truncated) = fs_port.read_capped(&target, 1024).unwrap();
        assert!(truncated, "read should report truncation");
        assert_eq!(bytes.len(), 1024, "read must be capped to `max`");

        // A small file is returned whole and not flagged truncated.
        let small = dir.join("small.bin");
        fs::write(&small, b"hello").unwrap();
        let (bytes, truncated) = fs_port.read_capped(&small, 1024).unwrap();
        assert!(!truncated);
        assert_eq!(bytes, b"hello");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_read_capped_matches_std_override() {
        // The default trait impl (read-then-truncate) must agree with the
        // StdFileSystem streaming override on the observable result.
        struct DefaultReadFs {
            bytes: Vec<u8>,
        }
        impl FileSystem for DefaultReadFs {
            fn read(&self, _path: &Path) -> io::Result<Vec<u8>> {
                Ok(self.bytes.clone())
            }
            fn write_atomic(&self, _path: &Path, _bytes: &[u8]) -> io::Result<()> {
                Ok(())
            }
            fn metadata(&self, _path: &Path) -> io::Result<FileMetadata> {
                Ok(FileMetadata {
                    len: self.bytes.len() as u64,
                    is_dir: false,
                })
            }
            fn exists(&self, _path: &Path) -> bool {
                true
            }
            fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
                Ok(())
            }
            fn remove_file(&self, _path: &Path) -> io::Result<()> {
                Ok(())
            }
            fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
                Ok(())
            }
        }

        let fs_port = DefaultReadFs {
            bytes: vec![b'z'; 5000],
        };
        let (bytes, truncated) = fs_port.read_capped(Path::new("x"), 1000).unwrap();
        assert!(truncated);
        assert_eq!(bytes.len(), 1000);
    }

    #[test]
    fn path_contains_handles_verbatim_and_separators() {
        // Verbatim Windows root must contain a plain input (and vice-versa);
        // separators and (on Windows) case must not matter.
        assert!(path_contains(
            Path::new(r"\\?\E:\proj"),
            Path::new(r"E:\proj\src\x.rs")
        ));
        assert!(path_contains(Path::new("/home/u/proj"), Path::new("/home/u/proj/src")));
        // The root itself is contained.
        assert!(path_contains(Path::new("/a/b"), Path::new("/a/b")));
        // A sibling that merely shares a prefix is NOT contained.
        assert!(!path_contains(Path::new("/a/proj"), Path::new("/a/project")));
        // A genuine escape is rejected.
        assert!(!path_contains(Path::new("/a/proj"), Path::new("/a/other")));
    }

    #[cfg(windows)]
    #[test]
    fn path_contains_is_case_insensitive_on_windows() {
        assert!(path_contains(
            Path::new(r"\\?\E:\Proj"),
            Path::new(r"e:\proj\SRC\x.rs")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn resolve_passes_when_root_is_verbatim_and_input_is_plain() {
        // Build a workspace whose root is a verbatim path (as `fs::canonicalize`
        // produces on Windows), then resolve a plain absolute input inside it.
        let dir = unique_temp_dir("verbatim_root");
        fs::create_dir_all(dir.join("src")).unwrap();
        let canonical = fs::canonicalize(&dir).unwrap();
        assert!(
            canonical.to_string_lossy().starts_with(r"\\?\"),
            "expected a verbatim canonical root on Windows, got {canonical:?}"
        );
        let ws = Workspace::from_canonical_root(canonical);

        // A non-verbatim absolute path to a file inside the workspace must resolve.
        let plain_input = dir.join("src").join("x.rs");
        let resolved = ws.resolve(&plain_input).expect("resolve plain input");
        assert!(path_contains(ws.root(), &resolved));

        // A genuine escape is still rejected.
        let outside = unique_temp_dir("verbatim_outside");
        assert!(ws.resolve(outside.join("y.rs")).is_err());

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn rename_and_remove_round_trip() {
        let dir = unique_temp_dir("rename");
        let fs_port = StdFileSystem::new();
        let from = dir.join("a.txt");
        let to = dir.join("sub/b.txt");
        fs_port.write_atomic(&from, b"x\n").unwrap();

        fs_port.rename(&from, &to).unwrap();
        assert!(!fs_port.exists(&from));
        assert!(fs_port.exists(&to));

        fs_port.remove_file(&to).unwrap();
        assert!(!fs_port.exists(&to));

        let _ = fs::remove_dir_all(&dir);
    }
}
