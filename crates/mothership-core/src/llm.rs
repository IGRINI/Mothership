use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    net::TcpStream,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{blocking::Client, header::HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tungstenite::{client::IntoClientRequest, connect, stream::MaybeTlsStream, Message};

use crate::auth::{
    refresh_codex_token_with_client, CodexCredentialPayload, ConnectionStatus, CredentialVault,
    OpenAiCodexGateway, OpenAiCodexOAuthAdapter, ProviderConnection, SecretMaterial, SecretPayload,
    StoreCredentialRequest,
};
use crate::{MothershipError, Result};

const OPENAI_CODEX_MODELS_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/models";
const MODEL_CATALOG_CACHE_TTL: Duration = Duration::from_secs(300);
const MODEL_CATALOG_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const CHAT_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CHAT_HTTP_JSON_TIMEOUT: Duration = Duration::from_secs(180);
const CHAT_HTTP_STREAM_TIMEOUT: Duration = Duration::from_secs(45);
const CHAT_WEBSOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.133.0";
const DEFAULT_CODEX_CHAT_SYSTEM_PROMPT: &str = r#"You are Mothership's local AI coding assistant.
Answer in the user's language unless the user asks otherwise.
Be direct, technically precise, and practical.
Use only the conversation context available in this request.
Do not claim that you edited files, ran commands, opened applications, or inspected the local machine unless that information is present in the conversation context.
When code or commands are useful, provide concrete, executable examples."#;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmModel {
    pub provider_id: String,
    pub provider_label: String,
    pub id: String,
    pub label: String,
    pub family: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub recommended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SelectedLlmModel {
    pub provider_id: String,
    pub model_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorSettingsSchema {
    pub model_management: ConnectorModelManagementSchema,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorModelManagementSchema {
    pub kind: ConnectorModelManagementKind,
    pub title: String,
    pub description: String,
    pub add_model_label: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorModelManagementKind {
    FixedCatalog,
    RemoteCatalog,
    EditableList,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmModelCatalogCache {
    pub provider_id: String,
    pub models: Vec<LlmModel>,
    pub etag: Option<String>,
    pub fetched_at: String,
    pub expires_at: String,
}

impl LlmModelCatalogCache {
    pub fn is_fresh(&self) -> bool {
        parse_timestamp(&self.expires_at)
            .map(|expires_at| expires_at > unix_timestamp_secs())
            .unwrap_or(false)
    }
}

pub trait LlmModelCatalogRepository: Send + Sync {
    fn load_llm_model_catalog_cache(
        &self,
        provider_id: &str,
    ) -> Result<Option<LlmModelCatalogCache>>;

    fn save_llm_model_catalog_cache(&self, cache: &LlmModelCatalogCache) -> Result<()>;
}

pub struct RemoteModelCatalogInput<'a> {
    pub connection: &'a ProviderConnection,
    pub secret: &'a SecretMaterial,
}

pub struct RemoteModelCatalog {
    pub models: Vec<LlmModel>,
    pub etag: Option<String>,
    pub refreshed_secret: Option<SecretMaterial>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmChatCompletionRequest {
    pub provider_id: String,
    pub model_id: String,
    pub system_prompt: String,
    pub messages: Vec<LlmChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmChatMessage {
    pub role: LlmChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LlmChatRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LlmTransportKind {
    WebSocket,
    HttpSse,
    HttpJson,
}

impl LlmTransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::HttpSse => "http_sse",
            Self::HttpJson => "http_json",
        }
    }
}

pub trait LlmChatCompletionEventSink {
    fn transport_selected(&mut self, transport: LlmTransportKind);

    fn delta(&mut self, delta: &str);
}

pub trait LlmChatCompletionGateway: Send + Sync {
    fn complete_chat(
        &self,
        request: LlmChatCompletionRequest,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<String>;
}

type LlmTransportAttemptResult<T> = std::result::Result<T, LlmTransportAttemptError>;

#[derive(Debug)]
struct LlmTransportAttemptError {
    error: MothershipError,
    allow_fallback: bool,
}

impl LlmTransportAttemptError {
    fn retryable(error: MothershipError) -> Self {
        Self {
            error,
            allow_fallback: true,
        }
    }

    fn committed(error: MothershipError) -> Self {
        Self {
            error,
            allow_fallback: false,
        }
    }

    fn retryable_message(message: impl Into<String>) -> Self {
        Self::retryable(MothershipError::InvalidRequest(message.into()))
    }

    fn committed_message(message: impl Into<String>) -> Self {
        Self::committed(MothershipError::InvalidRequest(message.into()))
    }
}

impl From<MothershipError> for LlmTransportAttemptError {
    fn from(error: MothershipError) -> Self {
        Self::retryable(error)
    }
}

pub trait LlmConnectorAdapter: Send + Sync {
    fn provider_id(&self) -> &'static str;

    fn provider_label(&self) -> &'static str;

    fn bundled_models(&self) -> Vec<LlmModel>;

    fn chat_system_prompt(&self, _model_id: &str) -> Option<String> {
        None
    }

    fn remote_model_catalog(
        &self,
        _input: RemoteModelCatalogInput<'_>,
    ) -> Result<Option<RemoteModelCatalog>> {
        Ok(None)
    }

    fn settings_schema(&self) -> ConnectorSettingsSchema;
}

pub struct StaticLlmConnectorRegistry {
    adapters: Vec<Box<dyn LlmConnectorAdapter>>,
}

impl StaticLlmConnectorRegistry {
    pub fn new(adapters: Vec<Box<dyn LlmConnectorAdapter>>) -> Self {
        Self { adapters }
    }

    pub fn with_openai_codex() -> Self {
        Self::new(vec![Box::new(OpenAiCodexLlmConnector)])
    }

    pub fn list_models(&self) -> Vec<LlmModel> {
        self.list_bundled_models()
    }

    pub fn list_bundled_models(&self) -> Vec<LlmModel> {
        self.adapters
            .iter()
            .flat_map(|adapter| adapter.bundled_models())
            .collect()
    }

    pub fn find_model(&self, provider_id: &str, model_id: &str) -> Option<LlmModel> {
        self.adapters
            .iter()
            .find(|adapter| adapter.provider_id() == provider_id)
            .and_then(|adapter| {
                adapter
                    .bundled_models()
                    .into_iter()
                    .find(|model| model.id == model_id)
            })
    }

    pub fn settings_schema(&self, provider_id: &str) -> Option<ConnectorSettingsSchema> {
        self.adapters
            .iter()
            .find(|adapter| adapter.provider_id() == provider_id)
            .map(|adapter| adapter.settings_schema())
    }

    pub fn provider_label(&self, provider_id: &str) -> Option<&'static str> {
        self.adapters
            .iter()
            .find(|adapter| adapter.provider_id() == provider_id)
            .map(|adapter| adapter.provider_label())
    }

    pub fn chat_system_prompt(&self, provider_id: &str, model_id: &str) -> Option<String> {
        self.adapters
            .iter()
            .find(|adapter| adapter.provider_id() == provider_id)
            .and_then(|adapter| adapter.chat_system_prompt(model_id))
    }
}

pub struct LlmModelCatalogService<'a> {
    repository: &'a dyn LlmModelCatalogRepository,
    vault: &'a dyn CredentialVault,
    registry: &'a StaticLlmConnectorRegistry,
}

impl<'a> LlmModelCatalogService<'a> {
    pub fn new(
        repository: &'a dyn LlmModelCatalogRepository,
        vault: &'a dyn CredentialVault,
        registry: &'a StaticLlmConnectorRegistry,
    ) -> Self {
        Self {
            repository,
            vault,
            registry,
        }
    }

    pub fn list_models(&self, connections: &[ProviderConnection]) -> Result<Vec<LlmModel>> {
        let mut models = Vec::new();

        for adapter in &self.registry.adapters {
            models.extend(self.models_for_adapter(adapter.as_ref(), connections)?);
        }

        Ok(models)
    }

    fn models_for_adapter(
        &self,
        adapter: &dyn LlmConnectorAdapter,
        connections: &[ProviderConnection],
    ) -> Result<Vec<LlmModel>> {
        let provider_id = adapter.provider_id();
        let model_management = adapter.settings_schema().model_management.kind;

        if model_management != ConnectorModelManagementKind::RemoteCatalog {
            return Ok(adapter.bundled_models());
        }

        let Some(connection) = active_connection(connections, provider_id) else {
            return Ok(Vec::new());
        };

        let cached = self.repository.load_llm_model_catalog_cache(provider_id)?;
        if let Some(cache) = cached
            .as_ref()
            .filter(|cache| cache.is_fresh() && !cache.models.is_empty())
        {
            return Ok(cache.models.clone());
        }

        let catalog = match self.load_remote_catalog(adapter, connection) {
            Ok(Some(catalog)) => catalog,
            Ok(None) => {
                return Err(MothershipError::InvalidRequest(format!(
                    "connector does not implement remote model catalog: {provider_id}"
                )))
            }
            // Remote fetch failed (e.g. transient network error after retries). Fall
            // back to the last cached catalog if we have one — even if stale — rather
            // than failing the whole connector view with an error.
            Err(error) => {
                if let Some(cache) = cached.as_ref().filter(|cache| !cache.models.is_empty()) {
                    return Ok(cache.models.clone());
                }
                return Err(error);
            }
        };

        if catalog.models.is_empty() {
            return Err(MothershipError::InvalidRequest(format!(
                "remote model catalog returned no models: {provider_id}"
            )));
        }

        let now = unix_timestamp_secs();
        self.repository
            .save_llm_model_catalog_cache(&LlmModelCatalogCache {
                provider_id: provider_id.to_string(),
                models: catalog.models.clone(),
                etag: catalog.etag,
                fetched_at: now.to_string(),
                expires_at: now
                    .saturating_add(MODEL_CATALOG_CACHE_TTL.as_secs())
                    .to_string(),
            })?;

        if let Some(secret) = catalog.refreshed_secret {
            self.vault.replace(
                &connection.credential_ref.vault_handle,
                StoreCredentialRequest {
                    provider_id: connection.provider_id.clone(),
                    credential: secret,
                },
            )?;
        }

        Ok(catalog.models)
    }

    fn load_remote_catalog(
        &self,
        adapter: &dyn LlmConnectorAdapter,
        connection: &ProviderConnection,
    ) -> Result<Option<RemoteModelCatalog>> {
        let secret = self.vault.load(&connection.credential_ref.vault_handle)?;
        adapter.remote_model_catalog(RemoteModelCatalogInput {
            connection,
            secret: &secret,
        })
    }
}

#[derive(Debug)]
pub struct OpenAiCodexLlmConnector;

impl Default for OpenAiCodexLlmConnector {
    fn default() -> Self {
        Self
    }
}

pub struct OpenAiCodexChatCompletionGateway<'a> {
    auth_gateway: OpenAiCodexGateway<'a>,
    connection: &'a ProviderConnection,
    streaming_client: Client,
    json_client: Client,
}

impl<'a> OpenAiCodexChatCompletionGateway<'a> {
    pub fn new(vault: &'a dyn CredentialVault, connection: &'a ProviderConnection) -> Result<Self> {
        Ok(Self {
            auth_gateway: OpenAiCodexGateway::new(vault),
            connection,
            streaming_client: chat_streaming_http_client()?,
            json_client: chat_json_http_client()?,
        })
    }
}

impl LlmChatCompletionGateway for OpenAiCodexChatCompletionGateway<'_> {
    fn complete_chat(
        &self,
        request: LlmChatCompletionRequest,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<String> {
        if request.provider_id != OpenAiCodexOAuthAdapter::PROVIDER_ID {
            return Err(MothershipError::InvalidRequest(format!(
                "unsupported chat provider: {}",
                request.provider_id
            )));
        }
        if request.system_prompt.trim().is_empty() {
            return Err(MothershipError::InvalidRequest(
                "LLM request system prompt is required".to_string(),
            ));
        }

        let prepared = self
            .auth_gateway
            .prepare_request(self.connection, "https://api.openai.com/v1/responses")?;
        let mut failures = Vec::new();

        match self.complete_via_websocket(&prepared.url, &prepared.headers, &request, sink) {
            Ok(text) => return Ok(text),
            Err(error) => {
                failures.push(format!("websocket: {}", error.error));
                if !error.allow_fallback {
                    return Err(MothershipError::InvalidRequest(format!(
                        "LLM request failed: {}",
                        failures.join("; ")
                    )));
                }
            }
        }

        match self.complete_via_http_sse(&prepared.url, &prepared.headers, &request, sink) {
            Ok(text) => return Ok(text),
            Err(error) => {
                failures.push(format!("http_sse: {}", error.error));
                if !error.allow_fallback {
                    return Err(MothershipError::InvalidRequest(format!(
                        "LLM request failed: {}",
                        failures.join("; ")
                    )));
                }
            }
        }

        match self.complete_via_http_json(&prepared.url, &prepared.headers, &request, sink) {
            Ok(text) => {
                if !text.is_empty() {
                    sink.delta(&text);
                }
                Ok(text)
            }
            Err(error) => {
                failures.push(format!("http_json: {error}"));
                Err(MothershipError::InvalidRequest(format!(
                    "LLM request failed: {}",
                    failures.join("; ")
                )))
            }
        }
    }
}

impl OpenAiCodexChatCompletionGateway<'_> {
    fn complete_via_websocket(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        request: &LlmChatCompletionRequest,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> LlmTransportAttemptResult<String> {
        let websocket_url = websocket_url(url)?;
        let mut websocket_request = websocket_url.into_client_request().map_err(|error| {
            LlmTransportAttemptError::retryable_message(format!(
                "invalid WebSocket request: {error}"
            ))
        })?;

        for (name, value) in headers {
            websocket_request.headers_mut().insert(
                header_name(name)?,
                tungstenite::http::HeaderValue::from_str(value).map_err(|error| {
                    LlmTransportAttemptError::retryable_message(format!(
                        "invalid WebSocket header: {error}"
                    ))
                })?,
            );
        }

        let (mut socket, _) = connect(websocket_request).map_err(|error| {
            LlmTransportAttemptError::retryable_message(format!(
                "WebSocket connect failed: {error}"
            ))
        })?;
        configure_websocket_timeouts(socket.get_mut())?;
        sink.transport_selected(LlmTransportKind::WebSocket);

        let payload = serde_json::to_string(&websocket_body(request)).map_err(|error| {
            LlmTransportAttemptError::retryable_message(format!(
                "invalid WebSocket request body: {error}"
            ))
        })?;
        socket
            .send(Message::Text(payload.into()))
            .map_err(|error| {
                LlmTransportAttemptError::retryable_message(format!(
                    "WebSocket send failed: {error}"
                ))
            })?;

        let mut output = String::new();
        loop {
            let message = socket.read().map_err(|error| {
                if output.is_empty() {
                    LlmTransportAttemptError::retryable_message(format!(
                        "WebSocket read failed: {error}"
                    ))
                } else {
                    LlmTransportAttemptError::committed_message(format!(
                        "WebSocket stream failed after partial output: {error}"
                    ))
                }
            })?;

            match message {
                Message::Text(text) => {
                    if handle_transport_event(&text, &mut output, sink)? {
                        return Ok(output);
                    }
                }
                Message::Binary(bytes) => {
                    let text = String::from_utf8(bytes.to_vec()).map_err(|error| {
                        LlmTransportAttemptError::retryable_message(format!(
                            "invalid WebSocket UTF-8 payload: {error}"
                        ))
                    })?;
                    if handle_transport_event(&text, &mut output, sink)? {
                        return Ok(output);
                    }
                }
                Message::Close(_) => {
                    if output.is_empty() {
                        return Err(LlmTransportAttemptError::retryable_message(
                            "WebSocket closed before response completed".to_string(),
                        ));
                    }
                    return Ok(output);
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }

    fn complete_via_http_sse(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        request: &LlmChatCompletionRequest,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> LlmTransportAttemptResult<String> {
        let mut request_headers = reqwest_headers(headers)?;
        request_headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/event-stream"),
        );

        let response = self
            .streaming_client
            .post(url)
            .headers(request_headers)
            .json(&http_body(request, true))
            .send()
            .map_err(|error| {
                LlmTransportAttemptError::retryable_message(format!(
                    "HTTP SSE request failed: {error}"
                ))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(LlmTransportAttemptError::retryable_message(format!(
                "HTTP SSE request rejected: {status}: {}",
                sanitize_provider_error(&body)
            )));
        }

        sink.transport_selected(LlmTransportKind::HttpSse);
        let mut output = String::new();
        let reader = BufReader::new(response);

        for line in reader.lines() {
            let line = line.map_err(|error| {
                if output.is_empty() {
                    LlmTransportAttemptError::retryable_message(format!(
                        "HTTP SSE read failed: {error}"
                    ))
                } else {
                    LlmTransportAttemptError::committed_message(format!(
                        "HTTP SSE stream failed after partial output: {error}"
                    ))
                }
            })?;
            let line = line.trim();
            if !line.starts_with("data:") {
                continue;
            }

            let data = line.trim_start_matches("data:").trim();
            if data == "[DONE]" {
                return Ok(output);
            }

            if handle_transport_event(data, &mut output, sink)? {
                return Ok(output);
            }
        }

        if output.is_empty() {
            Err(LlmTransportAttemptError::retryable_message(
                "HTTP SSE stream ended without text".to_string(),
            ))
        } else {
            Ok(output)
        }
    }

    fn complete_via_http_json(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        request: &LlmChatCompletionRequest,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<String> {
        sink.transport_selected(LlmTransportKind::HttpJson);
        let response = self
            .json_client
            .post(url)
            .headers(reqwest_headers(headers)?)
            .json(&http_body(request, false))
            .send()
            .map_err(|error| {
                MothershipError::InvalidRequest(format!("HTTP JSON request failed: {error}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(MothershipError::InvalidRequest(format!(
                "HTTP JSON request rejected: {status}: {}",
                sanitize_provider_error(&body)
            )));
        }

        let value: serde_json::Value = response.json().map_err(|error| {
            MothershipError::InvalidRequest(format!("invalid HTTP JSON response: {error}"))
        })?;

        extract_response_text(&value).ok_or_else(|| {
            MothershipError::InvalidRequest("HTTP JSON response did not contain text".to_string())
        })
    }
}

fn http_body(request: &LlmChatCompletionRequest, stream: bool) -> serde_json::Value {
    json!({
        "model": request.model_id,
        "instructions": request.system_prompt,
        "input": response_input(&request.messages),
        "stream": stream,
        "store": false,
    })
}

fn websocket_body(request: &LlmChatCompletionRequest) -> serde_json::Value {
    json!({
        "type": "response.create",
        "model": request.model_id,
        "instructions": request.system_prompt,
        "input": response_input(&request.messages),
        "store": false,
    })
}

fn response_input(messages: &[LlmChatMessage]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter(|message| !message.content.trim().is_empty())
        .map(|message| match message.role {
            LlmChatRole::User => json!({
                "role": "user",
                "content": [{"type": "input_text", "text": message.content}],
            }),
            LlmChatRole::Assistant => json!({
                "role": "assistant",
                "content": [{"type": "output_text", "text": message.content}],
            }),
        })
        .collect()
}

fn handle_transport_event(
    data: &str,
    output: &mut String,
    sink: &mut dyn LlmChatCompletionEventSink,
) -> LlmTransportAttemptResult<bool> {
    let had_output = !output.is_empty();
    handle_response_event(data, output, sink).map_err(|error| {
        if had_output {
            LlmTransportAttemptError::committed(error)
        } else {
            LlmTransportAttemptError::retryable(error)
        }
    })
}

fn handle_response_event(
    data: &str,
    output: &mut String,
    sink: &mut dyn LlmChatCompletionEventSink,
) -> Result<bool> {
    let value: serde_json::Value = serde_json::from_str(data).map_err(|error| {
        MothershipError::InvalidRequest(format!("invalid Responses event JSON: {error}"))
    })?;

    if let Some(error) = response_error_message(&value) {
        return Err(MothershipError::InvalidRequest(format!(
            "Responses event failed: {error}"
        )));
    }

    if let Some(delta) = value.get("delta").and_then(|value| value.as_str()) {
        if !delta.is_empty() {
            output.push_str(delta);
            sink.delta(delta);
        }
    }

    if value.get("type").and_then(|value| value.as_str()) == Some("response.completed") {
        return Ok(true);
    }

    Ok(false)
}

fn response_error_message(value: &serde_json::Value) -> Option<String> {
    let event_type = value.get("type").and_then(|value| value.as_str());
    if matches!(event_type, Some("response.failed" | "error")) {
        return value
            .get("message")
            .and_then(|value| value.as_str())
            .or_else(|| {
                value.get("error").and_then(|error| match error {
                    serde_json::Value::String(error) => Some(error.as_str()),
                    serde_json::Value::Object(error) => error
                        .get("message")
                        .and_then(|value| value.as_str())
                        .or_else(|| error.get("code").and_then(|value| value.as_str())),
                    _ => None,
                })
            })
            .map(ToOwned::to_owned)
            .or_else(|| Some(event_type.unwrap_or("provider_error").to_string()));
    }

    None
}

fn extract_response_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.get("output_text").and_then(|value| value.as_str()) {
        return Some(text.to_string());
    }

    let mut text = String::new();
    for item in value.get("output")?.as_array()? {
        let Some(content) = item.get("content").and_then(|value| value.as_array()) else {
            continue;
        };
        for part in content {
            if part.get("type").and_then(|value| value.as_str()) == Some("output_text") {
                if let Some(part_text) = part.get("text").and_then(|value| value.as_str()) {
                    text.push_str(part_text);
                }
            }
        }
    }

    (!text.is_empty()).then_some(text)
}

fn websocket_url(url: &str) -> Result<String> {
    let mut url = url::Url::parse(url)
        .map_err(|error| MothershipError::InvalidRequest(format!("invalid URL: {error}")))?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => {
            return Err(MothershipError::InvalidRequest(format!(
                "unsupported WebSocket base scheme: {other}"
            )))
        }
    };
    url.set_scheme(scheme).map_err(|_| {
        MothershipError::InvalidRequest("failed to convert URL to WebSocket scheme".to_string())
    })?;
    Ok(url.to_string())
}

fn chat_streaming_http_client() -> Result<Client> {
    Client::builder()
        .connect_timeout(CHAT_HTTP_CONNECT_TIMEOUT)
        .timeout(CHAT_HTTP_STREAM_TIMEOUT)
        .build()
        .map_err(|error| {
            MothershipError::InvalidRequest(format!(
                "failed to build streaming HTTP client: {error}"
            ))
        })
}

fn chat_json_http_client() -> Result<Client> {
    Client::builder()
        .connect_timeout(CHAT_HTTP_CONNECT_TIMEOUT)
        .timeout(CHAT_HTTP_JSON_TIMEOUT)
        .build()
        .map_err(|error| {
            MothershipError::InvalidRequest(format!("failed to build JSON HTTP client: {error}"))
        })
}

fn configure_websocket_timeouts(
    stream: &mut MaybeTlsStream<TcpStream>,
) -> LlmTransportAttemptResult<()> {
    let read_timeout = Some(CHAT_WEBSOCKET_IDLE_TIMEOUT);
    let write_timeout = Some(CHAT_WEBSOCKET_IDLE_TIMEOUT);
    match stream {
        MaybeTlsStream::Plain(tcp) => {
            tcp.set_read_timeout(read_timeout).map_err(|error| {
                LlmTransportAttemptError::retryable_message(format!(
                    "failed to set WebSocket read timeout: {error}"
                ))
            })?;
            tcp.set_write_timeout(write_timeout).map_err(|error| {
                LlmTransportAttemptError::retryable_message(format!(
                    "failed to set WebSocket write timeout: {error}"
                ))
            })?;
        }
        MaybeTlsStream::NativeTls(tls) => {
            tls.get_ref()
                .set_read_timeout(read_timeout)
                .map_err(|error| {
                    LlmTransportAttemptError::retryable_message(format!(
                        "failed to set WebSocket TLS read timeout: {error}"
                    ))
                })?;
            tls.get_ref()
                .set_write_timeout(write_timeout)
                .map_err(|error| {
                    LlmTransportAttemptError::retryable_message(format!(
                        "failed to set WebSocket TLS write timeout: {error}"
                    ))
                })?;
        }
        _ => {}
    }
    Ok(())
}

fn reqwest_headers(headers: &BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut output = HeaderMap::new();
    for (name, value) in headers {
        output.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                MothershipError::InvalidRequest(format!("invalid HTTP header name: {error}"))
            })?,
            reqwest::header::HeaderValue::from_str(value).map_err(|error| {
                MothershipError::InvalidRequest(format!("invalid HTTP header value: {error}"))
            })?,
        );
    }
    Ok(output)
}

fn header_name(name: &str) -> Result<tungstenite::http::HeaderName> {
    tungstenite::http::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
        MothershipError::InvalidRequest(format!("invalid WebSocket header name: {error}"))
    })
}

