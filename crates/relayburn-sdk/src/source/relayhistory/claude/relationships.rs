//! Claude session relationships beyond the root: the explicit
//! `continuedFromSessionId` / `forkSessionId` edges a record names, and one
//! subagent edge per resolved sidechain agent.

use std::collections::HashSet;

use ai_hist::{Relationship, RelationshipSide, SessionEvidence};

use crate::reader::types::{
    RelationshipSourceKind, RelationshipType, SessionRelationshipRecord, TurnRecord,
};
use crate::util::time::format_iso_ms;

pub(super) fn refine(
    ev: &SessionEvidence,
    turns: &[TurnRecord],
    relationships: &mut Vec<SessionRelationshipRecord>,
) {
    let version = relationships.first().and_then(|r| r.source_version.clone());
    relationships.extend(explicit(ev));
    relationships.extend(subagents(turns));
    for row in relationships.iter_mut() {
        if row.source_version.is_none() {
            row.source_version = version.clone();
        }
    }
}

fn explicit(ev: &SessionEvidence) -> Vec<SessionRelationshipRecord> {
    let session_id = ev.session.session_id.as_str();
    let mut edges: Vec<&Relationship> = ev
        .relationships
        .iter()
        .filter(|r| r.side == RelationshipSide::Continuity)
        .filter(|r| r.child_session_id.as_deref() == Some(session_id))
        .filter(|r| r.parent_session_id != session_id)
        .collect();
    // The evidence dates an edge by its session's first activity and names
    // no record position, so ties order by edge kind.
    edges.sort_by(|a, b| {
        (a.spawned_at_ms, &a.relationship).cmp(&(b.spawned_at_ms, &b.relationship))
    });
    edges
        .into_iter()
        .filter_map(|r| {
            let relationship_type = match r.evidence_ref.as_deref()? {
                "continuedFromSessionId" => RelationshipType::Continuation,
                "forkSessionId" => RelationshipType::Fork,
                _ => return None,
            };
            Some(SessionRelationshipRecord {
                v: 1,
                source: RelationshipSourceKind::ClaudeCode,
                session_id: session_id.to_string(),
                related_session_id: Some(r.parent_session_id.clone()),
                relationship_type,
                ts: r.spawned_at_ms.map(format_iso_ms),
                source_session_id: None,
                source_version: None,
                parent_tool_use_id: None,
                agent_id: None,
                subagent_type: None,
                description: None,
            })
        })
        .collect()
}

fn subagents(turns: &[TurnRecord]) -> Vec<SessionRelationshipRecord> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for turn in turns {
        let Some(sub) = turn.subagent.as_ref() else {
            continue;
        };
        let Some(agent_id) = sub.agent_id.as_ref() else {
            continue;
        };
        if !seen.insert(agent_id.as_str()) {
            continue;
        }
        out.push(SessionRelationshipRecord {
            v: 1,
            source: RelationshipSourceKind::NativeClaude,
            session_id: turn.session_id.clone(),
            related_session_id: sub.parent_agent_id.clone(),
            relationship_type: RelationshipType::Subagent,
            ts: (!turn.ts.is_empty()).then(|| turn.ts.clone()),
            source_session_id: None,
            source_version: None,
            parent_tool_use_id: sub.parent_tool_use_id.clone(),
            agent_id: Some(agent_id.clone()),
            subagent_type: sub.subagent_type.clone(),
            description: sub.description.clone(),
        });
    }
    out
}
