//! Claude session relationship inference.
//!
//! Explicit (`forkSessionId` / `continuedFromSessionId`) and inferred
//! (parent-UUID continuation, shared-source fork, `/resume` markers) session
//! relationship reconstruction, plus subagent-spawn and compaction-event
//! annotation. Split out of `claude.rs`; the parse engine there drives these
//! helpers as it walks each line and after the per-file pass completes.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use super::{
    extract_plain_user_text_from_obj, string_field, SESSION_ID_KEYS, SOURCE_VERSION_KEYS,
    TIMESTAMP_KEYS,
};
use crate::reader::types::{
    CompactionEvent, RelationshipSourceKind, RelationshipType, SessionRelationshipRecord,
    ToolResultEventRecord, TurnRecord,
};

#[derive(Debug, Clone, Default)]
pub struct ClaudeRelationshipEvidence {
    pub file_session_id: Option<String>,
    pub first_ts: Option<String>,
    pub in_log_session_ids: Vec<String>,
    pub source_version: Option<String>,
    pub first_parent_uuid: Option<String>,
    pub seen_uuids: Vec<String>,
    pub has_resume_marker: bool,
    pub resume_target_session_id: Option<String>,
    pub explicit_continuation_target_session_ids: Option<Vec<String>>,
    pub explicit_fork_target_session_ids: Option<Vec<String>>,
    /// TS uses a module-level WeakSet to gate `firstParentUuid` to the very
    /// first non-sidechain user line. We carry the same gate inline.
    pub(super) user_seen: bool,
}

#[derive(Debug, Clone)]
pub struct ReconcileClaudeRelationshipsInput {
    pub evidence: ClaudeRelationshipEvidence,
}

pub(super) fn build_explicit_claude_relationships(
    line: &serde_json::Map<String, Value>,
    session_id: &str,
    fallback_ts: Option<&str>,
) -> Vec<SessionRelationshipRecord> {
    let mut rows = Vec::new();
    let fork = string_field(line, &["forkSessionId", "fork_session_id"], true);
    if let Some(ref fork_id) = fork {
        if fork_id != session_id {
            rows.push(build_explicit_claude_relationship(
                line,
                session_id,
                fork_id,
                RelationshipType::Fork,
                fallback_ts,
            ));
        }
    }
    let cont = string_field(
        line,
        &["continuedFromSessionId", "continued_from_session_id"],
        true,
    );
    if let Some(ref c) = cont {
        if c != session_id {
            rows.push(build_explicit_claude_relationship(
                line,
                session_id,
                c,
                RelationshipType::Continuation,
                fallback_ts,
            ));
        }
    }
    rows
}

pub(super) fn build_explicit_claude_relationship(
    line: &serde_json::Map<String, Value>,
    session_id: &str,
    related_session_id: &str,
    relationship_type: RelationshipType,
    fallback_ts: Option<&str>,
) -> SessionRelationshipRecord {
    let mut row = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::ClaudeCode,
        session_id: session_id.to_string(),
        related_session_id: Some(related_session_id.to_string()),
        relationship_type,
        ts: None,
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    let ts = string_field(line, TIMESTAMP_KEYS, true).or_else(|| fallback_ts.map(str::to_string));
    if let Some(t) = ts {
        row.ts = Some(t);
    }
    if let Some(s) = string_field(line, &["sourceSessionId", "source_session_id"], true) {
        row.source_session_id = Some(s);
    }
    if let Some(s) = string_field(line, SOURCE_VERSION_KEYS, true) {
        row.source_version = Some(s);
    }
    row
}

pub(super) fn record_explicit_relationship_evidence(
    evidence: &mut ClaudeRelationshipEvidence,
    line: &serde_json::Map<String, Value>,
) {
    if let Some(c) = string_field(
        line,
        &["continuedFromSessionId", "continued_from_session_id"],
        true,
    ) {
        evidence.explicit_continuation_target_session_ids = Some(append_unique(
            evidence.explicit_continuation_target_session_ids.clone(),
            c,
        ));
    }
    if let Some(f) = string_field(line, &["forkSessionId", "fork_session_id"], true) {
        evidence.explicit_fork_target_session_ids = Some(append_unique(
            evidence.explicit_fork_target_session_ids.clone(),
            f,
        ));
    }
}

pub(super) fn append_unique(values: Option<Vec<String>>, value: String) -> Vec<String> {
    let mut v = values.unwrap_or_default();
    if !v.iter().any(|s| s == &value) {
        v.push(value);
    }
    v
}