impl LlmConnectorAdapter for OpenAiCodexLlmConnector {
    fn provider_id(&self) -> &'static str {
        "openai"
    }

    fn provider_label(&self) -> &'static str {
        "OpenAI"
    }

    fn bundled_models(&self) -> Vec<LlmModel> {
        openai_codex_model_catalog()
    }

    fn chat_system_prompt(&self, model_id: &str) -> Option<String> {
        (!model_id.trim().is_empty()).then(|| DEFAULT_CODEX_CHAT_SYSTEM_PROMPT.to_string())
    }

    fn remote_model_catalog(
        &self,
        input: RemoteModelCatalogInput<'_>,
    ) -> Result<Option<RemoteModelCatalog>> {
        if input.connection.provider_id.as_str() != OpenAiCodexOAuthAdapter::PROVIDER_ID {
            return Ok(None);
        }

        let credential: CodexCredentialPayload =
            serde_json::from_str(input.secret.payload.expose_for_vault())?;
        fetch_codex_remote_model_catalog(&Client::new(), &credential, input.secret).map(Some)
    }

    fn settings_schema(&self) -> ConnectorSettingsSchema {
        ConnectorSettingsSchema {
            model_management: ConnectorModelManagementSchema {
                kind: ConnectorModelManagementKind::RemoteCatalog,
                title: "Codex models".to_string(),
                description:
                    "Models are fetched from the Codex backend and cached locally for a short time."
                        .to_string(),
                add_model_label: None,
            },
        }
    }
}

