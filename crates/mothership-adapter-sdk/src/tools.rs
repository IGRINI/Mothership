//! Shared provider tool-call parsing helpers.
//!
//! Providers expose tool calls in different wire shapes, but adapters must only
//! normalize them into stable id/name/arguments. Core owns dispatch.

use serde_json::{json, Value};

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
        let arguments = parse_tool_arguments("{\"program\":\"git\"}");

        assert_eq!(arguments["program"], "git");
    }

    #[test]
    fn invalid_raw_arguments_are_preserved() {
        let arguments = parse_tool_arguments("{oops");

        assert_eq!(arguments["rawArguments"], "{oops");
    }
}
