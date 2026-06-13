//! Core-owned tool batch scheduling policy.
//!
//! Providers and sidecars may submit a batch of tool calls, but Core decides
//! whether that batch is safe to run concurrently.

use serde::Deserialize;

use crate::LlmToolCallRequest;

use super::catalog::{READ_FILE_TOOL_NAME, RUN_COMMAND_TOOL_NAME, SEARCH_TEXT_TOOL_NAME};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ToolBatchPlan {
    Sequential,
    Parallel,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ToolConcurrency {
    ParallelSafe,
    Exclusive,
}

pub fn tool_batch_plan(requests: &[LlmToolCallRequest]) -> ToolBatchPlan {
    if requests.len() < 2 {
        return ToolBatchPlan::Sequential;
    }

    if requests
        .iter()
        .all(|request| tool_concurrency(request) == ToolConcurrency::ParallelSafe)
    {
        ToolBatchPlan::Parallel
    } else {
        ToolBatchPlan::Sequential
    }
}

pub fn tool_concurrency(request: &LlmToolCallRequest) -> ToolConcurrency {
    // The read-only file and search tools never mutate state, so a batch of them
    // (or a mix with read-only commands) is always safe to run concurrently.
    if request.name == READ_FILE_TOOL_NAME || request.name == SEARCH_TEXT_TOOL_NAME {
        return ToolConcurrency::ParallelSafe;
    }

    if request.name != RUN_COMMAND_TOOL_NAME {
        return ToolConcurrency::Exclusive;
    }

    let Ok(arguments) =
        serde_json::from_value::<RunCommandToolArguments>(request.arguments.clone())
    else {
        return ToolConcurrency::Exclusive;
    };

    if read_only_command(&arguments) {
        ToolConcurrency::ParallelSafe
    } else {
        ToolConcurrency::Exclusive
    }
}

fn read_only_command(arguments: &RunCommandToolArguments) -> bool {
    let program = program_name(&arguments.program);
    match program.as_str() {
        "git" => arguments
            .args
            .first()
            .map(|arg| {
                matches!(
                    arg.to_ascii_lowercase().as_str(),
                    "status"
                        | "diff"
                        | "log"
                        | "show"
                        | "branch"
                        | "rev-parse"
                        | "ls-files"
                        | "remote"
                )
            })
            .unwrap_or(false),
        "pwd" | "ls" | "dir" | "cat" | "type" => true,
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => {
            read_only_powershell(&arguments.args)
        }
        _ => false,
    }
}

fn read_only_powershell(args: &[String]) -> bool {
    let command = powershell_command_text(args);
    let trimmed = command.trim();
    if trimmed.is_empty() || contains_powershell_control_operator(trimmed) {
        return false;
    }

    matches!(
        trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "get-childitem" | "get-content" | "get-location" | "select-string"
    )
}

fn powershell_command_text(args: &[String]) -> String {
    for (index, arg) in args.iter().enumerate() {
        if arg.eq_ignore_ascii_case("-command") || arg.eq_ignore_ascii_case("-c") {
            return args[index + 1..].join(" ");
        }
    }
    args.first().cloned().unwrap_or_default()
}

fn contains_powershell_control_operator(command: &str) -> bool {
    command
        .chars()
        .any(|ch| matches!(ch, ';' | '|' | '&' | '\n' | '\r'))
}

fn program_name(program: &str) -> String {
    program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim()
        .to_ascii_lowercase()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunCommandToolArguments {
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_git_batch_can_run_in_parallel() {
        let requests = vec![
            run_command_request("call_1", "git", &["status"]),
            run_command_request("call_2", "git", &["diff"]),
        ];

        assert_eq!(tool_batch_plan(&requests), ToolBatchPlan::Parallel);
    }

    #[test]
    fn mutating_or_unknown_batch_stays_sequential() {
        let requests = vec![
            run_command_request("call_1", "git", &["status"]),
            run_command_request("call_2", "git", &["checkout", "main"]),
        ];

        assert_eq!(tool_batch_plan(&requests), ToolBatchPlan::Sequential);
    }

    #[test]
    fn search_text_is_parallel_safe() {
        // The read-only search tool is parallel-safe on its own and alongside
        // read-only commands.
        assert_eq!(
            tool_concurrency(&search_request("c1", SEARCH_TEXT_TOOL_NAME)),
            ToolConcurrency::ParallelSafe
        );

        let requests = vec![
            search_request("c1", SEARCH_TEXT_TOOL_NAME),
            run_command_request("c2", "git", &["status"]),
        ];
        assert_eq!(tool_batch_plan(&requests), ToolBatchPlan::Parallel);
    }

    #[test]
    fn read_file_is_parallel_safe() {
        // read_file is read-only: a single read is parallel-safe, and a batch
        // mixing reads with search_text and a read-only command runs concurrently.
        assert_eq!(
            tool_concurrency(&read_file_request("c0")),
            ToolConcurrency::ParallelSafe
        );

        let requests = vec![
            read_file_request("c1"),
            search_request("c2", SEARCH_TEXT_TOOL_NAME),
            run_command_request("c3", "git", &["status"]),
        ];
        assert_eq!(tool_batch_plan(&requests), ToolBatchPlan::Parallel);
    }

    #[test]
    fn read_file_with_mutating_command_stays_sequential() {
        // A read-only read_file mixed with a mutating command is NOT parallel-safe.
        let requests = vec![
            read_file_request("c1"),
            run_command_request("c2", "git", &["checkout", "main"]),
        ];
        assert_eq!(tool_batch_plan(&requests), ToolBatchPlan::Sequential);
    }

    #[test]
    fn simple_powershell_read_commands_are_parallel_safe() {
        for args in [
            &["-Command", "Get-ChildItem -Path src"][..],
            &["-Command", "Get-Content README.md"][..],
            &["-Command", "Get-Location"][..],
            &[
                "-Command",
                "Select-String -Path src\\main.rs -Pattern needle",
            ][..],
        ] {
            assert_eq!(
                tool_concurrency(&run_command_request("c1", "powershell.exe", args)),
                ToolConcurrency::ParallelSafe,
                "{args:?}"
            );
        }
    }

    #[test]
    fn compound_powershell_commands_are_not_parallel_safe() {
        for args in [
            &["-Command", "Get-ChildItem; Remove-Item -Recurse src"][..],
            &["-Command", "Get-ChildItem | Remove-Item"][..],
            &["-Command", "Get-Content README.md && Remove-Item README.md"][..],
            &["-Command", "Get-ChildItem`nRemove-Item -Recurse src"][..],
            &["-Command", "Get-ChildItemEvil -Path src"][..],
            &["-Command", "Get-ChildItem", ";", "Remove-Item", "src"][..],
        ] {
            assert_eq!(
                tool_concurrency(&run_command_request("c1", "powershell.exe", args)),
                ToolConcurrency::Exclusive,
                "{args:?}"
            );
        }
    }

    fn read_file_request(id: &str) -> LlmToolCallRequest {
        LlmToolCallRequest {
            run_id: Some("run".to_string()),
            tool_call_id: id.to_string(),
            name: READ_FILE_TOOL_NAME.to_string(),
            arguments: serde_json::json!({ "path": "src/lib.rs" }),
        }
    }

    fn search_request(id: &str, name: &str) -> LlmToolCallRequest {
        LlmToolCallRequest {
            run_id: Some("run".to_string()),
            tool_call_id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({ "pattern": "x" }),
        }
    }

    fn run_command_request(id: &str, program: &str, args: &[&str]) -> LlmToolCallRequest {
        LlmToolCallRequest {
            run_id: Some("run".to_string()),
            tool_call_id: id.to_string(),
            name: RUN_COMMAND_TOOL_NAME.to_string(),
            arguments: serde_json::json!({
                "program": program,
                "args": args,
            }),
        }
    }
}
