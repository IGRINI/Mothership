//! Core-owned prompt composition.
//!
//! Provider adapters receive the structured bundle and may adapt it to their
//! provider's wire format, but the product prompt and runtime context originate
//! here.

use mothership_adapter_host::protocol::{PromptBundle, PromptSection};

use crate::llm::ProviderRuntimeKind;
use crate::ProjectSummary;

const BASE_SYSTEM_PROMPT: &str = "You are Mothership's local AI coding assistant.
Answer in the user's language unless the user asks otherwise.
Be direct, practical, and precise.
Use only the conversation context and tool results available in this request.
Do not claim that you edited files, ran commands, opened applications, or inspected the local machine unless a tool result shows it.
For non-trivial tool work, keep the user visibly oriented with short progress updates:
- Before a tool call, briefly say what observable action you are about to take when it helps the user follow the work.
- After a tool result, briefly state what you learned or what changed before choosing the next step.
- Do not reveal private chain-of-thought. Summarize actions, evidence, and decisions at a high level.

Runtime context:
- The local host operating system is {host_os}.
- Tool calls run through Mothership's supervised runtime, not directly inside the model provider.
- When using run_command, pass the executable as program and command arguments as args. Do not rely on POSIX shell syntax unless you explicitly invoke a shell.
- Prefer Windows-compatible commands. Simple read-only aliases such as pwd, ls, dir, cat, type, and grep are accepted and normalized by the runtime.
- If the project folder is not known, say so and ask the user to open or select a project before running project-specific commands.";

const SELF_MANAGED_AGENT_PROMPT: &str = "You are running inside Mothership as an upstream self-managed coding agent.
Answer in the user's language unless the user asks otherwise.
Be direct, practical, and precise.
Mothership displays this session and supervises process lifecycle, but this provider runtime owns its own tool execution, permissions, compaction, subagents, and continuation state.
Use the tools and rules exposed by this upstream agent runtime. Do not claim that Mothership Core executed a tool unless Mothership explicitly reports that.
Keep the user visibly oriented with short progress updates during non-trivial work, without revealing private chain-of-thought.";

pub fn runtime_prompt_bundle(project: Option<&ProjectSummary>) -> PromptBundle {
    runtime_prompt_bundle_for(project, ProviderRuntimeKind::CoreManaged)
}

pub fn runtime_prompt_bundle_for(
    project: Option<&ProjectSummary>,
    runtime_kind: ProviderRuntimeKind,
) -> PromptBundle {
    let base_prompt = match runtime_kind {
        ProviderRuntimeKind::CoreManaged => {
            BASE_SYSTEM_PROMPT.replace("{host_os}", std::env::consts::OS)
        }
        ProviderRuntimeKind::SelfManaged => SELF_MANAGED_AGENT_PROMPT.to_string(),
    };
    let mut sections = vec![PromptSection {
        id: "core.base".to_string(),
        source: "core".to_string(),
        priority: 0,
        locked: true,
        content: base_prompt,
    }];

    if let Some(project) = project {
        sections.push(PromptSection {
            id: "core.project".to_string(),
            source: "core".to_string(),
            priority: 10,
            locked: true,
            content: project_context_prompt(project, runtime_kind),
        });
    }

    PromptBundle { sections }
}

fn project_context_prompt(project: &ProjectSummary, runtime_kind: ProviderRuntimeKind) -> String {
    let base = format!(
        "Active project:\n- Project id: {id}\n- Name: {name}\n- Root directory: {path}",
        id = project.id,
        name = project.name,
        path = project.path,
    );
    match runtime_kind {
        ProviderRuntimeKind::CoreManaged => format!(
            "{base}\n- When run_command omits cwd, Mothership runs it from this root.\n- Use cwd only for subdirectories inside the project root."
        ),
        ProviderRuntimeKind::SelfManaged => format!(
            "{base}\n- Mothership passes this root as structured runtime context to the provider runtime.\n- Treat this root as the current project workspace unless the user asks otherwise."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_prompt_includes_host_os_and_tool_context() {
        let prompt = runtime_prompt_bundle(None).rendered_text();
        assert!(prompt.contains(std::env::consts::OS));
        assert!(prompt.contains("supervised runtime"));
        assert!(prompt.contains("run_command"));
    }

    #[test]
    fn self_managed_runtime_prompt_avoids_core_tool_contract() {
        let prompt =
            runtime_prompt_bundle_for(None, ProviderRuntimeKind::SelfManaged).rendered_text();
        assert!(prompt.contains("self-managed coding agent"));
        assert!(prompt.contains("provider runtime owns its own tool execution"));
        assert!(!prompt.contains("run_command"));
    }

    #[test]
    fn self_managed_project_context_does_not_reference_core_tools() {
        let project = ProjectSummary {
            id: "project_1".to_string(),
            name: "Mothership".to_string(),
            path: "E:\\Mothership".to_string(),
            chat_count: 1,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
            last_opened_at: "now".to_string(),
        };
        let prompt = runtime_prompt_bundle_for(Some(&project), ProviderRuntimeKind::SelfManaged)
            .rendered_text();

        assert!(prompt.contains("structured runtime context"));
        assert!(!prompt.contains("run_command"));
    }
}
