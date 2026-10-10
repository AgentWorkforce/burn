//! Which harness or API a ledger record came from.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    ClaudeCode,
    Codex,
    Opencode,
    CopilotCli,
    AnthropicApi,
    OpenaiApi,
    GeminiApi,
}

impl SourceKind {
    /// Kebab-case label as emitted on the wire (matches `#[serde(rename_all = "kebab-case")]`).
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::CopilotCli => "copilot-cli",
            Self::AnthropicApi => "anthropic-api",
            Self::OpenaiApi => "openai-api",
            Self::GeminiApi => "gemini-api",
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_str())
    }
}
