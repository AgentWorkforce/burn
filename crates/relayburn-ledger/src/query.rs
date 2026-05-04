//! Filter struct shared by all query verbs. Mirrors the TS `Query` type
//! so cross-tree consumers (CLI, MCP, SDK) ask the same questions.

use serde::{Deserialize, Serialize};

use relayburn_reader::SourceKind;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Query {
    /// Inclusive lower bound on `ts` (string compare; assumes ISO-8601).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Inclusive upper bound on `ts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Project filter. Matches against either `project` or `project_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceKind>,
}

impl Query {
    pub fn for_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            ..Default::default()
        }
    }
}
