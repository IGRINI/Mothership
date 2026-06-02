use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context as _};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::bridge::ToolBridgeClient;

const SERVER_NAME: &str = "mothership";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

pub(crate) async fn run_from_args(args: &[String]) -> anyhow::Result<()> {
    let config = BridgeArgs::parse(args)?;
    let bridge = ToolBridgeClient::new(config.port, config.token);
    run_stdio_server(bridge).await
}

struct BridgeArgs {
    port: u16,
    token: String,
}

impl BridgeArgs {
    fn parse(args: &[String]) -> anyhow::Result<Self> {
        let mut port = None;
        let mut token = None;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--port" => {
                    index += 1;
                    let value = args.get(index).context("missing --port value")?;
                    port = Some(value.parse::<u16>().context("invalid --port value")?);
                }
                "--token" => {
                    index += 1;
                    token = Some(args.get(index).context("missing --token value")?.clone());
                }
                other => bail!("unknown mcp-bridge argument `{other}`"),
            }
            index += 1;
        }

        Ok(Self {
            port: port.context("missing --port")?,
            token: token.context("missing --token")?,
        })
    }
}

async fn run_stdio_server(bridge: ToolBridgeClient) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    let counter = AtomicU64::new(1);

    while let Some(line) = reader.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(request) => handle_request(&bridge, &counter, request).await,
            Err(error) => Some(json_rpc_error(Value::Null, -32700, &error.to_string())),
        };

        if let Some(response) = response {
            stdout
                .write_all(serde_json::to_string(&response)?.as_bytes())
                .await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

async fn handle_request(
    bridge: &ToolBridgeClient,
    counter: &AtomicU64,
    request: JsonRpcRequest,
) -> Option<Value> {
    match request.method.as_str() {
        "initialize" => Some(initialize_response(request.id, request.params)),
        "notifications/initialized" => None,
        "tools/list" => Some(match bridge.list_tools().await {
            Ok(tools) => json_rpc_result(
                request.id,
                json!({
                    "tools": tools.into_iter().map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "inputSchema": tool.parameters,
                        })
                    }).collect::<Vec<_>>()
                }),
            ),
            Err(error) => json_rpc_error(request_id(request.id), -32000, &format!("{error:#}")),
        }),
        "tools/call" => Some(match parse_tool_call(request.params) {
            Ok((name, arguments)) => {
                let call_number = counter.fetch_add(1, Ordering::SeqCst);
                let tool_call_id = format!("mcp_{}_{}", std::process::id(), call_number);
                match bridge.call_tool(tool_call_id, name, arguments).await {
                    Ok(result) => json_rpc_result(
                        request.id,
                        json!({
                            "content": [{
                                "type": "text",
                                "text": result.content,
                            }],
                            "isError": !result.ok,
                        }),
                    ),
                    Err(error) => {
                        json_rpc_error(request_id(request.id), -32000, &format!("{error:#}"))
                    }
                }
            }
            Err(error) => json_rpc_error(request_id(request.id), -32602, &format!("{error:#}")),
        }),
        _ => Some(json_rpc_error(
            request_id(request.id),
            -32601,
            &format!("unknown method `{}`", request.method),
        )),
    }
}

fn initialize_response(id: Option<Value>, params: Value) -> Value {
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json_rpc_result(
        id,
        json!({
            "protocolVersion": protocol_version,
            "serverInfo": {
                "name": SERVER_NAME,
                "version": SERVER_VERSION,
            },
            "capabilities": {
                "tools": {
                    "listChanged": false,
                },
            },
        }),
    )
}

fn parse_tool_call(params: Value) -> anyhow::Result<(String, Value)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("tools/call params.name is required")?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok((name, arguments))
}

fn request_id(id: Option<Value>) -> Value {
    id.unwrap_or(Value::Null)
}

fn json_rpc_result(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request_id(id),
        "result": result,
    })
}

fn json_rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
}
