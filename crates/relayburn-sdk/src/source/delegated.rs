//! Delegated work: the Claude subagents a session spawned, billed with it.
//!
//! relayhistory stores a Claude subagent's evidence under its own agent id
//! and keeps it out of the catalog: the subagent is part of the session
//! that spawned it, not a conversation of its own, and its usage is never
//! folded into its parent's. burn bills it with the root session the way it
//! bills an inline sidechain: each child turn is a turn of the root session
//! whose [`Subagent`] names the child (`agent_id`), its spawner
//! (`parent_agent_id`: the root session id, or the parent subagent's id
//! for nested work), the spawning tool use, and the agent type and
//! description from the delegation edge.
//!
//! Codex child threads are sessions of their own in burn (each rollout is
//! its own session, with a subagent edge naming its parent thread), so they
//! are never folded here.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use ai_hist::{
    DiscoveryState, EvidenceKind, Relationship, RelationshipSide, SessionEvidence, SessionIdentity,
    SessionQuery, SessionRef, SessionStore, Source,
};
use anyhow::{Context, Result};

use super::relayhistory::records_from_evidence;
use super::SessionRecords;
use crate::reader::{
    RelationshipType, SourceKind, Subagent, SubagentCounts, SubagentTranscript, ToolCall, TurnKey,
    TurnRecord,
};

/// Evidence of every Claude subagent `root` delegated work to, nested ones
/// included, in spawn order. Empty for other sources, and one indexed
/// lookup for a session that spawned nothing.
pub(crate) fn delegated_children(
    store: &SessionStore,
    root: &SessionEvidence,
) -> Result<Vec<SessionEvidence>> {
    if root.session.source != Source::Claude {
        return Ok(Vec::new());
    }
    claude_children(store, &root.session.session_id, super::records_query())
}

/// [`delegated_children`] of the Claude session `session_id`, each read
/// with `query`.
pub(crate) fn claude_children(
    store: &SessionStore,
    session_id: &str,
    query: SessionQuery,
) -> Result<Vec<SessionEvidence>> {
    let identity = SessionIdentity::new(Source::Claude.as_str(), session_id);
    let ids = store
        .delegated_descendants(&[identity])
        .context("list delegated subagents")?;
    let mut children = Vec::with_capacity(ids.len());
    for id in ids {
        let reference = SessionRef::id(Source::Claude, id.session_id.clone());
        let evidence = store
            .session(&reference, query.clone())
            .with_context(|| format!("read subagent {}", id.session_id))?;
        children
            .extend(evidence.filter(|ev| ev.session.discovery_state == DiscoveryState::Delegated));
    }
    children.sort_by(|a, b| {
        (a.session.first_activity_ms, &a.session.session_id)
            .cmp(&(b.session.first_activity_ms, &b.session.session_id))
    });
    Ok(children)
}

/// `root`'s records with each delegated child's (from
/// [`delegated_children`]) billed to it, re-keyed under the root session.
pub(crate) fn records_with_children(
    root: &SessionEvidence,
    children: &[SessionEvidence],
) -> SessionRecords {
    let mut records = records_from_evidence(root);
    let root_id = root.session.session_id.as_str();
    let known: HashSet<&str> = std::iter::once(root_id)
        .chain(children.iter().map(|c| c.session.session_id.as_str()))
        .collect();
    for child in children {
        if let Some(sub) = subagent_of(child, &known, root_id) {
            fold_child(&mut records, root_id, sub, records_from_evidence(child));
        }
    }
    records
}

/// The delegation edge that spawned `child`.
fn spawn_edge(child: &SessionEvidence) -> Option<&Relationship> {
    let id = child.session.session_id.as_str();
    child.relationships.iter().find(|r| {
        r.side == RelationshipSide::Child
            && r.relationship == "delegated"
            && r.child_session_id.as_deref() == Some(id)
    })
}

/// The [`Subagent`] every turn of `child` carries. A spawner outside the
/// loaded tree hangs the child under the root, so its spend stays in the
/// root's totals.
fn subagent_of(child: &SessionEvidence, known: &HashSet<&str>, root_id: &str) -> Option<Subagent> {
    let edge = spawn_edge(child)?;
    let spawner = Some(edge.parent_session_id.as_str())
        .filter(|p| known.contains(p) && *p != child.session.session_id)
        .unwrap_or(root_id);
    Some(Subagent {
        is_sidechain: true,
        parent_tool_use_id: edge.evidence_ref.clone(),
        agent_id: Some(child.session.session_id.clone()),
        parent_agent_id: Some(spawner.to_string()),
        subagent_type: edge.child_agent_type.clone(),
        description: edge.child_agent_name.clone(),
    })
}