pub fn openai_codex_model_catalog() -> Vec<LlmModel> {
    vec![
        codex_model(
            "gpt-5.5",
            "GPT-5.5",
            "GPT-5",
            "Current high-capability Codex model for complex agent work.",
            true,
        ),
        codex_model(
            "gpt-5.4",
            "GPT-5.4",
            "GPT-5",
            "Balanced Codex model for everyday coding sessions.",
            false,
        ),
        codex_model(
            "gpt-5.4-mini",
            "GPT-5.4 Mini",
            "GPT-5",
            "Lower-latency Codex model for smaller edits and quick checks.",
            false,
        ),
        codex_model(
            "gpt-5.3-codex",
            "GPT-5.3 Codex",
            "GPT-5 Codex",
            "Codex-specialized model kept for compatibility with existing workflows.",
            false,
        ),
        codex_model(
            "gpt-5.3-codex-spark",
            "GPT-5.3 Codex Spark",
            "GPT-5 Codex",
            "Fast Codex-specialized model for lightweight agent tasks.",
            false,
        ),
        codex_model(
            "gpt-5.2",
            "GPT-5.2",
            "GPT-5",
            "Older Codex-compatible model kept for account catalogs that still expose it.",
            false,
        ),
    ]
}

pub fn default_llm_model() -> LlmModel {
    StaticLlmConnectorRegistry::with_openai_codex()
        .list_models()
        .into_iter()
        .find(|model| model.recommended)
        .expect("OpenAI Codex model catalog must contain a recommended model")
}

