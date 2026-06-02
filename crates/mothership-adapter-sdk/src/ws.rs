//! WebSocket transport session: a lazily-opened, reused-across-turns connection
//! to a backend, with idle disconnect.
//!
//! Requirements this satisfies (per the product owner):
//! - **lazy**: the socket opens on first [`send_text`](WsSession::send_text),
//!   not when the adapter spawns — so a Codex adapter that's never used opens
//!   nothing;
//! - **persistent**: the connection is kept and reused across chat turns;
//! - **idle disconnect**: [`close_if_idle`](WsSession::close_if_idle) closes the
//!   socket after a quiet period while leaving the owning adapter process warm;
//!   the next turn reconnects lazily;
//! - **per-process, never global**: each adapter process owns its own session,
//!   so multiple concurrent agents (resident + pool-spawned ephemeral adapters)
//!   never contend on one socket.
//!
//! It is a thin generic primitive: framing/semantics on top (e.g. the OpenAI
//! Responses beta protocol) live in the provider module.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Connection = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A reusable WebSocket connection that opens lazily and closes on idle.
pub struct WsSession {
    url: String,
    headers: Vec<(String, String)>,
    idle_after: Duration,
    connection: Option<Connection>,
    last_activity: Instant,
}

impl WsSession {
    /// Describe the connection without opening it. `idle_after` is how long the
    /// socket may sit unused before [`close_if_idle`](Self::close_if_idle) drops it.
    pub fn new(
        url: impl Into<String>,
        headers: Vec<(String, String)>,
        idle_after: Duration,
    ) -> Self {
        Self {
            url: url.into(),
            headers,
            idle_after,
            connection: None,
            last_activity: Instant::now(),
        }
    }

    /// Whether a live socket is currently held.
    pub fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    async fn ensure(&mut self) -> Result<&mut Connection> {
        if self.connection.is_none() {
            let mut request = self
                .url
                .as_str()
                .into_client_request()
                .context("build websocket request")?;
            for (name, value) in &self.headers {
                let name = HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("invalid header name: {name}"))?;
                let value = HeaderValue::from_str(value).context("invalid header value")?;
                request.headers_mut().insert(name, value);
            }
            let (stream, _response) = connect_async(request).await.context("websocket connect")?;
            self.connection = Some(stream);
        }
        self.last_activity = Instant::now();
        Ok(self
            .connection
            .as_mut()
            .expect("connection present after ensure"))
    }

    /// Send one text frame, connecting lazily if needed.
    pub async fn send_text(&mut self, text: String) -> Result<()> {
        let connection = self.ensure().await?;
        connection
            .send(Message::text(text))
            .await
            .context("websocket send")?;
        self.last_activity = Instant::now();
        Ok(())
    }

    /// Await the next text/binary message, bounded by `read_idle` so a stalled
    /// stream errors instead of hanging. `Ok(None)` means the peer closed.
    pub async fn next_text(&mut self, read_idle: Duration) -> Result<Option<String>> {
        loop {
            let connection = match self.connection.as_mut() {
                Some(connection) => connection,
                None => return Ok(None),
            };
            match tokio::time::timeout(read_idle, connection.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    self.last_activity = Instant::now();
                    return Ok(Some(text.to_string()));
                }
                Ok(Some(Ok(Message::Binary(bytes)))) => {
                    self.last_activity = Instant::now();
                    return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                    self.connection = None;
                    return Ok(None);
                }
                // Ping/Pong/Frame: keep waiting for real data.
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(error))) => {
                    self.connection = None;
                    return Err(error).context("websocket read");
                }
                Err(_) => bail!("websocket read idle for more than {read_idle:?}"),
            }
        }
    }

    /// Close the socket if it has been idle past `idle_after`, keeping the owning
    /// process warm. The next [`send_text`](Self::send_text) reconnects lazily.
    /// Call this from the SDK runtime's `on_idle` hook.
    pub async fn close_if_idle(&mut self) {
        if self.connection.is_some() && self.last_activity.elapsed() >= self.idle_after {
            if let Some(mut connection) = self.connection.take() {
                let _ = connection.close(None).await;
            }
        }
    }

    /// Force-close now (e.g. session marked dirty after a mid-stream failure).
    pub async fn close(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            let _ = connection.close(None).await;
        }
    }
}
