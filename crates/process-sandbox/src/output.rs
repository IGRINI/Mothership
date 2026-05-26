//! Output draining and bounding.
//!
//! Enforces the central invariant from `OUTPUT_AND_IPC.md`: **drain stdout and
//! stderr in separate concurrent tasks, always** — never read one to EOF and then
//! the other, or the child can block writing the stream you are not reading and
//! "the tool hangs."
//!
//! For now we keep a simple bounded in-memory capture (head + tail, capped bytes).
//!
//! TODO(T3.3 follow-up): full [`OutputPolicy`] — UI stream throttling
//! (`ui_stream_bytes_per_sec`), a separate `agent_tail_bytes` budget, and
//! `spill_to_file` for the complete unbounded log on disk + a `full_log_ref`.
//! Today only `memory_preview_bytes` (as head+tail) is honored. Volume is never a
//! reason to kill — see the module docs of `OUTPUT_AND_IPC.md`.

use tokio::io::{AsyncRead, AsyncReadExt};

/// Which stream produced a chunk of output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// How much output to hold/stream/return. Mirrors the struct sketched in
/// `OUTPUT_AND_IPC.md`. Only `memory_preview_bytes` is wired up today (see the
/// module TODO); the rest are placeholders for the follow-up.
#[derive(Debug, Clone, Copy)]
pub struct OutputPolicy {
    /// Bytes held in RAM per stream as a head+tail preview (e.g. 256 KiB).
    pub memory_preview_bytes: usize,
    /// UI stream rate cap, throttled (e.g. 128 KiB/s). NOT yet enforced.
    pub ui_stream_bytes_per_sec: usize,
    /// Tail bytes handed back to the model (e.g. 64 KiB). NOT yet enforced.
    pub agent_tail_bytes: usize,
    /// Whether to spill the full log to disk, unbounded. NOT yet implemented.
    pub spill_to_file: bool,
}

impl Default for OutputPolicy {
    fn default() -> Self {
        Self {
            memory_preview_bytes: 256 * 1024,
            ui_stream_bytes_per_sec: 128 * 1024,
            agent_tail_bytes: 64 * 1024,
            spill_to_file: false,
        }
    }
}

/// A bounded in-memory capture of one stream: keeps the first `cap/2` and last
/// `cap/2` bytes, dropping the middle (with a marker) once the cap is exceeded.
///
/// This is deliberately small and allocation-cheap on the hot path: we only ever
/// retain at most `cap` bytes regardless of how much the tool prints, so a 1 GB
/// `cargo build` log never blows up memory.
#[derive(Debug)]
pub struct BoundedCapture {
    cap: usize,
    head: Vec<u8>,
    /// Ring of the most recent bytes, kept to at most `cap - head_budget()`.
    tail: std::collections::VecDeque<u8>,
    total: usize,
}

impl BoundedCapture {
    /// Create a capture that retains at most `cap` bytes (split head/tail).
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            total: 0,
        }
    }

    fn head_budget(&self) -> usize {
        self.cap / 2
    }

    fn tail_budget(&self) -> usize {
        self.cap - self.head_budget()
    }

    /// Append a chunk, preserving head and the most-recent tail within the cap.
    pub fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        if self.cap == 0 {
            return;
        }

        // Fill the head first.
        let head_budget = self.head_budget();
        let mut rest = chunk;
        if self.head.len() < head_budget {
            let take = (head_budget - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }

        // Everything else flows into the bounded tail ring.
        let tail_budget = self.tail_budget();
        if tail_budget == 0 || rest.is_empty() {
            return;
        }
        if rest.len() >= tail_budget {
            // Only the last `tail_budget` bytes of `rest` can survive.
            self.tail.clear();
            self.tail.extend(&rest[rest.len() - tail_budget..]);
        } else {
            while self.tail.len() + rest.len() > tail_budget {
                self.tail.pop_front();
            }
            self.tail.extend(rest);
        }
    }

    /// Total number of bytes ever observed (including dropped middle).
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total
    }

    /// `true` if any bytes were dropped from the middle (output exceeded the cap).
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.total > self.head.len() + self.tail.len()
    }

    /// Render the capture as bytes: head, an optional `… <N> bytes elided …`
    /// marker, then tail.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        let truncated = self.truncated();
        let elided = self.total.saturating_sub(self.head.len() + self.tail.len());
        let mut out = self.head;
        if truncated {
            out.extend_from_slice(format!("\n… {elided} bytes elided …\n").as_bytes());
        }
        out.extend(self.tail);
        out
    }

    /// Render the capture as a lossy UTF-8 string (head + marker + tail).
    #[must_use]
    pub fn into_string_lossy(self) -> String {
        String::from_utf8_lossy(&self.into_bytes()).into_owned()
    }
}

