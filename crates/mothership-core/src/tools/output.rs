use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::{MothershipError, Result};

use super::types::{
    ToolArtifactRange, ToolExecutionEvent, ToolExecutionEventKind, ToolExecutionEventSink,
    ToolExecutionRequest, ToolOutputPolicy, ToolOutputStream,
};

/// Hard cap on a single artifact-range read, regardless of the requested limit.
pub const MAX_ARTIFACT_RANGE_BYTES: u64 = 512 * 1024;

/// Extra bytes read past the cap so a chunk can end on a line boundary and
/// absorb the writer's stream-header lines. Keeps memory bounded to ~cap+slack.
const HEADER_SLACK_BYTES: u64 = 8 * 1024;

#[async_trait::async_trait]
pub trait ToolOutputStore: Send + Sync {
    async fn open(&self, tool_call_id: &str) -> Result<Box<dyn ToolOutputWriter>>;

    /// Read a newline-aligned slice of a previously-spilled artifact, identified
    /// by the `tool_call_id` that produced it. `log_ref` is the durable reference
    /// handed to the UI; the store validates it against its own root (containment)
    /// and binds it to this tool call — the caller can never read another call's
    /// blob nor an arbitrary path. Default: unsupported.
    async fn read_range(
        &self,
        _tool_call_id: &str,
        _log_ref: &str,
        _offset: u64,
        _limit: u64,
    ) -> Result<ToolArtifactRange> {
        Err(MothershipError::Runtime(
            "this output store does not support range reads".to_string(),
        ))
    }
}

#[async_trait::async_trait]
pub trait ToolOutputWriter: Send {
    async fn append(&mut self, stream: ToolOutputStream, bytes: &[u8]) -> Result<()>;

    async fn finish(&mut self) -> Result<String>;
}

#[derive(Debug, Clone)]
pub struct FileToolOutputStore {
    root: PathBuf,
}

impl FileToolOutputStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait::async_trait]
impl ToolOutputStore for FileToolOutputStore {
    async fn open(&self, tool_call_id: &str) -> Result<Box<dyn ToolOutputWriter>> {
        tokio::fs::create_dir_all(&self.root).await?;
        let file_name = format!("{}.log", sanitize_file_stem(tool_call_id));
        let path = self.root.join(file_name);
        ensure_child_path(&self.root, &path)?;
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .await?;
        Ok(Box::new(FileToolOutputWriter { path, file }))
    }

    async fn read_range(
        &self,
        tool_call_id: &str,
        log_ref: &str,
        offset: u64,
        limit: u64,
    ) -> Result<ToolArtifactRange> {
        // Derive the blob path from the tool_call_id (the same mapping `open`
        // uses); the UI never gets to pick the path. Containment + an existence
        // check keep the read inside this store's root.
        let file_name = format!("{}.log", sanitize_file_stem(tool_call_id));
        let path = self.root.join(&file_name);
        ensure_child_path(&self.root, &path)?;
        let canonical = tokio::fs::canonicalize(&path)
            .await
            .map_err(|error| MothershipError::Runtime(error.to_string()))?;

        // Bind the durable reference to this tool call: the supplied `log_ref`
        // must resolve to exactly this id's blob.
        let supplied = tokio::fs::canonicalize(Path::new(log_ref))
            .await
            .map_err(|_| MothershipError::Runtime("unknown artifact reference".to_string()))?;
        if supplied != canonical {
            return Err(MothershipError::Runtime(
                "artifact reference does not match tool call".to_string(),
            ));
        }

        // Bounded, seeked read: `offset` is a byte offset into the blob FILE, and
        // we read at most `cap + slack` bytes from there — never the whole
        // (possibly multi-MB) blob. The window ends on a line boundary; the
        // writer's stream headers are stripped from just that window.
        let file_size = tokio::fs::metadata(&canonical).await?.len();
        let cap = limit.min(MAX_ARTIFACT_RANGE_BYTES);
        let start = offset.min(file_size);
        let remaining = file_size - start;
        let window_len = cap.saturating_add(HEADER_SLACK_BYTES).min(remaining);

        let mut file = tokio::fs::File::open(&canonical).await?;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let mut window = vec![0_u8; window_len as usize];
        file.read_exact(&mut window).await?;

        let to_eof = window_len == remaining;
        let raw_end = pick_line_end(&window, cap as usize, to_eof);
        let next = start + raw_end as u64;
        let eof = next >= file_size;

        Ok(ToolArtifactRange {
            content: strip_stream_headers(&window[..raw_end]),
            offset: start,
            next_offset: if eof { None } else { Some(next) },
            total_bytes: file_size,
            eof,
        })
    }
}