/// Owned, hashable identity for a relationship row. Used as a `HashSet` key
/// for cross-line dedup; cheap because the original `relationship_key` did one
/// `format!`-driven allocation per call but had to be re-run for every
/// candidate during `has_relationship`.
pub(super) type RelationshipKey = (&'static str, String, &'static str, String, String, String);

pub(super) fn relationship_key_borrowed(
    row: &SessionRelationshipRecord,
) -> (&'static str, &str, &'static str, &str, &str, &str) {
    (
        row.source.wire_str(),
        row.session_id.as_str(),
        row.relationship_type.wire_str(),
        row.related_session_id.as_deref().unwrap_or(""),
        row.agent_id.as_deref().unwrap_or(""),
        row.parent_tool_use_id.as_deref().unwrap_or(""),
    )
}

pub(super) fn relationship_key(row: &SessionRelationshipRecord) -> RelationshipKey {
    let b = relationship_key_borrowed(row);
    (
        b.0,
        b.1.to_string(),
        b.2,
        b.3.to_string(),
        b.4.to_string(),
        b.5.to_string(),
    )
}

pub(super) fn has_relationship(
    rows: &[SessionRelationshipRecord],
    row: &SessionRelationshipRecord,
) -> bool {
    let key = relationship_key_borrowed(row);
    rows.iter().any(|r| relationship_key_borrowed(r) == key)
}

pub(super) fn collect_subagent_relationships(
    turns: &[TurnRecord],
    out: &mut Vec<SessionRelationshipRecord>,
) {
    let mut seen = HashSet::new();
    for t in turns {
        let sub = match &t.subagent {
            Some(s) if s.is_sidechain => s,
            _ => continue,
        };
        let agent_id = match &sub.agent_id {
            Some(a) => a,
            None => continue,
        };
        if !seen.insert(agent_id.clone()) {
            continue;
        }
        let mut row = SessionRelationshipRecord {
            v: 1,
            source: RelationshipSourceKind::NativeClaude,
            session_id: t.session_id.clone(),
            related_session_id: sub.parent_agent_id.clone(),
            relationship_type: RelationshipType::Subagent,
            ts: None,
            source_session_id: None,
            source_version: None,
            parent_tool_use_id: sub.parent_tool_use_id.clone(),
            agent_id: Some(agent_id.clone()),
            subagent_type: sub.subagent_type.clone(),
            description: sub.description.clone(),
        };
        if !t.ts.is_empty() {
            row.ts = Some(t.ts.clone());
        }
        out.push(row);
    }
}

pub(super) fn record_evidence_from_line(evidence: &mut ClaudeRelationshipEvidence, line: &Value) {
    let lo = match line.as_object() {
        Some(o) => o,
        None => return,
    };
    if let Some(uuid) = lo.get("uuid").and_then(Value::as_str) {
        if !uuid.is_empty() {
            evidence.seen_uuids.push(uuid.to_string());
        }
    }
    if let Some(sid) = string_field(lo, SESSION_ID_KEYS, true) {
        if !evidence.in_log_session_ids.iter().any(|s| s == &sid) {
            evidence.in_log_session_ids.push(sid);
        }
        if evidence.first_ts.is_none() {
            evidence.first_ts = string_field(lo, TIMESTAMP_KEYS, true);
        }
    }
    if evidence.source_version.is_none() {
        evidence.source_version = string_field(lo, SOURCE_VERSION_KEYS, true);
    }
    let line_type = lo.get("type").and_then(Value::as_str).unwrap_or("");
    let is_sidechain = lo
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if line_type == "user" && !is_sidechain && !evidence.user_seen {
        evidence.user_seen = true;
        if let Some(p) = lo.get("parentUuid").and_then(Value::as_str) {
            if !p.is_empty() {
                evidence.first_parent_uuid = Some(p.to_string());
            }
        }
    }
}

pub(super) fn record_resume_marker(
    evidence: &mut ClaudeRelationshipEvidence,
    line: &serde_json::Map<String, Value>,
) {
    let text = match extract_plain_user_text_from_obj(line) {
        Some(t) if !t.is_empty() => t,
        _ => return,
    };
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return;
    }
    let after_slash = &trimmed[1..];
    let cmd_end = after_slash
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after_slash.len());
    let cmd = &after_slash[..cmd_end];
    let cmd_lower = cmd.to_lowercase();
    if cmd_lower != "resume" && cmd_lower != "continue" {
        return;
    }
    evidence.has_resume_marker = true;
    let rest = after_slash[cmd_end..].trim_start();
    if !rest.is_empty() && evidence.resume_target_session_id.is_none() {
        let token_end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        let token = &rest[..token_end];
        if !token.is_empty() {
            evidence.resume_target_session_id = Some(token.to_string());
        }
    }
}

