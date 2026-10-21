//! Session sourcing: the record buckets burn derives from one harness
//! session. Ingest appends them to the ledger; ledger-less verbs analyze
//! them in memory.

use std::collections::HashSet;

use serde::Serialize;

use crate::reader::{
    build_inferences, CompactionEvent, ContentRecord, Inference, RequestIdLookup,
    SessionRelationshipRecord, ToolResultEventRecord, TurnRecord, UserTurnRecord,
};

/// Everything burn derives from one session.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRecords {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub compactions: Vec<CompactionEvent>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
    pub user_turns: Vec<UserTurnRecord>,
    #[serde(serialize_with = "serialize_request_ids")]
    pub request_id_lookup: RequestIdLookup,
    /// One row per model request, for a source whose turns are not one
    /// request each (Codex). `None` derives them from the turns and
    /// [`Self::request_id_lookup`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inferences: Option<Vec<Inference>>,
    /// A trailing task is still being written: its turn waits for the
    /// rollout to settle, so the session is read again on a later ingest.
    #[serde(skip)]
    pub pending: bool,
    /// The session is a fork whose evidence leaves out the history it
    /// copied from its parent (a Codex paginated fork). A turn the ledger
    /// holds for it beyond these records was billed from that copy.
    #[serde(skip)]
    pub copied_history: bool,
}

impl SessionRecords {
    /// One row per model request of the session's turns.
    pub(crate) fn inferences(&self) -> Vec<Inference> {
        match &self.inferences {
            Some(requests) => requests.clone(),
            None => build_inferences(&self.turns, &self.request_id_lookup),
        }
    }
}

/// How long a rollout must go unwritten before its open trailing task is
/// final: relayhistory's quiescence grace for a message still being
/// written. A model pauses between records while it thinks or a tool runs;
/// billing a task during such a pause would cut it short.
pub(crate) const TAIL_GRACE_MS: i64 = 120_000;

/// What a session is mapped against: the moment it is read at, and the
/// message ids of it the ledger already bills.
///
/// A Codex task that is still open when its rollout settles is billed
/// through its last usage snapshot; one that is billed so and then
/// continues bills the rest from where the ledger's record of it ends.
#[derive(Debug, Clone, Default)]
pub(crate) struct Billing {
    pub now_ms: i64,
    pub billed: HashSet<String>,
}

impl Billing {
    /// Read now, against no ledger.
    pub(crate) fn now() -> Self {
        Self::at(crate::util::time::now_ms())
    }

    /// Read long after every session settled, against no ledger.
    #[cfg(test)]
    pub(crate) fn settled() -> Self {
        Self::at(i64::MAX)
    }

    /// Read at `now_ms`, against no ledger.
    pub(crate) fn at(now_ms: i64) -> Self {
        Self {
            now_ms,
            billed: HashSet::new(),
        }
    }
}

fn serialize_request_ids<S: serde::Serializer>(
    lookup: &RequestIdLookup,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(lookup.len()))?;
    for (key, request_id) in lookup {
        seq.serialize_element(&(&key.session_id, &key.message_id, request_id))?;
    }
    seq.end()
}

mod delegated;
#[cfg(test)]
pub(crate) mod fixtures;
pub(crate) mod locate;
#[cfg(test)]
mod parity_tests;
mod relayhistory;
mod stage;
mod usage;

pub(crate) use delegated::{
    claude_children, delegated_children, records_with_children, split_delegated,
    summary_subagent_counts,
};
pub(crate) use relayhistory::{records_from_evidence, source_kind};

/// The evidence burn derives records from: everything but the prompt log
/// and commit links, which no record reads.
pub(crate) fn records_query() -> ai_hist::SessionQuery {
    use ai_hist::EvidenceKind;
    let mut query = ai_hist::SessionQuery::default();
    query.kinds = Some(vec![
        EvidenceKind::SessionEvent,
        EvidenceKind::ToolCall,
        EvidenceKind::FileEdit,
        EvidenceKind::Relationship,
        EvidenceKind::SessionMarker,
    ]);
    query
}

#[cfg(test)]
mod tests {
    use ai_hist::EvidenceKind;

    #[test]
    fn records_read_everything_but_the_prompt_log_and_commit_links() {
        let kinds = super::records_query().kinds.expect("an explicit kind list");
        for skipped in [EvidenceKind::History, EvidenceKind::CommitLink] {
            assert!(!kinds.contains(&skipped), "{skipped:?}");
        }
        for read in [
            EvidenceKind::SessionEvent,
            EvidenceKind::ToolCall,
            EvidenceKind::FileEdit,
            EvidenceKind::Relationship,
            EvidenceKind::SessionMarker,
        ] {
            assert!(kinds.contains(&read), "{read:?}");
        }
    }
}
