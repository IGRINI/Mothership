//! Shared provider-adapter policy for bounded agentic tool turns.
//!
//! The host-side tool runtime detects no-progress repeated tool calls. This
//! adapter-side policy is only a broad safety budget for providers that keep
//! asking for tools with distinct arguments. Keep it here so provider adapters
//! share one contract without depending on the app core.

/// Default upper bound for model/tool turns inside one adapter chat request.
pub const DEFAULT_MAX_AGENTIC_TURNS: usize = 256;

/// Prompt used after the broad agentic safety budget is exhausted.
pub const DEFAULT_FINAL_SYNTHESIS_PROMPT: &str = "The agentic turn safety budget has been reached. Stop requesting tools. Using only the conversation and tool results already available, answer the user's original request. Be explicit if the result is partial, and suggest the next focused continuation if more inspection is needed.";

/// Fallback delta emitted if final synthesis fails or returns no visible answer.
pub const DEFAULT_FALLBACK_MESSAGE: &str = "\n\nStopped after reaching the agentic turn safety budget. The inspection is partial; ask to continue with a narrower area if more work is needed.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgenticTurnPolicy {
    max_turns: usize,
    final_synthesis_prompt: &'static str,
    fallback_message: &'static str,
}

impl AgenticTurnPolicy {
    pub const fn new(
        max_turns: usize,
        final_synthesis_prompt: &'static str,
        fallback_message: &'static str,
    ) -> Self {
        Self {
            max_turns,
            final_synthesis_prompt,
            fallback_message,
        }
    }

    pub const fn max_turns(self) -> usize {
        self.max_turns
    }

    pub const fn final_synthesis_prompt(self) -> &'static str {
        self.final_synthesis_prompt
    }

    pub const fn fallback_message(self) -> &'static str {
        self.fallback_message
    }
}

impl Default for AgenticTurnPolicy {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_AGENTIC_TURNS,
            DEFAULT_FINAL_SYNTHESIS_PROMPT,
            DEFAULT_FALLBACK_MESSAGE,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_has_actionable_limit_and_messages() {
        let policy = AgenticTurnPolicy::default();

        assert!(policy.max_turns() > 0);
        assert!(policy
            .final_synthesis_prompt()
            .contains("Stop requesting tools"));
        assert!(policy.fallback_message().contains("Stopped after reaching"));
    }
}