pub fn find_llm_model(provider_id: &str, model_id: &str) -> Option<LlmModel> {
    StaticLlmConnectorRegistry::with_openai_codex().find_model(provider_id, model_id)
}

pub fn connector_settings_schema(provider_id: &str) -> Result<ConnectorSettingsSchema> {
    StaticLlmConnectorRegistry::with_openai_codex()
        .settings_schema(provider_id)
        .ok_or_else(|| MothershipError::InvalidRequest(format!("unknown provider: {provider_id}")))
}

pub fn chat_system_prompt(provider_id: &str, model_id: &str) -> Result<String> {
    StaticLlmConnectorRegistry::with_openai_codex()
        .chat_system_prompt(provider_id, model_id)
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or_else(|| {
            MothershipError::InvalidRequest(format!(
                "no chat system prompt registered for {provider_id}/{model_id}"
            ))
        })
}

fn codex_model(
    id: &str,
    label: &str,
    family: &str,
    description: &str,
    recommended: bool,
) -> LlmModel {
    LlmModel {
        provider_id: "openai".to_string(),
        provider_label: "OpenAI".to_string(),
        id: id.to_string(),
        label: label.to_string(),
        family: family.to_string(),
        description: description.to_string(),
        capabilities: vec![
            "text".to_string(),
            "reasoning".to_string(),
            "tools".to_string(),
            "code".to_string(),
        ],
        recommended,
    }
}

