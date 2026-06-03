use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context as _};
use mothership_adapter_sdk::protocol::{ReasoningConfig, ReasoningEffort};
use mothership_adapter_sdk::{ChatRequest, ChatRoundOutcome, ChatSink, Context as AdapterContext};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::auth::ClaudeAuthRuntime;
use crate::bridge::ToolBridgeServer;
use crate::model_catalog::{ClaudeModelCatalog, ClaudeModelInfo};
use crate::settings::ClaudeAgentSettings;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CLIENT_APP: &str = "mothership/0.1.0";
const SETTING_SOURCES: &str = "user,project";
const MODEL_METADATA_TIMEOUT: Duration = Duration::from_secs(20);
const HEADLESS_DISALLOWED_TOOLS: &[&str] = &[
    "AskUserQuestion",
    "CronCreate",
    "CronDelete",
    "CronList",
    "EnterPlanMode",
    "EnterWorktree",
    "ExitPlanMode",
    "ExitWorktree",
    "ScheduleWakeup",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAgentState {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    claude_code_version: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    requested_model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ControlEnvelope {
    response: ControlResponse,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
enum ControlResponse {
    Success {
        request_id: String,
        #[serde(default)]
        response: Value,
    },
    Error {
        request_id: String,
        error: String,
    },
}

#[derive(Debug, Deserialize)]
struct InitializeResponse {
    #[serde(default)]
    models: Vec<ClaudeModelInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsResponse {
    #[serde(default)]
    effective: Option<EffectiveSettings>,
    #[serde(default)]
    available_models: Option<Vec<String>>,
}

impl SettingsResponse {
    fn into_available_models(self) -> Option<Vec<String>> {
        self.effective
            .and_then(|settings| settings.available_models)
            .or(self.available_models)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffectiveSettings {
    #[serde(default)]
    available_models: Option<Vec<String>>,
}

pub(crate) async fn supported_models(
    settings: &ClaudeAgentSettings,
    ctx: &AdapterContext,
) -> anyhow::Result<ClaudeModelCatalog> {
    match tokio::time::timeout(
        MODEL_METADATA_TIMEOUT,
        supported_models_inner(settings, ctx),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => bail!(
            "Claude model metadata request timed out after {} seconds",
            MODEL_METADATA_TIMEOUT.as_secs()
        ),
    }
}

async fn supported_models_inner(
    settings: &ClaudeAgentSettings,
    ctx: &AdapterContext,
) -> anyhow::Result<ClaudeModelCatalog> {
    let executable = resolve_executable(settings)?;
    let auth = ClaudeAuthRuntime::prepare(settings)?;
    let mut command = claude_command(&executable, &auth);
    command.kill_on_drop(true);
    command
        .args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--tools",
            "",
            "--no-session-persistence",
            "--strict-mcp-config",
            "--setting-sources",
            SETTING_SOURCES,
            "--disable-slash-commands",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn Claude Agent SDK binary {}", executable.display()))?;

    let mut stdin = child.stdin.take().context("Claude process has no stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("Claude process has no stdout")?;
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(read_stderr(stderr));

    let request = json!({
        "type": "control_request",
        "request_id": "models",
        "request": {
            "subtype": "initialize",
            "skills": []
        }
    });
    stdin.write_all(request.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    let request = json!({
        "type": "control_request",
        "request_id": "settings",
        "request": {
            "subtype": "get_settings"
        }
    });
    stdin.write_all(request.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.shutdown().await?;

    let mut reader = BufReader::new(stdout).lines();
    let mut models = None;
    let mut available_models = None;
    let mut settings_seen = false;
    while let Some(line) = reader.next_line().await? {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("control_response") {
            continue;
        }
        let envelope: ControlEnvelope = serde_json::from_value(value)?;
        match envelope.response {
            ControlResponse::Success {
                request_id,
                response,
            } if request_id == "models" => {
                models = Some(
                    serde_json::from_value::<InitializeResponse>(response)
                        .context("decode Claude model metadata response")?
                        .models,
                );
                if settings_seen {
                    break;
                }
            }
            ControlResponse::Success {
                request_id,
                response,
            } if request_id == "settings" => {
                available_models = serde_json::from_value::<SettingsResponse>(response)
                    .ok()
                    .and_then(SettingsResponse::into_available_models);
                settings_seen = true;
                if models.is_some() {
                    break;
                }
            }
            ControlResponse::Error { request_id, error } if request_id == "models" => {
                bail!("Claude model metadata request failed: {error}");
            }
            ControlResponse::Error { request_id, .. } if request_id == "settings" => {
                settings_seen = true;
                if models.is_some() {
                    break;
                }
            }
            _ => {}
        }
    }

    if let Some(models) = models {
        let _ = child.kill().await;
        let _ = stderr_task.await.unwrap_or_default();
        auth.persist_refreshed_credentials(Some(ctx));
        return Ok(ClaudeModelCatalog {
            models,
            available_models,
        });
    }

    let status = child.wait().await?;
    let stderr = stderr_task.await.unwrap_or_default();
    if !status.success() {
        bail!(
            "Claude model metadata process exited with {status}: {}",
            stderr.trim()
        );
    }
    auth.persist_refreshed_credentials(Some(ctx));

    Ok(ClaudeModelCatalog {
        models: Vec::new(),
        available_models,
    })
}

pub(crate) async fn stream_chat(
    settings: &ClaudeAgentSettings,
    request: ChatRequest,
    ctx: &AdapterContext,
    sink: &mut ChatSink,
) -> anyhow::Result<ChatRoundOutcome> {
    let executable = resolve_executable(settings)?;
    let auth = ClaudeAuthRuntime::prepare(settings)?;
    let mut state = request
        .state
        .clone()
        .map(serde_json::from_value::<ClaudeAgentState>)
        .transpose()
        .context("decode Claude Agent SDK continuation state")?
        .unwrap_or_default();
    if state.requested_model.as_deref() != Some(request.model.as_str()) {
        state.session_id = None;
        state.claude_code_version = None;
        state.model = None;
    }
    let prompt = prompt_from_messages(&request.messages, state.session_id.is_some());

    let tool_bridge = if request.tools.is_empty() {
        None
    } else {
        Some(
            ToolBridgeServer::start(request.tools.clone(), ctx.clone())
                .await
                .context("start Claude MCP bridge for Mothership tools")?,
        )
    };

    let mut command = claude_command(&executable, &auth);
    command.kill_on_drop(true);
    let disallowed_tools = disallowed_tools_for_request(&request).join(",");
    command.args([
        "--print",
        "--input-format",
        "text",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--setting-sources",
        SETTING_SOURCES,
        "--tools",
        "default",
        "--disallowedTools",
        disallowed_tools.as_str(),
        "--permission-mode",
        "bypassPermissions",
        "--allow-dangerously-skip-permissions",
    ]);
    if let Some(tool_bridge) = tool_bridge.as_ref() {
        command
            .arg("--mcp-config")
            .arg(mcp_config_json(tool_bridge)?);
    }
    command.arg("--model").arg(&request.model);
    if let Some(session_id) = state.session_id.as_deref() {
        command.arg("--resume").arg(session_id);
    }
    if let Some(effort) = request.reasoning.as_ref().and_then(reasoning_effort) {
        command.arg("--effort").arg(effort);
    }
    let instructions = adapter_instructions(request.prompt.rendered_text(), tool_bridge.is_some());
    if !instructions.trim().is_empty() {
        command.arg("--append-system-prompt").arg(instructions);
    }
    command.arg(prompt);
    apply_runtime_context(&mut command, &request)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn Claude Agent SDK binary {}", executable.display()))?;
    let stdout = child
        .stdout
        .take()
        .context("Claude process has no stdout")?;
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(read_stderr(stderr));
    let mut reader = BufReader::new(stdout).lines();
    let mut assistant_text = String::new();
    let mut result_error = None::<String>;

    loop {
        tokio::select! {
            line = reader.next_line() => {
                let Some(line) = line? else { break };
                handle_chat_line(&line, sink, &mut assistant_text, &mut state, &mut result_error)?;
            }
            _ = sink.cancelled() => {
                let _ = child.kill().await;
                auth.persist_refreshed_credentials(Some(ctx));
                return Ok(ChatRoundOutcome::default());
            }
        }
    }

    let status = child.wait().await?;
    let stderr = stderr_task.await.unwrap_or_default();
    if !status.success() {
        bail!("Claude process exited with {status}: {}", stderr.trim());
    }
    if let Some(error) = result_error {
        bail!("Claude returned an error: {error}");
    }
    auth.persist_refreshed_credentials(Some(ctx));

    Ok(ChatRoundOutcome {
        state: Some(serde_json::to_value(ClaudeAgentState {
            requested_model: Some(request.model),
            ..state
        })?),
        tool_calls: Vec::new(),
    })
}

/// Mothership file tools that, when bridged to Claude (as `mcp__mothership__*`),
/// must supersede Claude's native file-mutation tools so every file change flows
/// through Mothership's approval/event/history pipeline rather than skipping it.
const MOTHERSHIP_FILE_TOOL_NAMES: &[&str] =
    &["read_file", "write_file", "edit_file", "apply_patch"];

/// Claude's native file-MUTATION tools. Disabled when the supervised Mothership
/// file tools are bridged, so writes cannot bypass Mothership's
/// approval/event/history pipeline.
const NATIVE_FILE_MUTATION_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// Claude's native file READER. Disabled when Mothership `read_file` is bridged so
/// every read also flows through the typed tool (uniform history/capabilities),
/// not just mutations — reads then go through `mcp__mothership__read_file`. Gated
/// on `read_file` specifically so the model is never left without a read path.
const NATIVE_FILE_READ_TOOLS: &[&str] = &["Read"];

/// Claude's native search tools. Disabled only when BOTH typed search tools
/// (`list_files` AND `search_text`) are bridged, so the model has a full typed
/// replacement for file discovery (`Glob` → `list_files`) and content search
/// (`Grep` → `search_text`) before either native tool is removed. If only one is
/// present, the native pair stays enabled so the model is never left without a
/// search path.
const NATIVE_SEARCH_TOOLS: &[&str] = &["Glob", "Grep"];

fn disallowed_tools_for_request(request: &ChatRequest) -> Vec<&'static str> {
    let mut tools = HEADLESS_DISALLOWED_TOOLS.to_vec();
    if request.tools.iter().any(|tool| tool.name == "run_command") {
        tools.push("Bash");
    }
    // When the Mothership file tools are present (and therefore bridged), disable
    // Claude's native file-mutation tools so file writes cannot bypass Mothership
    // approvals/events/history.
    if request
        .tools
        .iter()
        .any(|tool| MOTHERSHIP_FILE_TOOL_NAMES.contains(&tool.name.as_str()))
    {
        tools.extend_from_slice(NATIVE_FILE_MUTATION_TOOLS);
    }
    // Route reads through Mothership too: disable Claude's native reader when the
    // Mothership `read_file` tool is bridged, so every read is captured in typed
    // history/capabilities rather than skipping the pipeline. Gated on `read_file`
    // itself so we never strip the model's only read path.
    if request.tools.iter().any(|tool| tool.name == "read_file") {
        tools.extend_from_slice(NATIVE_FILE_READ_TOOLS);
    }
    // Route file discovery + content search through Mothership: disable Claude's
    // native `Glob`/`Grep` only when BOTH typed search tools are bridged (a full
    // typed replacement). With only one present, leave the native pair enabled so
    // the model always has a working search path.
    let has_list_files = request.tools.iter().any(|tool| tool.name == "list_files");
    let has_search_text = request.tools.iter().any(|tool| tool.name == "search_text");
    if has_list_files && has_search_text {
        tools.extend_from_slice(NATIVE_SEARCH_TOOLS);
    }
    tools
}

fn adapter_instructions(core_instructions: String, bridge_enabled: bool) -> String {
    if !bridge_enabled {
        return core_instructions;
    }

    let bridge_instructions = "When Mothership MCP tools are available, use them for local command execution and all file changes. Prefer `mcp__mothership__run_command` over direct shell tools, prefer the Mothership file tools (`mcp__mothership__read_file`, `mcp__mothership__write_file`, `mcp__mothership__edit_file`, `mcp__mothership__apply_patch`) over native file tools, and prefer the Mothership search tools (`mcp__mothership__list_files`, `mcp__mothership__search_text`) over native file discovery and content search, so Mothership can supervise cancellation, permissions, history, and output synchronization. Only fall back to provider-native tools when no Mothership tool can perform the task.";
    if core_instructions.trim().is_empty() {
        bridge_instructions.to_string()
    } else {
        format!("{core_instructions}\n\n{bridge_instructions}")
    }
}

fn mcp_config_json(tool_bridge: &ToolBridgeServer) -> anyhow::Result<String> {
    let adapter_exe = std::env::current_exe().context("resolve current adapter executable")?;
    Ok(json!({
        "mcpServers": {
            "mothership": {
                "type": "stdio",
                "command": adapter_exe,
                "args": [
                    "mcp-bridge",
                    "--port",
                    tool_bridge.port().to_string(),
                    "--token",
                    tool_bridge.token(),
                ],
                "timeout": 1800000,
                "alwaysLoad": true,
            }
        }
    })
    .to_string())
}

fn apply_runtime_context(command: &mut Command, request: &ChatRequest) -> anyhow::Result<()> {
    let Some(project_root) = request
        .runtime_context
        .project_root
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };

    let project_root = PathBuf::from(project_root);
    if !project_root.is_dir() {
        bail!(
            "active project root does not exist for Claude Agent SDK: {}",
            project_root.display()
        );
    }
    command.current_dir(project_root);
    Ok(())
}

fn handle_chat_line(
    line: &str,
    sink: &mut ChatSink,
    assistant_text: &mut String,
    state: &mut ClaudeAgentState,
    result_error: &mut Option<String>,
) -> anyhow::Result<()> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Ok(());
    };
    match value.get("type").and_then(Value::as_str) {
        Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
            state.session_id = value
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            state.claude_code_version = value
                .get("claude_code_version")
                .and_then(Value::as_str)
                .map(str::to_string);
            state.model = value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        Some("stream_event") => {
            if let Some(text) = value
                .get("event")
                .and_then(|event| event.get("delta"))
                .and_then(|delta| delta.get("text"))
                .and_then(Value::as_str)
            {
                sink.delta(text);
                assistant_text.push_str(text);
            }
        }
        Some("result") => {
            if value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                *result_error = value
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some("unknown Claude result error".to_string()));
            }
            if assistant_text.is_empty() {
                if let Some(text) = value.get("result").and_then(Value::as_str) {
                    sink.delta(text);
                    assistant_text.push_str(text);
                }
            }
            if state.session_id.is_none() {
                state.session_id = value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
        _ => {}
    }
    Ok(())
}

fn claude_command(executable: &Path, auth: &ClaudeAuthRuntime) -> Command {
    let mut command = Command::new(executable);
    command.env_clear();
    for (key, value) in inherited_child_env() {
        command.env(key, value);
    }
    auth.apply_to_command(&mut command);
    command.env("CLAUDE_AGENT_SDK_CLIENT_APP", CLIENT_APP);
    command.env("NO_COLOR", "1");
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

fn inherited_child_env() -> BTreeMap<String, String> {
    let allow = [
        "APPDATA",
        "COMSPEC",
        "HOME",
        "HOMEDRIVE",
        "HOMEPATH",
        "LOCALAPPDATA",
        "PATH",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "SYSTEMDRIVE",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "USERDOMAIN",
        "USERNAME",
        "USERPROFILE",
        "WINDIR",
    ];
    allow
        .into_iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| (key.to_string(), value))
        })
        .collect()
}

async fn read_stderr(stderr: Option<tokio::process::ChildStderr>) -> String {
    let Some(stderr) = stderr else {
        return String::new();
    };
    let mut reader = BufReader::new(stderr).lines();
    let mut output = String::new();
    while let Ok(Some(line)) = reader.next_line().await {
        output.push_str(&line);
        output.push('\n');
    }
    output
}

fn reasoning_effort(reasoning: &ReasoningConfig) -> Option<&'static str> {
    match reasoning.effort? {
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::XHigh => Some("xhigh"),
        ReasoningEffort::Max => Some("max"),
        ReasoningEffort::None | ReasoningEffort::Minimal => None,
    }
}

fn prompt_from_messages(
    messages: &[mothership_adapter_sdk::protocol::ChatMessage],
    resumed: bool,
) -> String {
    if resumed {
        return messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.content.clone())
            .unwrap_or_else(|| "Continue.".to_string());
    }

    if messages.len() == 1 {
        return messages[0].content.clone();
    }

    let mut prompt =
        String::from("Conversation context from Mothership before this Claude session:\n\n");
    for message in messages {
        let role = match message.role.as_str() {
            "assistant" => "Assistant",
            _ => "User",
        };
        prompt.push_str(role);
        prompt.push_str(":\n");
        prompt.push_str(&message.content);
        prompt.push_str("\n\n");
    }
    prompt.push_str("Continue by answering the latest user message.");
    prompt
}

fn resolve_executable(settings: &ClaudeAgentSettings) -> anyhow::Result<PathBuf> {
    if let Some(path) = settings.executable_path() {
        if path.is_file() {
            return Ok(path.clone());
        }
        bail!(
            "configured Claude executable does not exist: {}",
            path.display()
        );
    }
    if let Ok(path) = std::env::var("MOTHERSHIP_CLAUDE_AGENT_CLI") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Some(path) = bundled_sibling_binary() {
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Some(path) = workspace_node_modules_binary() {
        if path.is_file() {
            return Ok(path);
        }
    }

    bail!("Claude Agent SDK executable was not found; rebuild sidecar binaries or configure an executable path")
}

fn bundled_sibling_binary() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let directory = current.parent()?;
    let file_name = current.file_name()?.to_str()?;
    let adapter_prefix = "claude-agent-adapter-";
    if let Some(target_suffix) = file_name.strip_prefix(adapter_prefix) {
        return Some(directory.join(format!("claude-agent-sdk-cli-{target_suffix}")));
    }
    Some(directory.join(format!(
        "claude-agent-sdk-cli{}",
        std::env::consts::EXE_SUFFIX
    )))
}

fn workspace_node_modules_binary() -> Option<PathBuf> {
    let package = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "@anthropic-ai/claude-agent-sdk-win32-x64",
        ("windows", "aarch64") => "@anthropic-ai/claude-agent-sdk-win32-arm64",
        ("macos", "x86_64") => "@anthropic-ai/claude-agent-sdk-darwin-x64",
        ("macos", "aarch64") => "@anthropic-ai/claude-agent-sdk-darwin-arm64",
        ("linux", "x86_64") => "@anthropic-ai/claude-agent-sdk-linux-x64",
        ("linux", "aarch64") => "@anthropic-ai/claude-agent-sdk-linux-arm64",
        _ => return None,
    };
    let binary = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    std::env::current_dir()
        .ok()
        .map(|cwd| cwd.join("node_modules").join(package).join(binary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mothership_adapter_sdk::protocol::ToolDescriptor;

    fn tool(name: &str) -> ToolDescriptor {
        ToolDescriptor {
            id: format!("core.{name}"),
            name: name.to_string(),
            description: String::new(),
            parameters: json!({}),
            strict: false,
            annotations: BTreeMap::new(),
        }
    }

    fn request_with_tools(names: &[&str]) -> ChatRequest {
        ChatRequest {
            model: "claude".to_string(),
            reasoning: None,
            prompt: Default::default(),
            runtime_context: Default::default(),
            messages: Vec::new(),
            tools: names.iter().map(|name| tool(name)).collect(),
            state: None,
            tool_results: Vec::new(),
            extra_messages: Vec::new(),
        }
    }

    #[test]
    fn disallows_native_file_mutation_tools_when_file_tools_present() {
        // With the Mothership file tools bridged, Claude's native file-mutation
        // tools must be disabled so writes cannot bypass Mothership supervision.
        let request = request_with_tools(&[
            "run_command",
            "read_file",
            "write_file",
            "edit_file",
            "apply_patch",
        ]);
        let disallowed = disallowed_tools_for_request(&request);

        for native in NATIVE_FILE_MUTATION_TOOLS {
            assert!(
                disallowed.contains(native),
                "expected `{native}` to be disallowed when file tools are bridged"
            );
        }
        // Bash is also disabled because run_command is present.
        assert!(disallowed.contains(&"Bash"));
        // Native `Read` is disabled too (read_file is bridged), so reads also flow
        // through Mothership for uniform history/capabilities.
        assert!(
            disallowed.contains(&"Read"),
            "native Read must be disabled when read_file is bridged"
        );
    }

    #[test]
    fn keeps_native_read_when_read_file_not_bridged() {
        // Mutation tools bridged but no read_file: native Read must stay enabled so
        // the model is never left without any way to read a file.
        let request = request_with_tools(&["run_command", "edit_file"]);
        let disallowed = disallowed_tools_for_request(&request);
        assert!(disallowed.contains(&"Edit"));
        assert!(
            !disallowed.contains(&"Read"),
            "native Read must stay enabled when read_file is not bridged"
        );
    }

    #[test]
    fn keeps_native_file_tools_when_no_mothership_file_tools() {
        // If only run_command is offered (no bridged file tools), native file
        // tools must stay enabled so Claude can still edit.
        let request = request_with_tools(&["run_command"]);
        let disallowed = disallowed_tools_for_request(&request);

        for native in NATIVE_FILE_MUTATION_TOOLS {
            assert!(
                !disallowed.contains(native),
                "`{native}` must stay enabled when no Mothership file tools are present"
            );
        }
        assert!(disallowed.contains(&"Bash"));
    }

    #[test]
    fn presence_of_any_single_file_tool_triggers_native_disable() {
        // Even one bridged Mothership file tool is enough to supersede the native
        // mutation tools.
        let request = request_with_tools(&["edit_file"]);
        let disallowed = disallowed_tools_for_request(&request);
        assert!(disallowed.contains(&"Edit"));
        assert!(disallowed.contains(&"Write"));
        // No run_command here, so Bash stays enabled.
        assert!(!disallowed.contains(&"Bash"));
    }

    #[test]
    fn disallows_native_search_tools_only_when_both_search_tools_present() {
        // Both typed search tools bridged: native Glob/Grep are a full typed
        // replacement and must be disabled.
        let request = request_with_tools(&["list_files", "search_text"]);
        let disallowed = disallowed_tools_for_request(&request);
        for native in NATIVE_SEARCH_TOOLS {
            assert!(
                disallowed.contains(native),
                "expected `{native}` to be disallowed when both search tools are bridged"
            );
        }
    }

    #[test]
    fn keeps_native_search_tools_when_only_one_search_tool_present() {
        // Only list_files bridged: native search must stay enabled (no full
        // replacement).
        let request = request_with_tools(&["list_files"]);
        let disallowed = disallowed_tools_for_request(&request);
        for native in NATIVE_SEARCH_TOOLS {
            assert!(
                !disallowed.contains(native),
                "`{native}` must stay enabled when only one search tool is bridged"
            );
        }

        // Only search_text bridged: same.
        let request = request_with_tools(&["search_text"]);
        let disallowed = disallowed_tools_for_request(&request);
        for native in NATIVE_SEARCH_TOOLS {
            assert!(
                !disallowed.contains(native),
                "`{native}` must stay enabled when only one search tool is bridged"
            );
        }
    }

    #[test]
    fn keeps_native_search_tools_when_no_search_tools_present() {
        // No typed search tools: native Glob/Grep stay enabled.
        let request = request_with_tools(&["run_command", "read_file"]);
        let disallowed = disallowed_tools_for_request(&request);
        for native in NATIVE_SEARCH_TOOLS {
            assert!(
                !disallowed.contains(native),
                "`{native}` must stay enabled when no search tools are present"
            );
        }
    }
}