/// Append one child's records to `out` under `root_id`. A turn the root
/// already holds (the same provider message id) is billed once.
fn fold_child(out: &mut SessionRecords, root_id: &str, sub: Subagent, child: SessionRecords) {
    let held: HashSet<String> = out.turns.iter().map(|t| t.message_id.clone()).collect();
    let dropped: HashSet<String> = child
        .turns
        .iter()
        .map(|t| t.message_id.clone())
        .filter(|id| held.contains(id))
        .collect();
    let kept = |message_id: &str| !dropped.contains(message_id);
    let start = out.turns.len() as u64;
    let turns = child.turns.into_iter().filter(|t| kept(&t.message_id));
    for (turn_index, mut turn) in (start..).zip(turns) {
        turn.session_id = root_id.to_string();
        turn.subagent = Some(sub.clone());
        turn.turn_index = turn_index;
        out.turns.push(turn);
    }
    out.content.extend(
        child
            .content
            .into_iter()
            .filter(|c| kept(&c.message_id))
            .map(|mut c| {
                c.session_id = root_id.to_string();
                c
            }),
    );
    out.tool_result_events.extend(
        child
            .tool_result_events
            .into_iter()
            .filter(|e| e.message_id.as_deref().is_none_or(kept))
            .map(|mut e| {
                e.session_id = root_id.to_string();
                e
            }),
    );
    out.user_turns
        .extend(child.user_turns.into_iter().map(|mut u| {
            u.session_id = root_id.to_string();
            u
        }));
    out.compactions
        .extend(child.compactions.into_iter().map(|mut c| {
            c.session_id = root_id.to_string();
            c
        }));
    // The child's own subagent edges name the work it delegated in turn;
    // its root row is the root session's.
    out.relationships.extend(
        child
            .relationships
            .into_iter()
            .filter(|r| r.relationship_type == RelationshipType::Subagent)
            .map(|mut r| {
                r.session_id = root_id.to_string();
                r
            }),
    );
    for (key, request_id) in child.request_id_lookup {
        if kept(&key.message_id) {
            let key = TurnKey {
                session_id: root_id.to_string(),
                ..key
            };
            out.request_id_lookup.insert(key, request_id);
        }
    }
}

/// One [`SubagentTranscript`] per delegated child, paired to the tool use
/// in `turns` that spawned it. A child spawned from outside `turns` (by
/// another subagent) is unpaired and placed by its start time.
fn subagent_transcripts(
    turns: &[TurnRecord],
    children: &[SessionEvidence],
) -> Vec<SubagentTranscript> {
    let calls: HashSet<&str> = turns
        .iter()
        .flat_map(|t| t.tool_calls.iter().map(|c: &ToolCall| c.id.as_str()))
        .collect();
    let mut out: Vec<SubagentTranscript> = children
        .iter()
        .filter_map(|child| {
            let edge = spawn_edge(child)?;
            let tool_use = edge.evidence_ref.clone();
            Some(SubagentTranscript {
                agent_id: child.session.session_id.clone(),
                agent_type: edge.child_agent_type.clone(),
                description: edge.child_agent_name.clone(),
                meta_tool_use_id: tool_use.clone(),
                started_at_ms: child.session.first_activity_ms,
                paired_tool_use_id: tool_use.filter(|id| calls.contains(id.as_str())),
                source_path: child.session.raw_path.clone().unwrap_or_else(PathBuf::new),
            })
        })
        .collect();
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    out
}

/// The turns of the root's own conversation (`turns` without the delegated
/// children's), and one [`SubagentTranscript`] per child paired to them.
pub(crate) fn split_delegated(
    turns: &[TurnRecord],
    children: &[SessionEvidence],
) -> (Vec<TurnRecord>, Vec<SubagentTranscript>) {
    let delegated = child_ids(children);
    let own: Vec<TurnRecord> = turns
        .iter()
        .filter(|t| {
            !t.subagent
                .as_ref()
                .and_then(|s| s.agent_id.as_deref())
                .is_some_and(|id| delegated.contains(id))
        })
        .cloned()
        .collect();
    let transcripts = subagent_transcripts(&own, children);
    (own, transcripts)
}

/// Ids of the delegated children, whose turns are subagent work rather than
/// turns of the root's own conversation.
fn child_ids(children: &[SessionEvidence]) -> HashSet<&str> {
    children
        .iter()
        .map(|c| c.session.session_id.as_str())
        .collect()
}

/// Paired / orphan counts of the subagents the Claude sessions in `turns`
/// delegated work to, from the default relayhistory store's delegation
/// edges. A subagent pairs when the tool use that spawned it is among its
/// session's own turns. Zero when there is no store.
pub(crate) fn summary_subagent_counts(turns: &[TurnRecord]) -> SubagentCounts {
    let mut counts = SubagentCounts::default();
    let Ok(store) = super::locate::open_existing_store(&Default::default()) else {
        return counts;
    };
    let mut sessions: BTreeMap<&str, Vec<TurnRecord>> = BTreeMap::new();
    for turn in turns.iter().filter(|t| t.source == SourceKind::ClaudeCode) {
        sessions
            .entry(turn.session_id.as_str())
            .or_default()
            .push(turn.clone());
    }
    let mut edges_only = SessionQuery::default();
    edges_only.include_text = false;
    edges_only.kinds = Some(vec![EvidenceKind::Relationship]);
    for (session_id, session_turns) in sessions {
        let Ok(children) = claude_children(&store, session_id, edges_only.clone()) else {
            continue;
        };
        for transcript in split_delegated(&session_turns, &children).1 {
            if transcript.paired_tool_use_id.is_some() {
                counts.paired += 1;
            } else {
                counts.orphan += 1;
            }
        }
    }
    counts
}

#[cfg(test)]
#[path = "delegated_tests.rs"]
mod tests;