fn active_connection<'a>(
    connections: &'a [ProviderConnection],
    provider_id: &str,
) -> Option<&'a ProviderConnection> {
    connections.iter().find(|connection| {
        connection.provider_id.as_str() == provider_id
            && connection.status == ConnectionStatus::Active
    })
}

fn fetch_codex_remote_model_catalog(
    client: &Client,
    credential: &CodexCredentialPayload,
    current_secret: &SecretMaterial,
) -> Result<RemoteModelCatalog> {
    match request_codex_models(client, OPENAI_CODEX_MODELS_ENDPOINT, credential)? {
        CodexModelsHttpResult::Success(catalog) => Ok(RemoteModelCatalog {
            models: catalog.models,
            etag: catalog.etag,
            refreshed_secret: None,
        }),
        CodexModelsHttpResult::Unauthorized if !credential.refresh_token.trim().is_empty() => {
            let refreshed = refresh_codex_credential(client, credential)?;
            let catalog =
                match request_codex_models(client, OPENAI_CODEX_MODELS_ENDPOINT, &refreshed)? {
                    CodexModelsHttpResult::Success(catalog) => catalog,
                    CodexModelsHttpResult::Unauthorized => {
                        return Err(MothershipError::InvalidRequest(
                            "Codex models endpoint rejected refreshed token".to_string(),
                        ))
                    }
                };

            let secret = SecretMaterial {
                credential_kind: current_secret.credential_kind.clone(),
                payload: SecretPayload::new(serde_json::to_string(&refreshed)?),
                expires_at: refreshed.expires_at.clone(),
                fingerprint_hash: current_secret
                    .fingerprint_hash
                    .clone()
                    .or_else(|| refreshed.account_id.clone()),
            };

            Ok(RemoteModelCatalog {
                models: catalog.models,
                etag: catalog.etag,
                refreshed_secret: Some(secret),
            })
        }
        CodexModelsHttpResult::Unauthorized => Err(MothershipError::InvalidRequest(
            "Codex models endpoint rejected token".to_string(),
        )),
    }
}

