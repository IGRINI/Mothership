//! Core-owned tool batch scheduling policy.
//!
//! Providers and sidecars may submit a batch of tool calls, but Core decides
//! whether that batch is safe to run concurrently.

use serde::Deserialize;

use crate::LlmToolCallRequest;

use super::catalog::{LIST_FILES_TOOL_NAME, RUN_COMMAND_TOOL_NAME, SEARCH_TEXT_TOOL_NAME};

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
    // The read-only search tools never mutate state, so a batch of them (or a mix
    // with read-only commands) is always safe to run concurrently. `read_file` is
    // intentionally NOT included here yet — its parallel-safety is a separate
    // follow-up.
    if request.name == LIST_FILES_TOOL_NAME || request.name == SEARCH_TEXT_TOOL_NAME {
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
    let command = args
        .windows(2)
        .find_map(|pair| {
            (pair[0].eq_ignore_ascii_case("-command") || pair[0].eq_ignore_ascii_case("-c"))
                .then(|| pair[1].trim())
        })
        .unwrap_or_else(|| args.first().map(String::as_str).unwrap_or_default());
    let lower = command.to_ascii_lowercase();

    lower.starts_with("get-childitem")
        || lower.starts_with("get-content")
        || lower.starts_with("get-location")
        || lower.starts_with("select-string")
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
    fn search_tools_are_parallel_safe() {
        // The read-only search tools are parallel-safe on their own and alongside
        // read-only commands.
        assert_eq!(
            tool_concurrency(&search_request("c1", LIST_FILES_TOOL_NAME)),
            ToolConcurrency::ParallelSafe
        );
        assert_eq!(
            tool_concurrency(&search_request("c2", SEARCH_TEXT_TOOL_NAME)),
            ToolConcurrency::ParallelSafe
        );

        let requests = vec![
            search_request("c1", LIST_FILES_TOOL_NAME),
            search_request("c2", SEARCH_TEXT_TOOL_NAME),
            run_command_request("c3", "git", &["status"]),
        ];
        assert_eq!(tool_batch_plan(&requests), ToolBatchPlan::Parallel);
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
