//! Core-owned tool catalog.
//!
//! Provider adapters receive these neutral descriptors and only translate them
//! to provider-specific JSON shapes.

use std::collections::BTreeMap;

use mothership_adapter_host::protocol::ToolDescriptor;
use serde_json::json;

pub const RUN_COMMAND_TOOL_ID: &str = "core.run_command";
pub const RUN_COMMAND_TOOL_NAME: &str = "run_command";

pub fn default_tool_catalog() -> Vec<ToolDescriptor> {
    vec![run_command_tool_descriptor()]
}

fn run_command_tool_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        id: RUN_COMMAND_TOOL_ID.to_string(),
        name: RUN_COMMAND_TOOL_NAME.to_string(),
        description: "Run a local command through Mothership's supervised tool runtime. Use it for project inspection, tests, builds, git operations, and other development tasks. The app may ask the user for approval before execution.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "program": {
                    "type": "string",
                    "description": "Executable to run, for example git, npm, cargo, powershell, or python."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Command arguments without shell quoting."
                },
                "cwd": {
                    "type": "string",
                    "description": "Absolute working directory for the command. Omit only when the current project directory is not known."
                },
                "timeoutMs": {
                    "type": "integer",
                    "minimum": 1000,
                    "maximum": 1800000,
                    "description": "Optional timeout in milliseconds."
                }
            },
            "required": ["program"],
            "additionalProperties": false
        }),
        strict: false,
        annotations: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_catalog_exposes_run_command() {
        let tools = default_tool_catalog();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id, RUN_COMMAND_TOOL_ID);
        assert_eq!(tools[0].name, RUN_COMMAND_TOOL_NAME);
        assert_eq!(tools[0].parameters["required"][0], "program");
    }
}
