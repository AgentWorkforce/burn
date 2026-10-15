//! Codex: burn's records come from the rollout's task lifecycle rather
//! than from relayhistory's request grouping. A burn turn is one task
//! (`task_started` … `task_complete` / `turn_aborted`), its usage is the
//! difference of the cumulative `token_count` snapshots across it, and
//! everything a task derives is emitted only once it commits. Each
//! snapshot that moves the running total inside a task ends one model
//! request: the turn counts them (`request_count`) and each becomes an
//! inference row carrying its own share of the task's spend.

use ai_hist::SessionEvidence;

use crate::source::{Billing, SessionRecords};

mod context;
mod events;
mod records;
mod relationships;
mod snapshots;
mod targets;
mod tasks;

/// Burn's records for one Codex session.
pub(super) fn records(ev: &SessionEvidence, billing: &Billing) -> SessionRecords {
    let stream = events::stream(ev);
    let derived = tasks::Tasks::new(ev, billing).run(&stream);
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
        inferences: Some(derived.inferences),
        pending: derived.pending,
    }
}
