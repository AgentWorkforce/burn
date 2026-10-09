//! Codex: burn's records come from the rollout's task lifecycle rather
//! than from relayhistory's request grouping. A burn turn is one task
//! (`task_started` … `task_complete`), its usage is the difference of the
//! cumulative `token_count` snapshots across it, and everything a task
//! derives is emitted only once it commits.

use ai_hist::SessionEvidence;

use crate::source::SessionRecords;

mod context;
mod events;
mod records;
mod relationships;
mod snapshots;
mod targets;
mod tasks;

/// Burn's records for one Codex session.
pub(super) fn records(ev: &SessionEvidence) -> SessionRecords {
    let stream = events::stream(ev);
    let derived = tasks::Tasks::new(ev).run(&stream);
    let mut relationships = Vec::new();
    if derived.committed {
        relationships = relationships::session_meta(ev);
        relationships.extend(derived.subagents);
    }
    SessionRecords {
        turns: derived.turns,
        content: derived.content,
        compactions: derived.compactions,
        relationships,
        tool_result_events: derived.tool_result_events,
        user_turns: derived.user_turns,
        request_id_lookup: Default::default(),
    }
}
