//! Server-Sent-Events transport: shared streaming for adapters (the OpenAI
//! family, OpenRouter, anything SSE). Also the fallback half of a WS-primary
//! adapter, so a flaky WebSocket degrades to a working stream rather than a dead
//! chat.
//!
//! [`SseDecoder`] turns raw byte chunks (which split anywhere, mid-line) into the
//! `data:` payloads of complete events. [`read_sse`] drives a streaming HTTP
//! response through it with a per-read idle timeout, so a stalled stream fails
//! instead of hanging forever.

use std::time::Duration;

use anyhow::{bail, Result};

/// Incremental SSE line decoder. Feed it byte chunks; it returns the `data:`
/// payloads of any complete lines, buffering partial lines across chunks.
/// Comment lines (`:`), blank lines, and non-`data:` fields are skipped. The
/// `[DONE]` sentinel is returned as a payload; the caller decides what it means.
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut payloads = Vec::new();
        while let Some(newline) = self.buf.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]);
            let line = line.trim_end_matches('\r').trim();
            if line.is_empty() {
                continue;
            }
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if !data.is_empty() {
                    payloads.push(data.to_string());
                }
            }
            // Non-data fields (event:/id:/retry:) and comments (`:`) are ignored.
        }
        payloads
    }
}

/// Drives a streaming HTTP response through [`SseDecoder`], invoking `on_event`
/// with each `data:` payload. `idle` bounds the wait for the next byte chunk, so
/// a stalled stream errors instead of hanging. `on_event` returns `Ok(false)` to
/// stop early (e.g. on a terminal event); an `Err` aborts the stream.
pub async fn read_sse<F>(
    mut response: reqwest::Response,
    idle: Duration,
    mut on_event: F,
) -> Result<()>
where
    F: FnMut(&str) -> Result<bool>,
{
    let mut decoder = SseDecoder::new();
    loop {
        match tokio::time::timeout(idle, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                for payload in decoder.push(chunk.as_ref()) {
                    if !on_event(&payload)? {
                        return Ok(());
                    }
                }
            }
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => bail!("SSE stream idle for more than {idle:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SseDecoder;

    #[test]
    fn extracts_complete_events() {
        let mut decoder = SseDecoder::new();
        let out = decoder.push(b"data: {\"a\":1}\ndata: {\"b\":2}\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]);
    }

    #[test]
    fn buffers_partial_lines_across_chunks() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: {\"hel").is_empty());
        assert!(decoder.push(b"lo\":true}").is_empty()); // no newline yet
        let out = decoder.push(b"\n");
        assert_eq!(out, vec!["{\"hello\":true}".to_string()]);
    }

    #[test]
    fn skips_comments_blanks_and_other_fields() {
        let mut decoder = SseDecoder::new();
        let out = decoder.push(b": keep-alive\n\nevent: ping\ndata: payload\r\n");
        assert_eq!(out, vec!["payload".to_string()]);
    }

    #[test]
    fn surfaces_done_sentinel_to_caller() {
        let mut decoder = SseDecoder::new();
        let out = decoder.push(b"data: [DONE]\n");
        assert_eq!(out, vec!["[DONE]".to_string()]);
    }
}