struct FileToolOutputWriter {
    path: PathBuf,
    file: tokio::fs::File,
}

#[async_trait::async_trait]
impl ToolOutputWriter for FileToolOutputWriter {
    async fn append(&mut self, stream: ToolOutputStream, bytes: &[u8]) -> Result<()> {
        let label = match stream {
            ToolOutputStream::Stdout => "stdout",
            ToolOutputStream::Stderr => "stderr",
        };
        self.file
            .write_all(format!("\n--- {label} {} bytes ---\n", bytes.len()).as_bytes())
            .await?;
        self.file.write_all(bytes).await?;
        Ok(())
    }

    async fn finish(&mut self) -> Result<String> {
        self.file.flush().await?;
        Ok(self.path.display().to_string())
    }
}

pub(crate) type SharedToolOutputWriter = Arc<Mutex<Box<dyn ToolOutputWriter>>>;

#[derive(Debug)]
pub(crate) struct DrainedStream {
    pub preview: String,
    pub tail: String,
    pub total_bytes: usize,
    pub preview_truncated: bool,
    pub tail_truncated: bool,
}

pub(crate) async fn drain_stream(
    request: ToolExecutionRequest,
    stream: ToolOutputStream,
    mut reader: Box<dyn AsyncRead + Send + Unpin>,
    policy: ToolOutputPolicy,
    sink: Arc<dyn ToolExecutionEventSink>,
    writer: Option<SharedToolOutputWriter>,
) -> Result<DrainedStream> {
    let mut preview = BoundedCapture::new(policy.memory_preview_bytes);
    let mut tail = TailCapture::new(policy.agent_tail_bytes);
    let mut throttler = OutputThrottler::new(policy.ui_stream_bytes_per_sec);
    let mut decoder = ProcessOutputDecoder::new();
    let mut buf = [0_u8; 8 * 1024];

    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        let chunk = &buf[..read];
        preview.push(chunk);
        tail.push(chunk);

        if let Some(writer) = &writer {
            writer.lock().await.append(stream, chunk).await?;
        }

        if let Some(text) = decoder.decode_chunk(chunk) {
            if let Some(visible) = throttler.visible_text_chunk(&text) {
                sink.emit(ToolExecutionEvent {
                    tool_call_id: request.tool_call_id.clone(),
                    run_id: request.run_id.clone(),
                    project_id: request.project_id.clone(),
                    command: None,
                    kind: ToolExecutionEventKind::Output,
                    stream: Some(stream),
                    chunk: Some(visible),
                    message: None,
                    result: None,
                    tool_kind: None,
                    payload: None,
                    touched_paths: Vec::new(),
                    artifacts: Vec::new(),
                });
            }
        }
    }

    if let Some(text) = decoder.finish() {
        if let Some(visible) = throttler.visible_text_chunk(&text) {
            sink.emit(ToolExecutionEvent {
                tool_call_id: request.tool_call_id.clone(),
                run_id: request.run_id.clone(),
                project_id: request.project_id.clone(),
                command: None,
                kind: ToolExecutionEventKind::Output,
                stream: Some(stream),
                chunk: Some(visible),
                message: None,
                result: None,
                tool_kind: None,
                payload: None,
                touched_paths: Vec::new(),
                artifacts: Vec::new(),
            });
        }
    }

    Ok(DrainedStream {
        total_bytes: preview.total_bytes(),
        preview_truncated: preview.truncated(),
        tail_truncated: tail.truncated(),
        preview: preview.into_string_lossy(),
        tail: tail.into_string_lossy(),
    })
}

#[derive(Debug)]
struct ProcessOutputDecoder {
    pending: Vec<u8>,
    use_platform_fallback: bool,
}

