//! The reasoning settings a harness records per turn.

use serde::{Deserialize, Serialize};

/// A turn's reasoning settings, verbatim as the harness wrote them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningConfig {
    /// Reasoning effort (`low`, `medium`, `high`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Reasoning-summary mode (`auto`, `concise`, `detailed`, `none`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}