pub(super) fn emit_local_continuation_from_resume(
    out: &mut Vec<SessionRelationshipRecord>,
    ev: &ClaudeRelationshipEvidence,
) {
    if !ev.has_resume_marker {
        return;
    }
    let fid = match ev.file_session_id.clone() {
        Some(s) => s,
        None => return,
    };
    let mut row = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::ClaudeCode,
        session_id: fid,
        related_session_id: ev.resume_target_session_id.clone(),
        relationship_type: RelationshipType::Continuation,
        ts: ev.first_ts.clone(),
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    if has_relationship(out, &row) {
        return;
    }
    apply_evidence_provenance(&mut row, ev);
    out.push(row);
}

pub(super) fn annotate_relationships_with_evidence(
    rows: &mut [SessionRelationshipRecord],
    ev: &ClaudeRelationshipEvidence,
) {
    for r in rows {
        apply_evidence_provenance(r, ev);
    }
}

pub(super) fn apply_evidence_provenance(
    row: &mut SessionRelationshipRecord,
    ev: &ClaudeRelationshipEvidence,
) {
    if row.source_session_id.is_none() {
        if let Some(f) = pick_foreign_session_id(ev) {
            row.source_session_id = Some(f);
        }
    }
    if row.source_version.is_none() {
        if let Some(ref v) = ev.source_version {
            row.source_version = Some(v.clone());
        }
    }
}

pub(super) fn pick_foreign_session_id(ev: &ClaudeRelationshipEvidence) -> Option<String> {
    let fid = ev.file_session_id.as_deref()?;
    for id in &ev.in_log_session_ids {
        if id != fid {
            return Some(id.clone());
        }
    }
    None
}

pub(super) fn annotate_spawn_events(events: &mut [ToolResultEventRecord], turns: &[TurnRecord]) {
    if events.is_empty() {
        return;
    }
    let mut agent_by_parent_tool_use: HashMap<String, String> = HashMap::new();
    for t in turns {
        let sub = match &t.subagent {
            Some(s) if s.is_sidechain => s,
            _ => continue,
        };
        if let (Some(p), Some(a)) = (&sub.parent_tool_use_id, &sub.agent_id) {
            agent_by_parent_tool_use
                .entry(p.clone())
                .or_insert_with(|| a.clone());
        }
    }
    if agent_by_parent_tool_use.is_empty() {
        return;
    }
    for ev in events {
        if let Some(a) = agent_by_parent_tool_use.get(&ev.tool_use_id) {
            ev.agent_id = Some(a.clone());
        }
    }
}