impl ProcessOutputDecoder {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            use_platform_fallback: false,
        }
    }

    fn decode_chunk(&mut self, chunk: &[u8]) -> Option<String> {
        if chunk.is_empty() {
            return None;
        }

        if self.use_platform_fallback {
            return Some(decode_platform_process_output(chunk));
        }

        self.pending.extend_from_slice(chunk);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let decoded = text.to_string();
                self.pending.clear();
                Some(decoded)
            }
            Err(error) if error.error_len().is_some() => {
                self.use_platform_fallback = true;
                let decoded = decode_platform_process_output(&self.pending);
                self.pending.clear();
                Some(decoded)
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to == 0 {
                    return None;
                }

                let decoded = std::str::from_utf8(&self.pending[..valid_up_to])
                    .ok()?
                    .to_string();
                self.pending = self.pending[valid_up_to..].to_vec();
                Some(decoded)
            }
        }
    }

    fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }

        let pending = std::mem::take(&mut self.pending);
        Some(decode_process_output(&pending))
    }
}

#[derive(Debug)]
struct OutputThrottler {
    bytes_per_sec: usize,
    window_start: Instant,
    emitted: usize,
}

impl OutputThrottler {
    fn new(bytes_per_sec: usize) -> Self {
        Self {
            bytes_per_sec,
            window_start: Instant::now(),
            emitted: 0,
        }
    }

    fn visible_text_chunk(&mut self, text: &str) -> Option<String> {
        if self.bytes_per_sec == 0 || text.is_empty() {
            return None;
        }
        if self.window_start.elapsed() >= Duration::from_secs(1) {
            self.window_start = Instant::now();
            self.emitted = 0;
        }
        let remaining = self.bytes_per_sec.saturating_sub(self.emitted);
        if remaining == 0 {
            return None;
        }

        if text.len() <= remaining {
            self.emitted += text.len();
            return Some(text.to_string());
        }

        let mut take = 0;
        for (index, ch) in text.char_indices() {
            let next = index + ch.len_utf8();
            if next > remaining {
                break;
            }
            take = next;
        }

        if take == 0 {
            return None;
        }

        self.emitted += take;
        Some(text[..take].to_string())
    }
}

#[derive(Debug)]
struct BoundedCapture {
    cap: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

impl BoundedCapture {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            head: Vec::new(),
            tail: VecDeque::new(),
            total: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        if self.cap == 0 {
            return;
        }
        let head_budget = self.cap / 2;
        let mut rest = chunk;
        if self.head.len() < head_budget {
            let take = (head_budget - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }

        let tail_budget = self.cap - head_budget;
        if tail_budget == 0 || rest.is_empty() {
            return;
        }
        if rest.len() >= tail_budget {
            self.tail.clear();
            self.tail.extend(&rest[rest.len() - tail_budget..]);
        } else {
            while self.tail.len() + rest.len() > tail_budget {
                self.tail.pop_front();
            }
            self.tail.extend(rest);
        }
    }

    fn total_bytes(&self) -> usize {
        self.total
    }

    fn truncated(&self) -> bool {
        self.total > self.head.len() + self.tail.len()
    }

    fn into_string_lossy(self) -> String {
        let truncated = self.truncated();
        let elided = self.total.saturating_sub(self.head.len() + self.tail.len());
        let mut out = self.head;
        if truncated {
            out.extend_from_slice(format!("\n... {elided} bytes elided ...\n").as_bytes());
        }
        out.extend(self.tail);
        decode_process_output(&out)
    }
}

#[derive(Debug)]
struct TailCapture {
    cap: usize,
    tail: VecDeque<u8>,
    total: usize,
}

impl TailCapture {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            tail: VecDeque::new(),
            total: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        if self.cap == 0 {
            return;
        }
        if chunk.len() >= self.cap {
            self.tail.clear();
            self.tail.extend(&chunk[chunk.len() - self.cap..]);
            return;
        }
        while self.tail.len() + chunk.len() > self.cap {
            self.tail.pop_front();
        }
        self.tail.extend(chunk);
    }

    fn truncated(&self) -> bool {
        self.total > self.tail.len()
    }

    fn into_string_lossy(self) -> String {
        let bytes = self.tail.into_iter().collect::<Vec<_>>();
        decode_process_output(&bytes)
    }
}