enum CodexModelsHttpResult {
    Success(CodexRemoteCatalog),
    Unauthorized,
}

struct CodexRemoteCatalog {
    models: Vec<LlmModel>,
    etag: Option<String>,
}

const MODEL_CATALOG_FETCH_MAX_ATTEMPTS: u32 = 3;

/// Exponential backoff (with jitter) for retrying transient model-catalog fetch
/// failures. `attempt` is the just-failed attempt number (1-based). Capped at ~2s.
fn model_fetch_retry_delay(attempt: u32) -> std::time::Duration {
    let base_ms = 250u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(4));
    let capped = base_ms.min(2000);
    std::time::Duration::from_millis(capped + pseudo_jitter_ms(capped / 3))
}

/// Cheap, non-cryptographic jitter in `0..=max` ms (derived from the clock) so
/// retries don't fire in lockstep. No extra dependency needed.
fn pseudo_jitter_ms(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % (max + 1)
}

fn request_codex_models(
    client: &Client,
    endpoint: &str,
    credential: &CodexCredentialPayload,
) -> Result<CodexModelsHttpResult> {
    let url = codex_models_url(endpoint)?;
    let account_id = credential
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        let mut request = client
            .get(&url)
            .timeout(MODEL_CATALOG_FETCH_TIMEOUT)
            .bearer_auth(credential.access_token.trim());
        if let Some(account_id) = account_id {
            request = request.header("ChatGPT-Account-Id", account_id);
        }

        let response = match request.send() {
            Ok(response) => response,
            // Transport-level failure (DNS / connect / TLS / timeout): retriable.
            Err(error) => {
                if attempt < MODEL_CATALOG_FETCH_MAX_ATTEMPTS {
                    std::thread::sleep(model_fetch_retry_delay(attempt));
                    continue;
                }
                return Err(MothershipError::InvalidRequest(format!(
                    "Codex models fetch failed after {attempt} attempts: {error}"
                )));
            }
        };

        let status = response.status();

        if status == StatusCode::UNAUTHORIZED {
            return Ok(CodexModelsHttpResult::Unauthorized);
        }

        // Retry transient server-side failures (429 / 5xx). Other 4xx are terminal
        // (auth/bad request) — retrying just delays the error.
        if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            && attempt < MODEL_CATALOG_FETCH_MAX_ATTEMPTS
        {
            std::thread::sleep(model_fetch_retry_delay(attempt));
            continue;
        }

        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(MothershipError::InvalidRequest(format!(
                "Codex models fetch rejected: {status}: {}",
                sanitize_provider_error(&body)
            )));
        }

        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let body = response.bytes().map_err(|error| {
            MothershipError::InvalidRequest(format!("Codex models body read failed: {error}"))
        })?;
        let payload: CodexModelsResponse = serde_json::from_slice(&body).map_err(|error| {
            MothershipError::InvalidRequest(format!("invalid Codex models response: {error}"))
        })?;

        return Ok(CodexModelsHttpResult::Success(CodexRemoteCatalog {
            models: codex_remote_models_to_llm(payload.models),
            etag,
        }));
    }
}

fn codex_models_url(endpoint: &str) -> Result<String> {
    let mut url = url::Url::parse(endpoint).map_err(|error| {
        MothershipError::InvalidRequest(format!("invalid Codex models endpoint: {error}"))
    })?;
    url.query_pairs_mut()
        .append_pair("client_version", codex_client_version());
    Ok(url.to_string())
}

