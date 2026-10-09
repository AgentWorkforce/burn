//! Rebuild the records of changed sessions and append them to the ledger.
//!
//! A change names the session a record is stored under. For a Claude
//! subagent that is its own agent id, but burn bills a subagent with the
//! session that spawned it, so a change there rebuilds the root session it
//! belongs to (with every delegated child folded in). Codex child threads
//! and OpenCode child sessions are sessions of their own in burn.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use ai_hist::SessionStore;
use ai_hist::{DiscoveryState, SessionEvidence, SessionIdentity, SessionQuery, SessionRef, Source};

use super::pending_stamps::{
    resolve_pending_stamps_for_session_in, PendingStampHarness, PendingStampSessionCandidate,
};
use super::IngestReport;
use crate::ledger::Ledger;
use crate::reader::{build_inferences, ContentStoreMode};
use crate::source::SessionRecords;
use crate::source::{delegated_children, records_from_evidence, records_with_children};

pub(super) struct ImportContext<'a> {
    pub content_mode: ContentStoreMode,
    pub ledger_home: Option<&'a Path>,
}

pub(super) struct Imported {
    pub report: IngestReport,
    /// Every session landed; the watermark may advance past them.
    pub complete: bool,
}

pub(super) fn import_sessions(
    ledger: &mut Ledger,
    store: &SessionStore,
    ctx: &ImportContext<'_>,
    changed: BTreeSet<SessionIdentity>,
) -> anyhow::Result<Imported> {
    let mut imported = Imported {
        report: IngestReport::empty(),
        complete: true,
    };
    for session in billed_sessions(store, changed)? {
        match import_one(ledger, store, ctx, &session) {
            Ok(report) => imported.report.merge(&report),
            Err(error) => {
                eprintln!(
                    "[burn] skipping {} session {}: {error:#}",
                    session.source_name, session.session_id
                );
                imported.complete = false;
            }
        }
    }
    Ok(imported)
}

/// The sessions burn bills the changed ones under: a Claude subagent's root
/// session in place of the subagent, every other session as itself.
fn billed_sessions(
    store: &SessionStore,
    changed: BTreeSet<SessionIdentity>,
) -> anyhow::Result<BTreeSet<SessionIdentity>> {
    let mut billed = BTreeSet::new();
    for session in changed {
        match session.source() {
            Some(Source::Claude) => billed.extend(delegation_roots(store, session)?),
            Some(source) if crate::source::is_accounted(source) => {
                billed.insert(session);
            }
            _ => {}
        }
    }
    Ok(billed)
}

/// The sessions at the top of `session`'s delegation chain; `session`
/// itself when nothing delegated to it.
fn delegation_roots(
    store: &SessionStore,
    session: SessionIdentity,
) -> anyhow::Result<Vec<SessionIdentity>> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    let mut frontier = vec![session];
    while let Some(next) = frontier.pop() {
        if !seen.insert(next.clone()) {
            continue;
        }
        let parents = store.delegated_by(&next)?;
        if parents.is_empty() {
            roots.push(next);
        } else {
            frontier.extend(parents);
        }
    }
    Ok(roots)
}

fn import_one(
    ledger: &mut Ledger,
    store: &SessionStore,
    ctx: &ImportContext<'_>,
    session: &SessionIdentity,
) -> anyhow::Result<IngestReport> {
    let Some(source) = session.source() else {
        return Ok(IngestReport::empty());
    };
    let reference = SessionRef::id(source, session.session_id.clone());
    let Some(evidence) = store.session(&reference, SessionQuery::default())? else {
        return Ok(IngestReport::empty());
    };
    // A Claude subagent with no spawner on record has no session to be
    // billed with.
    if source == Source::Claude && evidence.session.discovery_state == DiscoveryState::Delegated {
        return Ok(IngestReport::empty());
    }
    let mut records = match source {
        Source::Claude => records_with_children(&evidence, &delegated_children(store, &evidence)?),
        _ => records_from_evidence(&evidence),
    };
    if ctx.content_mode != ContentStoreMode::Full {
        records.content.clear();
    }
    let mut report = IngestReport {
        scanned_sessions: 1,
        ..IngestReport::empty()
    };
    report.appended_turns = ledger.append_turns(&records.turns)?;
    if report.appended_turns > 0 {
        report.ingested_sessions = 1;
        report.applied_pending_stamps = resolve_stamps(ledger, ctx, &evidence, &records);
    }
    append_derived(ledger, &records)?;
    Ok(report)
}

/// Fold any pending launcher stamp onto a session that just gained turns.
fn resolve_stamps(
    ledger: &mut Ledger,
    ctx: &ImportContext<'_>,
    evidence: &SessionEvidence,
    records: &SessionRecords,
) -> usize {
    let harness = match evidence.session.source {
        Source::Claude => PendingStampHarness::Claude,
        Source::Codex => PendingStampHarness::Codex,
        _ => PendingStampHarness::Opencode,
    };
    let candidate = PendingStampSessionCandidate {
        harness,
        session_id: evidence.session.session_id.clone(),
        session_path: evidence.session.raw_path.clone(),
        session_mtime_ms: written_at_ms(evidence),
        cwd: evidence
            .session
            .cwd
            .clone()
            .or_else(|| records.turns.first().and_then(|t| t.project.clone())),
    };
    match resolve_pending_stamps_for_session_in(ledger, &candidate, ctx.ledger_home) {
        Ok(resolved) => resolved.applied,
        Err(err) => {
            eprintln!(
                "[burn] pending stamp resolution failed for {}: {err}",
                candidate.session_id
            );
            0
        }
    }
}

/// When the session was last written: its transcript's mtime, else its
/// last recorded activity.
fn written_at_ms(evidence: &SessionEvidence) -> Option<i64> {
    let modified = evidence
        .session
        .raw_path
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok()?.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok());
    modified.or(evidence.session.last_activity_ms)
}

/// Append every record bucket beside the turns, plus the per-request
/// inferences rebuilt from the same turns. Each append skips what the
/// ledger already holds.
fn append_derived(ledger: &mut Ledger, records: &SessionRecords) -> anyhow::Result<()> {
    ledger.append_content(&records.content)?;
    ledger.append_compactions(&records.compactions)?;
    ledger.append_relationships(&records.relationships)?;
    ledger.append_tool_result_events(&records.tool_result_events)?;
    ledger.append_user_turns(&records.user_turns)?;
    let inferences = build_inferences(&records.turns, &records.request_id_lookup);
    ledger.append_inferences(&inferences)?;
    Ok(())
}
