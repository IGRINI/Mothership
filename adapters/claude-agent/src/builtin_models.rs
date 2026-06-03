#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ClaudeModelFamily {
    Opus,
    Sonnet,
    Haiku,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BuiltinClaudeModel {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) family: ClaudeModelFamily,
    pub(crate) recommended: bool,
}

pub(crate) const BUILTIN_CLAUDE_MODELS: &[BuiltinClaudeModel] = &[
    BuiltinClaudeModel {
        id: "opus[1m]",
        label: "Opus 4.8 (1M context)",
        family: ClaudeModelFamily::Opus,
        recommended: true,
    },
    BuiltinClaudeModel {
        id: "opus",
        label: "Opus 4.8",
        family: ClaudeModelFamily::Opus,
        recommended: false,
    },
    BuiltinClaudeModel {
        id: "sonnet",
        label: "Sonnet 4.6",
        family: ClaudeModelFamily::Sonnet,
        recommended: false,
    },
    BuiltinClaudeModel {
        id: "haiku",
        label: "Haiku 4.5",
        family: ClaudeModelFamily::Haiku,
        recommended: false,
    },
    BuiltinClaudeModel {
        id: "claude-opus-4-7",
        label: "Opus 4.7 Legacy",
        family: ClaudeModelFamily::Opus,
        recommended: false,
    },
    BuiltinClaudeModel {
        id: "claude-opus-4-7[1m]",
        label: "Opus 4.7 (1M context) Legacy",
        family: ClaudeModelFamily::Opus,
        recommended: false,
    },
    BuiltinClaudeModel {
        id: "claude-opus-4-6",
        label: "Opus 4.6 Legacy",
        family: ClaudeModelFamily::Opus,
        recommended: false,
    },
];