fn codex_client_version() -> &'static str {
    option_env!("MOTHERSHIP_CODEX_CLIENT_VERSION").unwrap_or(DEFAULT_CODEX_CLIENT_VERSION)
}

fn refresh_codex_credential(
    client: &Client,
    credential: &CodexCredentialPayload,
) -> Result<CodexCredentialPayload> {
    let refreshed = refresh_codex_token_with_client(client, &credential.refresh_token)?;
    let expires_at = refreshed.expires_in.map(|expires_in| {
        unix_timestamp_millis()
            .saturating_add(expires_in.saturating_mul(1000))
            .to_string()
    });

    Ok(CodexCredentialPayload {
        credential_type: credential.credential_type.clone(),
        access_token: refreshed.access_token,
        refresh_token: refreshed.refresh_token,
        id_token: refreshed.id_token,
        expires_at,
        account_id: credential.account_id.clone(),
    })
}

#[derive(Debug, Deserialize)]
struct CodexModelsResponse {
    models: Vec<CodexRemoteModel>,
}

#[derive(Debug, Deserialize)]
struct CodexRemoteModel {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    supported_in_api: Option<bool>,
    #[serde(default)]
    priority: Option<i64>,
}

fn codex_remote_models_to_llm(mut models: Vec<CodexRemoteModel>) -> Vec<LlmModel> {
    models.sort_by_key(|model| model.priority.unwrap_or(i64::MAX));

    models
        .into_iter()
        .filter(|model| model.supported_in_api.unwrap_or(true))
        .filter(|model| model.visibility.as_deref().unwrap_or("list") == "list")
        .enumerate()
        .map(|(index, model)| {
            let label = model
                .display_name
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| model.slug.clone());
            LlmModel {
                provider_id: OpenAiCodexOAuthAdapter::PROVIDER_ID.to_string(),
                provider_label: "OpenAI".to_string(),
                id: model.slug,
                label,
                family: "Codex".to_string(),
                description: model
                    .description
                    .unwrap_or_else(|| "Model provided by the Codex remote catalog.".to_string()),
                capabilities: vec![
                    "text".to_string(),
                    "reasoning".to_string(),
                    "tools".to_string(),
                    "code".to_string(),
                ],
                recommended: index == 0,
            }
        })
        .collect()
}

fn sanitize_provider_error(body: &str) -> String {
    if body.trim().is_empty() {
        return "empty response body".to_string();
    }

    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| match error {
                    serde_json::Value::Object(error) => error
                        .get("message")
                        .or_else(|| error.get("code"))
                        .and_then(|value| value.as_str())
                        .map(ToOwned::to_owned),
                    serde_json::Value::String(error) => Some(error.to_string()),
                    _ => None,
                })
                .or_else(|| {
                    value
                        .get("message")
                        .and_then(|value| value.as_str())
                        .map(ToOwned::to_owned)
                })
        })
        .unwrap_or_else(|| body.chars().take(300).collect())
}

