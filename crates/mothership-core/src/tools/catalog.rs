//! Core-owned tool catalog.
//!
//! Provider adapters receive these neutral descriptors and only translate them
//! to provider-specific JSON shapes.

use std::collections::BTreeMap;

use mothership_adapter_host::protocol::ToolDescriptor;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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

pub const LIST_FILES_TOOL_ID: &str = "core.list_files";
pub const LIST_FILES_TOOL_NAME: &str = "list_files";
pub const SEARCH_TEXT_TOOL_ID: &str = "core.search_text";
pub const SEARCH_TEXT_TOOL_NAME: &str = "search_text";

pub fn default_tool_catalog() -> Vec<ToolDescriptor> {
    vec![
        run_command_tool_descriptor(),
        read_file_tool_descriptor(),
        write_file_tool_descriptor(),
        edit_file_tool_descriptor(),
        apply_patch_tool_descriptor(),
        list_files_tool_descriptor(),
        search_text_tool_descriptor(),
    ]
}

// ---------------------------------------------------------------------------
// Catalog stability
// ---------------------------------------------------------------------------
//
// Providers prefix-cache the serialized tool catalog. If a tool definition
// silently changes between rebuilds/reconnects, the cache is poisoned and every
// request after the change pays a full re-encode (or, worse, the model sees a
// different tool than it was primed on). These primitives give the catalog a
// byte-stable canonical form, a fingerprint, and a session pin that detects
// drift — including drift introduced when MCP/dynamic tools are merged in.

/// Serialize a JSON value with object keys sorted recursively, producing a
/// byte-identical representation regardless of the map's backing store
/// (`BTreeMap` vs `preserve_order` `IndexMap`) or construction order.
pub fn canonical_json(value: &Value) -> String {
    serde_json::to_string(&sorted_value(value)).unwrap_or_default()
}

fn sorted_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let mut out = serde_json::Map::new();
            for key in keys {
                out.insert(key.clone(), sorted_value(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_value).collect()),
        other => other.clone(),
    }
}

/// The canonical, byte-stable serialization of a tool catalog. Repeated calls
/// with equal descriptors yield identical bytes — the property prefix caching on
/// the provider side depends on. Ordered hand-built schemas are preserved (the
/// shapes are unchanged; only the serialization is made deterministic).
pub fn canonical_catalog_bytes(tools: &[ToolDescriptor]) -> String {
    let value = serde_json::to_value(tools).unwrap_or(Value::Null);
    canonical_json(&value)
}

/// A short, stable fingerprint (sha256 hex) of a catalog's canonical bytes.
pub fn catalog_fingerprint(tools: &[ToolDescriptor]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_catalog_bytes(tools).as_bytes());
    format!("{:x}", hasher.finalize())
}

/// A session pin of a tool catalog: its canonical bytes + fingerprint, captured
/// once. The runtime pins the catalog when a session starts and compares on every
/// rebuild; a mismatch means a tool definition changed underneath the prefix
/// cache. This is the MCP-byte-pin: when a bridge reconnects and re-advertises
/// tools, [`CatalogPin::check`] catches a silent definition change.
#[derive(Debug, Clone)]
pub struct CatalogPin {
    fingerprint: String,
    bytes: String,
}

impl CatalogPin {
    /// Pin the catalog as it is now.
    pub fn pin(tools: &[ToolDescriptor]) -> Self {
        Self {
            fingerprint: catalog_fingerprint(tools),
            bytes: canonical_catalog_bytes(tools),
        }
    }

    /// The pinned fingerprint (sha256 hex).
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The pinned canonical bytes (what should be re-sent verbatim on reconnect).
    pub fn bytes(&self) -> &str {
        &self.bytes
    }

    /// Compare the current catalog against the pin: `Ok(())` if byte-identical,
    /// `Err(CatalogDrift)` describing the change otherwise.
    pub fn check(&self, tools: &[ToolDescriptor]) -> std::result::Result<(), CatalogDrift> {
        let current = catalog_fingerprint(tools);
        if current == self.fingerprint {
            Ok(())
        } else {
            Err(CatalogDrift {
                pinned_fingerprint: self.fingerprint.clone(),
                current_fingerprint: current,
            })
        }
    }
}

/// Reported when a catalog's canonical bytes change after it was pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogDrift {
    pub pinned_fingerprint: String,
    pub current_fingerprint: String,
}

impl std::fmt::Display for CatalogDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let short = |hash: &str| hash.chars().take(12).collect::<String>();
        write!(
            f,
            "tool catalog drifted after pinning (pinned {}, now {}) — provider prefix cache invalidated",
            short(&self.pinned_fingerprint),
            short(&self.current_fingerprint),
        )
    }
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
                    "maximum": 10485760,
                    "description": "Optional cap on how many INLINE bytes are returned (up to 10 MiB) before the content spills to a reference. Independently, the raw read off disk is hard-capped at 10 MiB: startLine/limit page only WITHIN that read window, not past the 10 MiB cap. For ranges in a file larger than 10 MiB, use run_command (e.g. sed/Get-Content)."
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

