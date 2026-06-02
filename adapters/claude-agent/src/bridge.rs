use std::net::{SocketAddr, SocketAddrV4};
use std::time::Duration;

use anyhow::{bail, Context as _};
use mothership_adapter_sdk::protocol::{ToolCallResult, ToolDescriptor};
use mothership_adapter_sdk::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TOOL_TIMEOUT: Duration = Duration::from_secs(31 * 60);

pub(crate) struct ToolBridgeServer {
    address: SocketAddr,
    token: String,
    task: JoinHandle<()>,
}

impl ToolBridgeServer {
    pub(crate) async fn start(tools: Vec<ToolDescriptor>, ctx: Context) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(SocketAddrV4::new([127, 0, 0, 1].into(), 0))
            .await
            .context("bind Claude MCP tool bridge")?;
        let address = listener.local_addr()?;
        let token = random_token()?;
        let server_token = token.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    break;
                };
                let tools = tools.clone();
                let ctx = ctx.clone();
                let token = server_token.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, tools, ctx, token).await {
                        eprintln!("claude-agent bridge: {error:#}");
                    }
                });
            }
        });

        Ok(Self {
            address,
            token,
            task,
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.address.port()
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for ToolBridgeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ToolBridgeClient {
    port: u16,
    token: String,
}

impl ToolBridgeClient {
    pub(crate) fn new(port: u16, token: impl Into<String>) -> Self {
        Self {
            port,
            token: token.into(),
        }
    }

    pub(crate) async fn list_tools(&self) -> anyhow::Result<Vec<ToolDescriptor>> {
        let response = self
            .send(
                BridgeRequest::ListTools {
                    token: self.token.clone(),
                },
                LIST_TOOLS_TIMEOUT,
            )
            .await?;
        if response.ok {
            Ok(response.tools)
        } else {
            bail!(
                "{}",
                response
                    .error
                    .unwrap_or_else(|| "tool bridge list_tools failed".to_string())
            )
        }
    }

    pub(crate) async fn call_tool(
        &self,
        tool_call_id: String,
        name: String,
        arguments: Value,
    ) -> anyhow::Result<ToolCallResult> {
        let response = self
            .send(
                BridgeRequest::CallTool {
                    token: self.token.clone(),
                    tool_call_id,
                    name,
                    arguments,
                },
                CALL_TOOL_TIMEOUT,
            )
            .await?;
        if response.ok {
            response
                .result
                .ok_or_else(|| anyhow::anyhow!("tool bridge call_tool returned no result"))
        } else {
            Ok(ToolCallResult {
                ok: false,
                content: response
                    .error
                    .unwrap_or_else(|| "tool bridge call_tool failed".to_string()),
            })
        }
    }

    async fn send(
        &self,
        request: BridgeRequest,
        timeout: Duration,
    ) -> anyhow::Result<BridgeResponse> {
        let address = SocketAddrV4::new([127, 0, 0, 1].into(), self.port);
        tokio::time::timeout(timeout, async move {
            let mut stream = TcpStream::connect(address)
                .await
                .with_context(|| format!("connect Claude MCP tool bridge on {address}"))?;
            let line = serde_json::to_string(&request)?;
            stream.write_all(line.as_bytes()).await?;
            stream.write_all(b"\n").await?;
            stream.flush().await?;

            let mut lines = BufReader::new(stream).lines();
            let line = lines
                .next_line()
                .await?
                .ok_or_else(|| anyhow::anyhow!("tool bridge closed without response"))?;
            serde_json::from_str::<BridgeResponse>(&line).context("decode tool bridge response")
        })
        .await
        .context("tool bridge request timed out")?
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BridgeRequest {
    ListTools {
        token: String,
    },
    CallTool {
        token: String,
        tool_call_id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct BridgeResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    tools: Vec<ToolDescriptor>,
    #[serde(default)]
    result: Option<ToolCallResult>,
}

async fn handle_connection(
    stream: TcpStream,
    tools: Vec<ToolDescriptor>,
    ctx: Context,
    token: String,
) -> anyhow::Result<()> {
    let mut lines = BufReader::new(stream);
    let mut line = String::new();
    if lines.read_line(&mut line).await? == 0 {
        return Ok(());
    }

    let request = serde_json::from_str::<BridgeRequest>(line.trim())?;
    let response = match request {
        BridgeRequest::ListTools { token: got } => {
            if got == token {
                BridgeResponse {
                    ok: true,
                    error: None,
                    tools,
                    result: None,
                }
            } else {
                bridge_error("invalid bridge token")
            }
        }
        BridgeRequest::CallTool {
            token: got,
            tool_call_id,
            name,
            arguments,
        } => {
            if got == token {
                let result = ctx.request_tool(tool_call_id, name, arguments).await;
                BridgeResponse {
                    ok: true,
                    error: None,
                    tools: Vec::new(),
                    result: Some(result),
                }
            } else {
                bridge_error("invalid bridge token")
            }
        }
    };

    let mut stream = lines.into_inner();
    stream
        .write_all(serde_json::to_string(&response)?.as_bytes())
        .await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

fn bridge_error(message: impl Into<String>) -> BridgeResponse {
    BridgeResponse {
        ok: false,
        error: Some(message.into()),
        tools: Vec::new(),
        result: None,
    }
}

fn random_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generate Claude MCP bridge token: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