fn decode_process_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(error) if error.error_len().is_none() => String::from_utf8_lossy(bytes).into_owned(),
        Err(_) => decode_platform_process_output(bytes),
    }
}

#[cfg(not(windows))]
fn decode_platform_process_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn decode_platform_process_output(bytes: &[u8]) -> String {
    for code_page in windows_output_code_pages() {
        if let Some(decoded) = decode_windows_code_page(bytes, code_page) {
            return decoded;
        }
    }

    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn windows_output_code_pages() -> Vec<u32> {
    let mut code_pages = Vec::with_capacity(4);
    push_unique_code_page(&mut code_pages, unsafe { GetOEMCP() });
    push_unique_code_page(&mut code_pages, unsafe { GetACP() });
    code_pages
}

#[cfg(windows)]
fn push_unique_code_page(code_pages: &mut Vec<u32>, code_page: u32) {
    if code_page != 0 && !code_pages.contains(&code_page) {
        code_pages.push(code_page);
    }
}

#[cfg(windows)]
fn decode_windows_code_page(bytes: &[u8], code_page: u32) -> Option<String> {
    const MB_ERR_INVALID_CHARS: u32 = 0x0000_0008;

    let byte_count = i32::try_from(bytes.len()).ok()?;
    let wide_len = unsafe {
        MultiByteToWideChar(
            code_page,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            byte_count,
            std::ptr::null_mut(),
            0,
        )
    };
    if wide_len <= 0 {
        return None;
    }

    let mut wide = vec![0_u16; wide_len as usize];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            byte_count,
            wide.as_mut_ptr(),
            wide_len,
        )
    };
    if written <= 0 {
        return None;
    }

    String::from_utf16(&wide[..written as usize]).ok()
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetACP() -> u32;
    fn GetOEMCP() -> u32;
    fn MultiByteToWideChar(
        code_page: u32,
        flags: u32,
        multi_byte_str: *const u8,
        multi_byte_count: i32,
        wide_char_str: *mut u16,
        wide_char_count: i32,
    ) -> i32;
}

fn sanitize_file_stem(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "tool-output".to_string()
    } else {
        out
    }
}

fn ensure_child_path(root: &Path, child: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .or_else(|_| std::fs::create_dir_all(root).and_then(|_| root.canonicalize()))?;
    let parent = child
        .parent()
        .ok_or_else(|| MothershipError::Runtime("tool output path has no parent".to_string()))?;
    let parent = parent
        .canonicalize()
        .or_else(|_| std::fs::create_dir_all(parent).and_then(|_| parent.canonicalize()))?;
    if !parent.starts_with(&root) {
        return Err(MothershipError::Runtime(
            "tool output path escaped log root".to_string(),
        ));
    }
    Ok(())
}

/// Strip the `\n--- stdout/stderr N bytes ---\n` separators the writer
/// interleaves between appends, rejoining the surrounding content. The separator
/// is removed *as a whole* (including the leading newline the writer adds), so a
/// redactor flush that split a line across two appends is stitched back together
/// rather than leaving a spurious line break.
fn strip_stream_headers(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(text.len());
    let mut rest: &str = &text;
    // A seeked window can begin exactly at a header (its leading newline was the
    // previous chunk's cut point), so strip a header at position 0 too.
    for marker in ["--- stdout ", "--- stderr "] {
        if let Some(body) = rest.strip_prefix(marker) {
            if let Some(line_end) = body.find('\n') {
                if is_byte_count_header(&body[..line_end]) {
                    rest = &body[line_end + 1..];
                }
            }
            break;
        }
    }
    while !rest.is_empty() {
        // The separator always begins with a newline the writer prepends.
        let next = ["\n--- stdout ", "\n--- stderr "]
            .iter()
            .filter_map(|marker| rest.find(marker).map(|idx| (idx, marker.len())))
            .min_by_key(|&(idx, _)| idx);
        let Some((idx, prefix_len)) = next else {
            out.push_str(rest);
            break;
        };
        let after = &rest[idx + prefix_len..];
        if let Some(line_end) = after.find('\n') {
            if is_byte_count_header(&after[..line_end]) {
                // A real separator: keep the content before its leading newline,
                // then resume after the separator's trailing newline.
                out.push_str(&rest[..idx]);
                rest = &after[line_end + 1..];
                continue;
            }
        }
        // A false positive (e.g. literal "--- stdout " in content): emit through
        // the matched newline so we always make forward progress.
        out.push_str(&rest[..idx + 1]);
        rest = &rest[idx + 1..];
    }
    out
}

