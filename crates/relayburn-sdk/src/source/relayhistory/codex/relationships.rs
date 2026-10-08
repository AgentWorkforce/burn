//! The session edges a Codex `session_meta` declares: the root row, and
//! the fork and continuation parents it names.

use ai_hist::{Relationship, RelationshipSide, SessionEvidence};

use crate::reader::types::{RelationshipSourceKind, RelationshipType, SessionRelationshipRecord};
use crate::util::time::format_iso_ms;

/// Root, then fork, then continuation.
pub(super) fn session_meta(ev: &SessionEvidence) -> Vec<SessionRelationshipRecord> {
    let session_id = ev.session.session_id.as_str();
    let declared = |kind: &str| {
        ev.relationships.iter().find(|r| {
            r.side == RelationshipSide::Continuity
                && r.relationship == kind
                && r.child_session_id.as_deref() == Some(session_id)
                && r.parent_session_id != session_id
        })
    };
    let (fork, continuation) = (declared("fork"), declared("continuation"));
    let origin = fork
        .or(continuation)
        .and_then(|r| r.origin_session_id.clone());
    let row = |relationship_type, related: Option<&Relationship>, ts_ms: Option<i64>| {
        SessionRelationshipRecord {
            v: 1,
            source: RelationshipSourceKind::Codex,
            session_id: session_id.to_string(),
            related_session_id: related.map(|r| r.parent_session_id.clone()),
            relationship_type,
            ts: ts_ms.map(format_iso_ms),
            source_session_id: origin.clone(),
            source_version: ev.session.agent_version.clone(),
            parent_tool_use_id: None,
            agent_id: None,
            subagent_type: None,
            description: None,
        }
    };
    let mut rows = vec![row(
        RelationshipType::Root,
        None,
        ev.session.first_activity_ms,
    )];
    for (relationship_type, related) in [
        (RelationshipType::Fork, fork),
        (RelationshipType::Continuation, continuation),
    ] {
        if let Some(related) = related {
            rows.push(row(relationship_type, Some(related), related.spawned_at_ms));
        }
    }
    rows
}
