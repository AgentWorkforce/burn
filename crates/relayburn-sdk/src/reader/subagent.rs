//! Delegated subagents as the span tree and summary see them.
//!
//! A Claude Code `Task`/`Agent` call spawns a subagent whose work
//! relayhistory stores under the subagent's own agent id. Each one is a
//! [`SubagentTranscript`]: who it is, what spawned it, and the tool use it
//! pairs to in the parent's turns. A transcript that pairs to no tool use is
//! an orphan, surfaced by presenters as the `UnattachedGroup` bucket
//! (slash-command synthetic dispatches are an expected source of these).

use std::path::PathBuf;

/// One delegated subagent of a session.
#[derive(Debug, Clone)]
pub struct SubagentTranscript {
    /// The subagent's agent id: the id its evidence is stored under, and
    /// the `agent_id` its billed turns carry.
    pub agent_id: String,
    /// The agent type the spawning call asked for, if recorded.
    pub agent_type: Option<String>,
    /// The spawning call's one-line description, if recorded.
    pub description: Option<String>,
    /// The tool use the delegation edge names as the spawn point.
    pub meta_tool_use_id: Option<String>,
    /// Epoch-millis of the subagent's earliest record, which places an
    /// unpaired subagent under the latest turn that started before it.
    pub started_at_ms: Option<i64>,
    /// The parent turn's tool use that spawned this subagent, when that
    /// tool use is among the parent's turns. `None` for orphans.
    pub paired_tool_use_id: Option<String>,
    /// The subagent transcript on disk, when relayhistory recorded one.
    pub source_path: PathBuf,
}

/// Paired / orphan subagent counts for the `burn summary` subagent line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentCounts {
    /// Subagents whose spawning tool use is among their session's turns.
    pub paired: u64,
    /// Subagents that pair to no tool use of their session.
    pub orphan: u64,
}

impl SubagentCounts {
    /// `true` when both counts are zero; presenters use this to skip the
    /// summary line entirely on sessions that never spawned a subagent.
    pub fn is_empty(&self) -> bool {
        self.paired == 0 && self.orphan == 0
    }

    /// Total subagents (paired + orphan).
    pub fn total(&self) -> u64 {
        self.paired + self.orphan
    }
}