fn list_files_tool_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        id: LIST_FILES_TOOL_ID.to_string(),
        name: LIST_FILES_TOOL_NAME.to_string(),
        description: "List files in the active project. Honors .gitignore/.ignore and hidden-file rules by default; pass includeIgnored to surface ignored/hidden files. Filter with an optional glob matched against project-relative paths. Sensitive files (credentials, keys) are never listed.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "glob": {
                    "type": "string",
                    "description": "Optional glob matched against project-relative paths, e.g. `**/*.rs` or `src/**`. Must be a relative pattern inside the project."
                },
                "dir": {
                    "type": "string",
                    "description": "Optional project-relative subdirectory to start from. Defaults to the project root. Must stay inside the project."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional maximum number of files to return (default 1000)."
                },
                "includeIgnored": {
                    "type": "boolean",
                    "description": "Include files normally hidden by .gitignore/.ignore and dotfiles (default false). Sensitive files are still excluded."
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        strict: false,
        annotations: BTreeMap::new(),
    }
}

fn search_text_tool_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        id: SEARCH_TEXT_TOOL_ID.to_string(),
        name: SEARCH_TEXT_TOOL_NAME.to_string(),
        description: "Search file contents in the active project for a pattern and return matching lines with their paths and line numbers. Literal substring match by default; pass regex:true to treat the pattern as a regular expression. Honors .gitignore by default (includeIgnored to override); binary and sensitive files are skipped.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Text to search for. A literal substring by default; a regular expression when regex is true."
                },
                "dir": {
                    "type": "string",
                    "description": "Optional project-relative subdirectory to search. Defaults to the project root. Must stay inside the project."
                },
                "glob": {
                    "type": "string",
                    "description": "Optional glob matched against project-relative paths to restrict which files are searched, e.g. `**/*.ts`."
                },
                "regex": {
                    "type": "boolean",
                    "description": "Treat pattern as a regular expression (default false = literal substring)."
                },
                "ignoreCase": {
                    "type": "boolean",
                    "description": "Case-insensitive match (default false)."
                },
                "maxMatches": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional maximum number of matches to return (default 200)."
                },
                "includeIgnored": {
                    "type": "boolean",
                    "description": "Search files normally hidden by .gitignore/.ignore and dotfiles (default false). Sensitive files are still excluded."
                }
            },
            "required": ["pattern"],
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
    fn canonical_catalog_bytes_are_stable_across_calls() {
        // Byte-for-byte identical on repeated serialization — the property the
        // provider prefix cache relies on.
        let a = canonical_catalog_bytes(&default_tool_catalog());
        let b = canonical_catalog_bytes(&default_tool_catalog());
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn canonical_json_is_key_order_independent() {
        // Two values that differ only in object-key insertion order serialize to
        // identical canonical bytes.
        let one = json!({ "b": 1, "a": { "y": 2, "x": 3 } });
        let two = json!({ "a": { "x": 3, "y": 2 }, "b": 1 });
        assert_eq!(canonical_json(&one), canonical_json(&two));
    }

    #[test]
    fn fingerprint_changes_when_a_tool_changes() {
        let base = default_tool_catalog();
        let mut mutated = base.clone();
        mutated[0].description.push_str(" (changed)");
        assert_ne!(catalog_fingerprint(&base), catalog_fingerprint(&mutated));
    }

    #[test]
    fn catalog_pin_detects_drift() {
        let base = default_tool_catalog();
        let pin = CatalogPin::pin(&base);
        // Same catalog: no drift.
        assert!(pin.check(&base).is_ok());
        assert_eq!(pin.fingerprint(), catalog_fingerprint(&base));

        // A reconnect that re-advertises a changed tool: drift is reported.
        let mut drifted = base.clone();
        drifted[1].description.push_str(" (silently changed on reconnect)");
        let err = pin.check(&drifted).expect_err("drift must be detected");
        assert_eq!(err.pinned_fingerprint, pin.fingerprint());
        assert_ne!(err.current_fingerprint, pin.fingerprint());
    }

    #[test]
    fn default_catalog_exposes_run_command() {
        let tools = default_tool_catalog();

        assert_eq!(tools[0].id, RUN_COMMAND_TOOL_ID);
        assert_eq!(tools[0].name, RUN_COMMAND_TOOL_NAME);
        assert_eq!(tools[0].parameters["required"][0], "program");
    }

    #[test]
    fn default_catalog_exposes_seven_tools() {
        let tools = default_tool_catalog();
        assert_eq!(tools.len(), 7);

        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                RUN_COMMAND_TOOL_NAME,
                READ_FILE_TOOL_NAME,
                WRITE_FILE_TOOL_NAME,
                EDIT_FILE_TOOL_NAME,
                APPLY_PATCH_TOOL_NAME,
                LIST_FILES_TOOL_NAME,
                SEARCH_TEXT_TOOL_NAME,
            ]
        );

        // Every file/search tool declares its required fields and forbids extras.
        for tool in tools.iter().skip(1) {
            assert_eq!(tool.parameters["additionalProperties"], false);
            assert!(tool.parameters["required"].is_array());
        }
    }

    #[test]
    fn search_tool_ids_and_required_fields() {
        let tools = default_tool_catalog();
        let list = tools
            .iter()
            .find(|tool| tool.name == LIST_FILES_TOOL_NAME)
            .expect("list_files present");
        assert_eq!(list.id, LIST_FILES_TOOL_ID);
        // list_files has no required fields.
        assert_eq!(list.parameters["required"].as_array().unwrap().len(), 0);

        let search = tools
            .iter()
            .find(|tool| tool.name == SEARCH_TEXT_TOOL_NAME)
            .expect("search_text present");
        assert_eq!(search.id, SEARCH_TEXT_TOOL_ID);
        assert_eq!(search.parameters["required"][0], "pattern");
    }
}
