//! Core-owned tool catalog.
//!
//! Provider adapters receive these neutral descriptors and only translate them
//! to provider-specific JSON shapes.

use std::collections::BTreeMap;

use mothership_adapter_host::protocol::ToolDescriptor;
use serde_json::json;

pub const RUN_COMMAND_TOOL_ID: &str = "core.run_command";
pub const RUN_COMMAND_TOOL_NAME: &str = "run_command";

pub const READ_FILE_TOOL_ID: &str = "core.read_file";
pub const READ_FILE_TOOL_NAME: &str = "read_file";
pub const WRITE_FILE_TOOL_ID: &str = "core.write_file";
pub const WRITE_FILE_TOOL_NAME: &str = "write_file";
pub const EDIT_FILE_TOOL_ID: &str = "core.edit_file";
pub const EDIT_FILE_TOOL_NAME: &str = "edit_file";
pub const APPLY_PATCH_TOOL_ID: &str = "core.apply_patch";
pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";

pub fn default_tool_catalog() -> Vec<ToolDescriptor> {
    vec![
        run_command_tool_descriptor(),
        read_file_tool_descriptor(),
        write_file_tool_descriptor(),
        edit_file_tool_descriptor(),
        apply_patch_tool_descriptor(),
    ]
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

fn read_file_tool_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        id: READ_FILE_TOOL_ID.to_string(),
        name: READ_FILE_TOOL_NAME.to_string(),
        description: "Read a UTF-8 text file from the active project and return its line-numbered contents. Binary files are refused. Use startLine/limit to page through large files.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Project-relative (or absolute, inside the project) path to read."
                },
                "startLine": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional 1-based line to start from."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional maximum number of lines to return."
                },
                "maxBytes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional cap on inlined bytes before the content spills to a reference."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        strict: false,
        annotations: BTreeMap::new(),
    }
}

fn write_file_tool_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        id: WRITE_FILE_TOOL_ID.to_string(),
        name: WRITE_FILE_TOOL_NAME.to_string(),
        description: "Create a new file or fully overwrite an existing one in the active project. To replace an existing file, pass overwrite or the file's current expectedSha256. Prefer edit_file for pointwise changes and apply_patch for multi-file changes.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Project-relative (or absolute, inside the project) path to write."
                },
                "content": {
                    "type": "string",
                    "description": "Full file contents to write."
                },
                "create": {
                    "type": "boolean",
                    "description": "Allow creating the file when it does not exist (default true)."
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Allow replacing an existing file (default false)."
                },
                "expectedSha256": {
                    "type": "string",
                    "description": "Optional precondition: the current file's sha256. A mismatch aborts the write."
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
        strict: false,
        annotations: BTreeMap::new(),
    }
}

fn edit_file_tool_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        id: EDIT_FILE_TOOL_ID.to_string(),
        name: EDIT_FILE_TOOL_NAME.to_string(),
        description: "Replace an exact span of text in a project file. oldText must match the file (copy it verbatim) and be unique unless replaceAll is set. Returns a clear error if the text is not found or is ambiguous.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Project-relative (or absolute, inside the project) path to edit."
                },
                "oldText": {
                    "type": "string",
                    "description": "Exact text to replace, copied verbatim from the file."
                },
                "newText": {
                    "type": "string",
                    "description": "Replacement text."
                },
                "replaceAll": {
                    "type": "boolean",
                    "description": "Replace every occurrence instead of requiring a unique match (default false)."
                },
                "expectedSha256": {
                    "type": "string",
                    "description": "Optional precondition: the current file's sha256. A mismatch aborts the edit."
                }
            },
            "required": ["path", "oldText", "newText"],
            "additionalProperties": false
        }),
        strict: false,
        annotations: BTreeMap::new(),
    }
}

fn apply_patch_tool_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        id: APPLY_PATCH_TOOL_ID.to_string(),
        name: APPLY_PATCH_TOOL_NAME.to_string(),
        description: "Apply a multi-file V4A patch (add/update/delete/move) to the active project. The patch is applied all-or-none: if any hunk does not match current content, nothing is written. Use this for complex or multi-file edits.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "A V4A patch envelope beginning with `*** Begin Patch` and ending with `*** End Patch`."
                }
            },
            "required": ["patch"],
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

        assert_eq!(tools[0].id, RUN_COMMAND_TOOL_ID);
        assert_eq!(tools[0].name, RUN_COMMAND_TOOL_NAME);
        assert_eq!(tools[0].parameters["required"][0], "program");
    }

    #[test]
    fn default_catalog_exposes_five_tools() {
        let tools = default_tool_catalog();
        assert_eq!(tools.len(), 5);

        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                RUN_COMMAND_TOOL_NAME,
                READ_FILE_TOOL_NAME,
                WRITE_FILE_TOOL_NAME,
                EDIT_FILE_TOOL_NAME,
                APPLY_PATCH_TOOL_NAME,
            ]
        );

        // Every file tool declares its required fields and forbids extras.
        for tool in tools.iter().skip(1) {
            assert_eq!(tool.parameters["additionalProperties"], false);
            assert!(tool.parameters["required"].is_array());
        }
    }
}