/// Drain a single async stream to EOF into a [`BoundedCapture`]. Intended to be
/// run inside its own `tokio::spawn` so stdout and stderr are drained
/// concurrently (see [`drain_parallel`]).
pub async fn drain<R>(mut reader: R, cap: usize) -> std::io::Result<BoundedCapture>
where
    R: AsyncRead + Unpin,
{
    let mut capture = BoundedCapture::new(cap);
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        capture.push(&buf[..n]);
    }
    Ok(capture)
}

/// Drain stdout and stderr **concurrently** into two bounded captures.
///
/// This is the safe pattern: each stream gets its own task, so a child blocked
/// writing stderr can never wedge us while we read stdout. Returns `(stdout,
/// stderr)` captures once both reach EOF.
///
/// `None` is returned for a stream that was not provided (already taken / not
/// piped).
pub async fn drain_parallel(
    stdout: Option<Box<dyn AsyncRead + Send + Unpin>>,
    stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
    policy: OutputPolicy,
) -> std::io::Result<(Option<BoundedCapture>, Option<BoundedCapture>)> {
    let cap = policy.memory_preview_bytes;

    // Two concurrent tasks — NEVER sequential read_to_end calls.
    let stdout_task = stdout.map(|s| tokio::spawn(async move { drain(s, cap).await }));
    let stderr_task = stderr.map(|s| tokio::spawn(async move { drain(s, cap).await }));

    let out = match stdout_task {
        Some(t) => Some(t.await.map_err(std::io::Error::other)??),
        None => None,
    };
    let err = match stderr_task {
        Some(t) => Some(t.await.map_err(std::io::Error::other)??),
        None => None,
    };
    Ok((out, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_capture_keeps_everything_under_cap() {
        let mut c = BoundedCapture::new(100);
        c.push(b"hello world");
        assert!(!c.truncated());
        assert_eq!(c.into_string_lossy(), "hello world");
    }

    #[test]
    fn bounded_capture_keeps_head_and_tail_when_over_cap() {
        let mut c = BoundedCapture::new(10); // head 5, tail 5
        c.push(b"AAAAA"); // fills head
        c.push(b"BBBBBBBBBB"); // pushes tail
        c.push(b"ZZZZZ"); // most recent -> tail
        assert!(c.truncated());
        let s = c.into_string_lossy();
        assert!(s.starts_with("AAAAA"), "head preserved: {s:?}");
        assert!(s.ends_with("ZZZZZ"), "tail preserved: {s:?}");
        assert!(s.contains("elided"), "marker present: {s:?}");
    }

    #[test]
    fn bounded_capture_zero_cap_drops_all_but_counts() {
        let mut c = BoundedCapture::new(0);
        c.push(b"anything");
        assert_eq!(c.total_bytes(), 8);
        assert!(c.truncated());
    }

    #[tokio::test]
    async fn drain_reads_to_eof() {
        let data = b"line1\nline2\n".to_vec();
        let cap = drain(std::io::Cursor::new(data), 1024).await.unwrap();
        assert_eq!(cap.total_bytes(), 12);
        assert_eq!(cap.into_string_lossy(), "line1\nline2\n");
    }

    #[tokio::test]
    async fn drain_parallel_handles_both_streams() {
        let out: Box<dyn AsyncRead + Send + Unpin> =
            Box::new(std::io::Cursor::new(b"stdout-data".to_vec()));
        let err: Box<dyn AsyncRead + Send + Unpin> =
            Box::new(std::io::Cursor::new(b"stderr-data".to_vec()));
        let (o, e) = drain_parallel(Some(out), Some(err), OutputPolicy::default())
            .await
            .unwrap();
        assert_eq!(o.unwrap().into_string_lossy(), "stdout-data");
        assert_eq!(e.unwrap().into_string_lossy(), "stderr-data");
    }
}
