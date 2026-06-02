use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::{MothershipError, Result};

use super::types::{
    ToolExecutionEvent, ToolExecutionEventKind, ToolExecutionEventSink, ToolExecutionRequest,
    ToolOutputPolicy, ToolOutputStream,
};

#[async_trait::async_trait]
pub trait ToolOutputStore: Send + Sync {
    async fn open(&self, tool_call_id: &str) -> Result<Box<dyn ToolOutputWriter>>;
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
