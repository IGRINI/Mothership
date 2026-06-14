//! Provider-agnostic LLM types.
//!
//! The core knows nothing about any specific provider. Providers are subprocess
//! adapters (see `mothership-adapter-host` + the `adapters/` crates) that own
//! their own transport, auth, and model catalog. This module only defines the
//! generic value types that cross the core↔adapter boundary plus the two traits
//! the chat path is built on. Model listing and chat both flow through adapters
//! (see [`crate::SubprocessChatGateway`]); the core holds no provider code.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::{ChatCancellationToken, Result};
use mothership_adapter_host::protocol::{
    FastModeCapabilities, PromptBundle, ReasoningCapabilities, ReasoningConfig, RuntimeContext,
    ToolDescriptor,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ProviderRuntimeKind {
    /// Core owns the agent loop and tool execution. The adapter translates one
    /// provider model round at a time.
    CoreManaged,
    /// The adapter owns its upstream agent runtime, including its own loop,
    /// tools, compaction, subagents, and provider-specific session state.
    SelfManaged,
}

impl ProviderRuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CoreManaged => "core_managed",
            Self::SelfManaged => "self_managed",
        }
    }
}

// `optional_fields`: `reasoning` / `fast_mode` are `#[serde(default)] Option<_>`
// (absent for models lacking those capabilities), matching the TS `?:` shape.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, optional_fields = nullable)]
pub struct LlmModel {
    pub provider_id: String,
    pub provider_label: String,
    pub id: String,
    pub label: String,
    pub family: String,
    pub description: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub reasoning: Option<ReasoningCapabilities>,
    #[serde(default)]
    pub fast_mode: Option<FastModeCapabilities>,
    pub recommended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SelectedLlmModel {
    pub provider_id: String,
    pub model_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FeatureRoute {
    pub feature: String,
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub options: Value,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ConnectorSettingsSchema {
    pub model_management: ConnectorModelManagementSchema,
}

// `optional_fields` makes the `Option` `add_model_label` optional in TS,
// matching the original `addModelLabel?`. `accepts_custom_model_ids` stays a
// required `boolean` (the snapshot always includes it).
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, optional_fields = nullable)]
pub struct ConnectorModelManagementSchema {
    pub kind: ConnectorModelManagementKind,
    pub title: String,
    pub description: String,
    pub add_model_label: Option<String>,
    #[serde(default)]
    pub accepts_custom_model_ids: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ConnectorModelManagementKind {
    FixedCatalog,
    RemoteCatalog,
    EditableList,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmChatCompletionRequest {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub fast_mode: bool,
    pub prompt: PromptBundle,
    #[serde(default)]
    pub runtime_context: RuntimeContext,
    #[serde(default)]
    pub tools: Vec<ToolDescriptor>,
    pub messages: Vec<LlmChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmChatRoundRequest {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub fast_mode: bool,
    pub prompt: PromptBundle,
    #[serde(default)]
    pub runtime_context: RuntimeContext,
    #[serde(default)]
    pub tools: Vec<ToolDescriptor>,
    pub messages: Vec<LlmChatMessage>,
    #[serde(default)]
    pub state: Option<Value>,
    #[serde(default)]
    pub tool_results: Vec<LlmToolCallResponse>,
    #[serde(default)]
    pub extra_messages: Vec<LlmChatMessage>,
}

impl LlmChatRoundRequest {
    pub fn from_completion(request: LlmChatCompletionRequest) -> Self {
        Self {
            provider_id: request.provider_id,
            model_id: request.model_id,
            reasoning: request.reasoning,
            fast_mode: request.fast_mode,
            prompt: request.prompt,
            runtime_context: request.runtime_context,
            tools: request.tools,
            messages: request.messages,
            state: None,
            tool_results: Vec::new(),
            extra_messages: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRequestDraft {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub fast_mode: bool,
    pub prompt: PromptBundle,
    #[serde(default)]
    pub runtime_context: RuntimeContext,
    #[serde(default)]
    pub tools: Vec<ToolDescriptor>,
    pub messages: Vec<LlmChatMessage>,
}

impl From<LlmChatCompletionRequest> for ProviderRequestDraft {
    fn from(request: LlmChatCompletionRequest) -> Self {
        Self {
            provider_id: request.provider_id,
            model_id: request.model_id,
            reasoning: request.reasoning,
            fast_mode: request.fast_mode,
            prompt: request.prompt,
            runtime_context: request.runtime_context,
            tools: request.tools,
            messages: request.messages,
        }
    }
}

impl From<ProviderRequestDraft> for LlmChatCompletionRequest {
    fn from(draft: ProviderRequestDraft) -> Self {
        Self {
            provider_id: draft.provider_id,
            model_id: draft.model_id,
            reasoning: draft.reasoning,
            fast_mode: draft.fast_mode,
            prompt: draft.prompt,
            runtime_context: draft.runtime_context,
            tools: draft.tools,
            messages: draft.messages,
        }
    }
}

/// Core-side hook for provider request modification.
///
/// A modifier sees the provider/model, reasoning config, runtime context,
/// rendered prompt bundle, tool catalog, and conversation messages before the
/// request crosses the adapter boundary. This is the native extension point for
/// future user/plugin "provider mods"; adapters still only map the final
/// structured request into provider-specific wire format.
pub trait ProviderRequestModifier: Send + Sync {
    fn name(&self) -> &'static str;

    fn modify(&self, draft: &mut ProviderRequestDraft) -> Result<()>;
}

#[derive(Clone, Default)]
pub struct ProviderRequestPipeline {
    modifiers: Vec<Arc<dyn ProviderRequestModifier>>,
}

impl ProviderRequestPipeline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_modifier(mut self, modifier: Arc<dyn ProviderRequestModifier>) -> Self {
        self.modifiers.push(modifier);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.modifiers.is_empty()
    }

    pub fn apply(&self, request: LlmChatCompletionRequest) -> Result<LlmChatCompletionRequest> {
        let mut draft = ProviderRequestDraft::from(request);
        for modifier in &self.modifiers {
            modifier.modify(&mut draft).map_err(|error| {
                crate::MothershipError::InvalidRequest(format!(
                    "provider request modifier `{}` failed: {error}",
                    modifier.name()
                ))
            })?;
        }
        Ok(draft.into())
    }
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
    Subprocess,
}

impl LlmTransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::HttpSse => "http_sse",
            Self::HttpJson => "http_json",
            Self::Subprocess => "subprocess",
        }
    }
}

pub trait LlmChatCompletionEventSink {
    fn transport_selected(&mut self, transport: LlmTransportKind);

    fn delta(&mut self, delta: &str);

    fn before_tool_call(&mut self, _tool_call_id: &str) {}
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmToolCallRequest {
    pub run_id: Option<String>,
    pub tool_call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmToolCallResult {
    pub ok: bool,
    pub content: String,
    pub backgrounded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmToolCallResponse {
    pub tool_call_id: String,
    pub result: LlmToolCallResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmChatRound {
    pub text: String,
    #[serde(default)]
    pub state: Option<Value>,
    #[serde(default)]
    pub tool_calls: Vec<LlmToolCallRequest>,
}

pub trait LlmToolCallHandler: Send + Sync {
    fn handle_tool_call(
        &self,
        request: LlmToolCallRequest,
        cancellation: &ChatCancellationToken,
    ) -> LlmToolCallResult;

    fn handle_tool_calls(
        &self,
        requests: Vec<LlmToolCallRequest>,
        cancellation: &ChatCancellationToken,
    ) -> Vec<LlmToolCallResult> {
        requests
            .into_iter()
            .map(|request| self.handle_tool_call(request, cancellation))
            .collect()
    }
}

pub trait LlmChatRoundGateway: Send + Sync {
    fn complete_round(
        &self,
        request: LlmChatRoundRequest,
        cancellation: &ChatCancellationToken,
        sink: &mut dyn LlmChatCompletionEventSink,
    ) -> Result<LlmChatRound>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mothership_adapter_host::protocol::{PromptSection, ToolDescriptor};
    use serde_json::json;

    struct TestModifier;

    impl ProviderRequestModifier for TestModifier {
        fn name(&self) -> &'static str {
            "test"
        }

        fn modify(&self, draft: &mut ProviderRequestDraft) -> Result<()> {
            draft.prompt.sections.push(PromptSection {
                id: "test.extra".to_string(),
                source: "test".to_string(),
                priority: 100,
                locked: false,
                content: "extra instruction".to_string(),
            });
            draft.tools.push(ToolDescriptor {
                id: "test_tool".to_string(),
                name: "test_tool".to_string(),
                description: "Test tool".to_string(),
                parameters: json!({ "type": "object" }),
                strict: false,
                annotations: Default::default(),
            });
            Ok(())
        }
    }

    #[test]
    fn provider_request_pipeline_applies_modifiers() {
        let request = LlmChatCompletionRequest {
            provider_id: "provider".to_string(),
            model_id: "model".to_string(),
            reasoning: None,
            fast_mode: false,
            prompt: PromptBundle::default(),
            runtime_context: RuntimeContext::default(),
            tools: Vec::new(),
            messages: Vec::new(),
        };
        let pipeline = ProviderRequestPipeline::new().with_modifier(Arc::new(TestModifier));

        let request = pipeline.apply(request).expect("pipeline");

        assert!(request.prompt.rendered_text().contains("extra instruction"));
        assert_eq!(request.tools[0].name, "test_tool");
    }
}
