//! Content-addressed snapshot storage for the change journal.
//!
//! This is the infrastructure half of the "key design risk": snapshot storage is
//! swappable, but the rollback *policy* lives in [`super::service`]. For the first
//! slice the backend is a simple content-addressed blob store on disk — full-file
//! before/after snapshots keyed by SHA-256, never touching the user's `.git`.
//! A later slice can replace this with a private/shadow git object store behind
//! the same [`SnapshotBlobStore`] port without changing the conflict rules.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The swappable snapshot backend: store and retrieve immutable content blobs by
/// their content hash.
pub trait SnapshotBlobStore: Send + Sync {
    /// Store `bytes`, returning their hex SHA-256 content id. Idempotent: storing
    /// identical bytes twice returns the same id and writes at most once.
    fn put(&self, bytes: &[u8]) -> io::Result<String>;
    /// Load the bytes for a content id, or `None` if no such blob exists.
    fn get(&self, hash: &str) -> io::Result<Option<Vec<u8>>>;
    /// Whether a blob exists for this content id.
    fn has(&self, hash: &str) -> bool;
    /// Remove the blob for a content id. Idempotent: removing an absent blob
    /// succeeds. Callers (retention pruning) must only pass hashes no longer
    /// referenced by any change-set file row — the store itself does no
    /// reference counting.
    fn remove(&self, hash: &str) -> io::Result<()>;
}

/// Hex-encode the SHA-256 of `bytes` (lower-case), matching
/// [`crate::tools::FileSystem::hash_file_sha256`] so a stored blob's id equals
/// the hash of the same file read back from disk.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A [`SnapshotBlobStore`] backed by a per-project directory, sharded by the
/// first two hex characters of the content id (so a single directory never holds
/// an unbounded fan-out of files).
#[derive(Debug, Clone)]
pub struct FileBlobStore {
    root: PathBuf,
}

impl FileBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        let split = 2.min(hash.len());
        let (shard, rest) = hash.split_at(split);
        self.root.join(shard).join(rest)
    }
}

impl SnapshotBlobStore for FileBlobStore {
    fn put(&self, bytes: &[u8]) -> io::Result<String> {
        let hash = sha256_hex(bytes);
        let path = self.path_for(&hash);
        if path.exists() {
            return Ok(hash);
        }
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "blob path has no parent")
        })?;
        fs::create_dir_all(parent)?;

        // Write to a unique temp file beside the target, then rename — so a
        // concurrent reader sees a complete blob or none, never a partial one.
        let temp = unique_temp_path(parent, &hash);
        fs::write(&temp, bytes)?;
        match fs::rename(&temp, &path) {
            Ok(()) => Ok(hash),
            Err(error) => {
                // Another writer may have won the race and created the blob; the
                // content is identical, so treat an existing target as success.
                let _ = fs::remove_file(&temp);
                if path.exists() {
                    Ok(hash)
                } else {
                    Err(error)
                }
            }
        }
    }

    fn get(&self, hash: &str) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.path_for(hash)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn has(&self, hash: &str) -> bool {
        self.path_for(hash).exists()
    }

    fn remove(&self, hash: &str) -> io::Result<()> {
        let path = self.path_for(hash);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        // Best effort: drop the shard directory once it is empty (remove_dir
        // refuses non-empty directories, so a concurrent writer is safe).
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
        Ok(())
    }
}

fn unique_temp_path(parent: &Path, hash: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    parent.join(format!(".{hash}.{pid}.{nanos}.btmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mothership_blob_{label}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn put_get_round_trip_and_dedup() {
        let dir = temp_dir("roundtrip");
        let store = FileBlobStore::new(&dir);

        let hash = store.put(b"hello world").unwrap();
        assert_eq!(hash, sha256_hex(b"hello world"));
        assert!(store.has(&hash));
        assert_eq!(
            store.get(&hash).unwrap().as_deref(),
            Some(&b"hello world"[..])
        );

        // Storing again is idempotent and yields the same id.
        let hash2 = store.put(b"hello world").unwrap();
        assert_eq!(hash, hash2);

        // Unknown id reads back as None.
        assert!(store.get("deadbeef").unwrap().is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_blob_is_storable() {
        let dir = temp_dir("empty");
        let store = FileBlobStore::new(&dir);
        let hash = store.put(b"").unwrap();
        assert_eq!(store.get(&hash).unwrap().as_deref(), Some(&b""[..]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_deletes_the_blob_and_is_idempotent() {
        let dir = temp_dir("remove");
        let store = FileBlobStore::new(&dir);

        let hash = store.put(b"to be removed").unwrap();
        assert!(store.has(&hash));

        store.remove(&hash).unwrap();
        assert!(!store.has(&hash));
        assert!(store.get(&hash).unwrap().is_none());

        // Removing an absent blob is not an error.
        store.remove(&hash).unwrap();

        let _ = fs::remove_dir_all(&dir);
    }
}