fn parse_timestamp(value: &str) -> Option<u64> {
    value.parse::<u64>().ok()
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn unix_timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::json;

    use super::*;
    use crate::{
        auth::{
            AuthMethodId, CredentialKind, CredentialRecordId, CredentialRef,
            InMemoryCredentialVault, ProviderConnectionId, ProviderId, VaultHandle,
        },
        Database,
    };

    #[test]
    fn codex_remote_catalog_filters_and_orders_models() {
        let payload: CodexModelsResponse = serde_json::from_value(json!({
            "models": [
                {
                    "slug": "hidden",
                    "display_name": "Hidden",
                    "visibility": "hidden",
                    "supported_in_api": true,
                    "priority": 1
                },
                {
                    "slug": "remote-b",
                    "display_name": "Remote B",
                    "description": "B",
                    "visibility": "list",
                    "supported_in_api": true,
                    "priority": 20
                },
                {
                    "slug": "remote-a",
                    "display_name": "Remote A",
                    "description": "A",
                    "visibility": "list",
                    "supported_in_api": true,
                    "priority": 10
                },
                {
                    "slug": "unsupported",
                    "display_name": "Unsupported",
                    "visibility": "list",
                    "supported_in_api": false,
                    "priority": 0
                }
            ]
        }))
        .expect("payload");

        let models = codex_remote_models_to_llm(payload.models);

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "remote-a");
        assert!(models[0].recommended);
        assert_eq!(models[1].id, "remote-b");
        assert!(!models[1].recommended);
    }

    #[test]
    fn codex_connector_reports_remote_catalog_management() {
        let schema = OpenAiCodexLlmConnector.settings_schema();

        assert_eq!(
            schema.model_management.kind,
            ConnectorModelManagementKind::RemoteCatalog
        );
    }

    #[test]
    fn codex_models_url_includes_client_version() {
        let url = codex_models_url("https://chatgpt.com/backend-api/codex/models").expect("url");

        assert!(url.contains("client_version="));
    }

    #[test]
    fn chat_response_input_uses_responses_message_shape() {
        let input = response_input(&[
            LlmChatMessage {
                role: LlmChatRole::User,
                content: "Build it".to_string(),
            },
            LlmChatMessage {
                role: LlmChatRole::Assistant,
                content: "Done".to_string(),
            },
            LlmChatMessage {
                role: LlmChatRole::User,
                content: "   ".to_string(),
            },
        ]);

        assert_eq!(
            input,
            vec![
                json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Build it"}]
                }),
                json!({
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Done"}]
                })
            ]
        );
    }

    #[test]
    fn codex_payloads_map_system_prompt_to_required_instructions() {
        let request = LlmChatCompletionRequest {
            provider_id: "openai".to_string(),
            model_id: "gpt-5.5".to_string(),
            system_prompt: "Follow the user request.".to_string(),
            messages: vec![LlmChatMessage {
                role: LlmChatRole::User,
                content: "Hello".to_string(),
            }],
        };

        assert_eq!(
            http_body(&request, true).get("instructions"),
            Some(&json!("Follow the user request."))
        );
        assert_eq!(
            websocket_body(&request).get("instructions"),
            Some(&json!("Follow the user request."))
        );
    }

    #[test]
    fn codex_connector_provides_chat_system_prompt() {
        let prompt = StaticLlmConnectorRegistry::with_openai_codex()
            .chat_system_prompt("openai", "gpt-5.5")
            .expect("system prompt");

        assert!(prompt.contains("Mothership"));
    }

    #[test]
    fn response_event_streams_delta_and_completion() {
        let mut sink = RecordingChatSink::default();
        let mut output = String::new();

        let completed = handle_response_event(
            r#"{"type":"response.output_text.delta","delta":"Hello"}"#,
            &mut output,
            &mut sink,
        )
        .expect("delta event");

        assert!(!completed);
        assert_eq!(output, "Hello");
        assert_eq!(sink.deltas, vec!["Hello".to_string()]);

        let completed =
            handle_response_event(r#"{"type":"response.completed"}"#, &mut output, &mut sink)
                .expect("completed event");

        assert!(completed);
    }

    #[test]
    fn response_event_returns_provider_error() {
        let mut sink = RecordingChatSink::default();
        let mut output = String::new();

        let error = handle_response_event(
            r#"{"type":"response.failed","error":{"message":"model rejected"}}"#,
            &mut output,
            &mut sink,
        )
        .expect_err("provider failure");

        assert!(error.to_string().contains("model rejected"));
        assert!(output.is_empty());
        assert!(sink.deltas.is_empty());
    }

    #[test]
    fn remote_catalog_cache_requires_active_connection() {
        let database_path = temp_database_path("remote_catalog_cache_requires_active_connection");
        let database = Database::open(database_path.clone()).expect("open database");
        database
            .save_llm_model_catalog_cache(&LlmModelCatalogCache {
                provider_id: "openai".to_string(),
                models: vec![test_model("remote-only")],
                etag: None,
                fetched_at: unix_timestamp_secs().to_string(),
                expires_at: unix_timestamp_secs().saturating_add(300).to_string(),
            })
            .expect("save cache");

        let vault = InMemoryCredentialVault::default();
        let registry = StaticLlmConnectorRegistry::with_openai_codex();
        let service = LlmModelCatalogService::new(&database, &vault, &registry);

        assert!(service.list_models(&[]).expect("list models").is_empty());

        let _ = fs::remove_file(database_path);
    }

    #[test]
    fn remote_catalog_error_is_returned_not_hidden_by_bundled_models() {
        let database_path = temp_database_path("remote_catalog_error_is_returned");
        let database = Database::open(database_path.clone()).expect("open database");
        let vault = InMemoryCredentialVault::default();
        let vault_handle = vault
            .store(StoreCredentialRequest {
                provider_id: ProviderId::from("mock"),
                credential: SecretMaterial {
                    credential_kind: CredentialKind::OAuthTokenSet,
                    payload: SecretPayload::new("{}"),
                    expires_at: None,
                    fingerprint_hash: None,
                },
            })
            .expect("store secret");
        let registry = StaticLlmConnectorRegistry::new(vec![Box::new(FailingRemoteConnector)]);
        let service = LlmModelCatalogService::new(&database, &vault, &registry);
        let connection = ProviderConnection {
            id: ProviderConnectionId::from("provider_connection_test"),
            provider_id: ProviderId::from("mock"),
            auth_method_id: AuthMethodId::from("mock_oauth"),
            status: ConnectionStatus::Active,
            account_label: None,
            account_email: None,
            scopes: Vec::new(),
            capabilities: Vec::new(),
            credential_ref: CredentialRef {
                record_id: CredentialRecordId::from("credential_record_test"),
                vault_handle: VaultHandle::new(vault_handle.as_str().to_string()),
            },
            expires_at: None,
            created_at: "0".to_string(),
            updated_at: "0".to_string(),
        };

        let error = service
            .list_models(&[connection])
            .expect_err("remote error should be returned");

        assert!(error.to_string().contains("remote failed"));

        let _ = fs::remove_file(database_path);
    }

    struct FailingRemoteConnector;

    impl LlmConnectorAdapter for FailingRemoteConnector {
        fn provider_id(&self) -> &'static str {
            "mock"
        }

        fn provider_label(&self) -> &'static str {
            "Mock"
        }

        fn bundled_models(&self) -> Vec<LlmModel> {
            vec![test_model("bundled-model")]
        }

        fn remote_model_catalog(
            &self,
            _input: RemoteModelCatalogInput<'_>,
        ) -> Result<Option<RemoteModelCatalog>> {
            Err(MothershipError::InvalidRequest("remote failed".to_string()))
        }

        fn settings_schema(&self) -> ConnectorSettingsSchema {
            ConnectorSettingsSchema {
                model_management: ConnectorModelManagementSchema {
                    kind: ConnectorModelManagementKind::RemoteCatalog,
                    title: "Mock models".to_string(),
                    description: "Mock remote catalog".to_string(),
                    add_model_label: None,
                },
            }
        }
    }

    #[derive(Default)]
    struct RecordingChatSink {
        deltas: Vec<String>,
    }

    impl LlmChatCompletionEventSink for RecordingChatSink {
        fn transport_selected(&mut self, _transport: LlmTransportKind) {}

        fn delta(&mut self, delta: &str) {
            self.deltas.push(delta.to_string());
        }
    }

    fn test_model(id: &str) -> LlmModel {
        LlmModel {
            provider_id: "openai".to_string(),
            provider_label: "OpenAI".to_string(),
            id: id.to_string(),
            label: id.to_string(),
            family: "Codex".to_string(),
            description: "Test model".to_string(),
            capabilities: vec!["text".to_string()],
            recommended: true,
        }
    }

    fn temp_database_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();

        std::env::temp_dir().join(format!("mothership_llm_{name}_{unique}.sqlite3"))
    }
}