/// `true` for the middle of a writer separator, e.g. `1234 bytes ---`.
fn is_byte_count_header(middle: &str) -> bool {
    match middle.strip_suffix(" bytes ---") {
        Some(count) => !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Choose how many raw bytes of a read window to commit to, ending on a line
/// boundary so a chunk never splits a line. `to_eof` means the window already
/// reaches the end of the file, so we keep all of it.
fn pick_line_end(window: &[u8], cap: usize, to_eof: bool) -> usize {
    if to_eof {
        return window.len();
    }
    let limit = cap.min(window.len());
    match window[..limit].iter().rposition(|&b| b == b'\n') {
        Some(nl) => nl + 1,
        // One line longer than the cap: emit the capped prefix; the next fetch
        // continues from there. (UTF-8 is handled by `from_utf8_lossy` on strip.)
        None => limit,
    }
}

#[cfg(test)]
mod range_tests {
    use super::{pick_line_end, strip_stream_headers, FileToolOutputStore, ToolOutputStore};
    use crate::tools::types::ToolOutputStream;

    #[test]
    fn strip_stream_headers_drops_writer_separators() {
        let raw = "\n--- stdout 18 bytes ---\n     1\u{2192}package main\n".as_bytes();
        let content = strip_stream_headers(raw);
        assert!(!content.contains("--- stdout"));
        assert!(content.contains("\u{2192}package main"));
    }

    #[test]
    fn strip_stream_headers_rejoins_a_line_split_across_appends() {
        // A redactor flush split "  1→foo" mid-line, inserting a second header.
        let raw = "\n--- stdout 5 bytes ---\n  1\u{2192}fo\n--- stdout 3 bytes ---\no\n".as_bytes();
        let content = strip_stream_headers(raw);
        assert_eq!(content, "  1\u{2192}foo\n");
    }

    #[test]
    fn strip_stream_headers_drops_a_header_at_the_window_start() {
        // A seeked window can begin at a header with no leading newline.
        let raw = "--- stdout 6 bytes ---\nhello\n".as_bytes();
        assert_eq!(strip_stream_headers(raw), "hello\n");
    }

    #[test]
    fn pick_line_end_keeps_whole_window_at_eof() {
        assert_eq!(pick_line_end(b"aaaa\nbbbb\n", 1024, true), 10);
    }

    #[test]
    fn pick_line_end_cuts_on_a_line_boundary_within_cap() {
        // cap 7 over "aaaa\nbbbb\n": last newline within [0,7] is at index 4.
        assert_eq!(pick_line_end(b"aaaa\nbbbb\n", 7, false), 5);
    }

    #[tokio::test]
    async fn read_range_round_trips_and_pages_bounded() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mship_artifact_{nanos}"));
        let store = FileToolOutputStore::new(&root);

        let body: String = (1..=40)
            .map(|n| format!("{n:6}\u{2192}line {n}\n"))
            .collect();
        let log_ref = {
            let mut writer = store.open("call-1").await.unwrap();
            writer
                .append(ToolOutputStream::Stdout, body.as_bytes())
                .await
                .unwrap();
            writer.finish().await.unwrap()
        };

        // Full read: headers stripped, content intact, EOF reported.
        let full = store
            .read_range("call-1", &log_ref, 0, 1 << 20)
            .await
            .unwrap();
        assert!(!full.content.contains("--- stdout"));
        assert_eq!(full.content, body);
        assert!(full.eof);

        // Bounded paged read: the cap forces multiple chunks that reassemble to
        // the full content without splitting a line.
        let mut assembled = String::new();
        let mut offset = 0_u64;
        loop {
            let chunk = store
                .read_range("call-1", &log_ref, offset, 64)
                .await
                .unwrap();
            assembled.push_str(&chunk.content);
            match chunk.next_offset {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(assembled, body);

        std::fs::remove_dir_all(&root).ok();
    }
}
