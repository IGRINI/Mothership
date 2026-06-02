//! Shared host-tool dispatch helpers for provider adapters.
//!
//! Providers expose tool calls in different wire shapes, but once an adapter has
//! parsed them into a stable id, name, and JSON arguments, dispatching to the
//! host is generic.

use anyhow::Result;
use serde_json::{json, Value};

use crate::protocol::{ToolCallInvocation, ToolCallResult};
use crate::ChatSink;

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ProviderToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    pub fn from_raw_json_arguments(
        id: impl Into<String>,
        name: impl Into<String>,
        raw_arguments: &str,
    ) -> Self {
        Self::new(id, name, parse_tool_arguments(raw_arguments))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderToolResult {
    pub call: ProviderToolCall,
    pub result: ToolCallResult,
}

pub async fn dispatch_tool_calls(
    calls: impl IntoIterator<Item = ProviderToolCall>,
    sink: &ChatSink,
) -> Result<Vec<ProviderToolResult>> {
    if sink.is_cancelled() {
        return Ok(Vec::new());
    }

    let calls = calls.into_iter().collect::<Vec<_>>();
    let results = match sink
        .request_tools(
            calls
                .iter()
                .map(|call| ToolCallInvocation {
                    tool_call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
        )
        .await
    {
        Ok(results) => results,
        Err(error) if sink.is_cancelled() => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    if results.len() != calls.len() {
        anyhow::bail!(
            "host returned {} tool result(s) for {} tool call(s)",
            results.len(),
            calls.len()
        );
    }

    Ok(calls
        .into_iter()
        .zip(results)
        .map(|(call, result)| ProviderToolResult { call, result })
        .collect())
}

pub fn parse_tool_arguments(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "rawArguments": arguments }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_json_arguments_parse_to_object() {
        let call = ProviderToolCall::from_raw_json_arguments(
            "call_1",
            "run_command",
            "{\"program\":\"git\"}",
        );

        assert_eq!(call.arguments["program"], "git");
    }

    #[test]
    fn invalid_raw_arguments_are_preserved() {
        let call = ProviderToolCall::from_raw_json_arguments("call_1", "run_command", "{oops");

        assert_eq!(call.arguments["rawArguments"], "{oops");
    }
}