pub(super) fn annotate_compaction_events(events: &mut [CompactionEvent], turns: &[TurnRecord]) {
    if events.is_empty() {
        return;
    }
    let mut by_message_id: HashMap<&str, &TurnRecord> = HashMap::new();
    for t in turns {
        by_message_id.insert(t.message_id.as_str(), t);
    }
    for ev in events {
        if let Some(ref pmid) = ev.preceding_message_id {
            if let Some(t) = by_message_id.get(pmid.as_str()) {
                ev.tokens_before_compact = Some(t.usage.cache_read);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-file reconciliation.
// ---------------------------------------------------------------------------

pub fn reconcile_claude_session_relationships(
    inputs: &[ReconcileClaudeRelationshipsInput],
) -> Vec<SessionRelationshipRecord> {
    let mut out: Vec<SessionRelationshipRecord> = Vec::new();
    let usable: Vec<&ClaudeRelationshipEvidence> = inputs
        .iter()
        .map(|i| &i.evidence)
        .filter(|e| e.file_session_id.is_some())
        .collect();
    if usable.is_empty() {
        return out;
    }

    let mut uuid_to_file_session: HashMap<String, String> = HashMap::new();
    for ev in &usable {
        let sid = ev.file_session_id.as_ref().unwrap().clone();
        for u in &ev.seen_uuids {
            uuid_to_file_session
                .entry(u.clone())
                .or_insert_with(|| sid.clone());
        }
    }

    let mut continuation_of: HashMap<String, String> = HashMap::new();
    for ev in &usable {
        let sid = ev.file_session_id.as_ref().unwrap().clone();
        let parent_uuid = match &ev.first_parent_uuid {
            Some(p) => p.clone(),
            None => continue,
        };
        let parent_sid = match uuid_to_file_session.get(&parent_uuid) {
            Some(p) => p.clone(),
            None => continue,
        };
        if parent_sid == sid {
            continue;
        }
        continuation_of.insert(sid.clone(), parent_sid.clone());
        if ev.has_resume_marker
            && ev.resume_target_session_id.as_deref() == Some(parent_sid.as_str())
        {
            continue;
        }
        if has_explicit_target(&ev.explicit_continuation_target_session_ids, &parent_sid) {
            continue;
        }
        let mut row = SessionRelationshipRecord {
            v: 1,
            source: RelationshipSourceKind::ClaudeCode,
            session_id: sid,
            related_session_id: Some(parent_sid),
            relationship_type: RelationshipType::Continuation,
            ts: ev.first_ts.clone(),
            source_session_id: None,
            source_version: None,
            parent_tool_use_id: None,
            agent_id: None,
            subagent_type: None,
            description: None,
        };
        apply_evidence_provenance(&mut row, ev);
        out.push(row);
    }

    let mut by_source_session: Vec<(String, Vec<&ClaudeRelationshipEvidence>)> = Vec::new();
    for ev in &usable {
        let foreign = match pick_foreign_session_id(ev) {
            Some(f) => f,
            None => continue,
        };
        let fid = ev.file_session_id.as_deref().unwrap_or("");
        if foreign == fid {
            continue;
        }
        if let Some(entry) = by_source_session.iter_mut().find(|(k, _)| k == &foreign) {
            entry.1.push(ev);
        } else {
            by_source_session.push((foreign, vec![ev]));
        }
    }

    for (foreign, group) in &by_source_session {
        if group.len() < 2 {
            continue;
        }
        for ev in group {
            let sid = ev.file_session_id.clone().unwrap();
            if let Some(parent) = continuation_of.get(&sid) {
                if group
                    .iter()
                    .any(|g| g.file_session_id.as_deref() == Some(parent.as_str()))
                {
                    continue;
                }
            }
            if has_explicit_target(&ev.explicit_fork_target_session_ids, foreign) {
                continue;
            }
            let row = SessionRelationshipRecord {
                v: 1,
                source: RelationshipSourceKind::ClaudeCode,
                session_id: sid,
                related_session_id: Some(foreign.clone()),
                relationship_type: RelationshipType::Fork,
                ts: ev.first_ts.clone(),
                source_session_id: Some(foreign.clone()),
                source_version: ev.source_version.clone(),
                parent_tool_use_id: None,
                agent_id: None,
                subagent_type: None,
                description: None,
            };
            out.push(row);
        }
    }

    out
}

pub(super) fn has_explicit_target(targets: &Option<Vec<String>>, session_id: &str) -> bool {
    targets
        .as_ref()
        .is_some_and(|t| t.iter().any(|s| s == session_id))
}

pub(super) fn collect_explicit_claude_relationships_incremental(
    line: &serde_json::Map<String, Value>,
    evidence: &mut ClaudeRelationshipEvidence,
    out: &mut Vec<(u64, SessionRelationshipRecord)>,
    seen: &mut HashSet<RelationshipKey>,
    session_id: &str,
    fallback_ts: Option<&str>,
    line_offset: u64,
) {
    record_explicit_relationship_evidence(evidence, line);
    for row in build_explicit_claude_relationships(line, session_id, fallback_ts) {
        let key = relationship_key(&row);
        if !seen.insert(key) {
            continue;
        }
        out.push((line_offset, row));
    }
}

pub(super) fn derive_file_session_id_from_parts(
    file_session_id: Option<&str>,
    session_path: Option<&str>,
) -> Option<String> {
    // Mirrors the TS `deriveFileSessionId`: only honor explicit caller signals
    // (`fileSessionId` then `sessionPath` basename). Do NOT fall back to the
    // on-disk path the parser opened — that would canonicalize relationship
    // rows to the input filename for default-options callers, breaking joins
    // against the real in-log `sessionId` UUIDs.
    if let Some(s) = file_session_id {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(sp) = session_path {
        if !sp.is_empty() {
            return basename_without_ext(sp, "jsonl");
        }
    }
    None
}

pub(super) fn basename_without_ext(path: &str, ext: &str) -> Option<String> {
    let name = Path::new(path).file_name()?.to_str()?;
    let suffix = format!(".{}", ext);
    let stem = if let Some(stripped) = name.strip_suffix(&suffix) {
        stripped
    } else {
        name
    };
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

pub(super) fn new_evidence(file_session_id: Option<String>) -> ClaudeRelationshipEvidence {
    ClaudeRelationshipEvidence {
        file_session_id,
        ..ClaudeRelationshipEvidence::default()
    }
}
