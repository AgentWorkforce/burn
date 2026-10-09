//! Session sourcing: the record buckets burn derives from one harness
//! session. Ingest appends them to the ledger; ledger-less verbs analyze
//! them in memory.

use serde::Serialize;

use crate::reader::{
    CompactionEvent, ContentRecord, RequestIdLookup, SessionRelationshipRecord,
    ToolResultEventRecord, TurnRecord, UserTurnRecord,
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
pub(crate) use relayhistory::records_from_evidence;

/// Whether burn accounts for sessions of `source`.
pub(crate) fn is_accounted(source: ai_hist::Source) -> bool {
    relayhistory::source_kind(source).is_some()
}
