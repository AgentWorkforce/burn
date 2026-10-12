//! Hotspot detector output paired with the evidence each detection names.

use super::super::document::FindingEvidence;
use super::FindingContext;
use crate::analyze::findings::PatternsResult;
use crate::analyze::findings::{
    cancellation_run_to_finding, compaction_loss_to_finding, edit_heavy_to_finding,
    edit_revert_to_finding, failure_run_to_finding, retry_loop_to_finding,
    skill_pruning_protection_to_finding, skill_recall_dup_to_finding, system_prompt_tax_to_finding,
};
use crate::analyze::tool_call_patterns::{ToolCallPatternCategory, ToolCallPatternFinding};
use crate::analyze::{
    cache_expiry_to_finding, ghost_surface_to_finding, tool_call_pattern_to_finding,
    tool_output_bloat_to_finding, CacheExpiry, GhostSurfaceFindingOptions, WasteFinding,
};
use crate::query_verbs::HotspotDetections;

/// What a detection adds to its [`WasteFinding`].
pub(super) struct Detail {
    pub evidence: FindingEvidence,
    /// A suggestion more specific than the finding code's.
    pub suggestion: Option<&'static str>,
}

impl From<FindingEvidence> for Detail {
    fn from(evidence: FindingEvidence) -> Self {
        Self {
            evidence,
            suggestion: None,
        }
    }
}

/// One detection: its finding and what it adds to it.
type Detection = (WasteFinding, Detail);

/// Every detection as a finding plus its detail, index-aligned.
pub(super) fn detections(
    d: &HotspotDetections,
    cx: &FindingContext<'_>,
) -> (Vec<WasteFinding>, Vec<Detail>) {
    let mut out: Vec<Detection> = Vec::new();
    run_detections(&d.patterns, cx, &mut out);
    edit_detections(&d.patterns, cx, &mut out);
    skill_detections(&d.patterns, cx, &mut out);
    surface_detections(d, &mut out);
    out.extend(
        d.tool_call_patterns
            .iter()
            .map(|t| tool_call_detection(t, cx)),
    );
    out.extend(d.cache_expiry.iter().map(cache_expiry_detection));
    out.into_iter().unzip()
}

/// Retry loops, failure runs, and cancellation runs.
fn run_detections(p: &PatternsResult, cx: &FindingContext<'_>, out: &mut Vec<Detection>) {
    out.extend(p.retry_loops.iter().map(|r| {
        let mut e = cx.turns(r.start_turn_index..=r.end_turn_index);
        e.tools = vec![r.tool.clone()];
        e.targets = r.target.iter().cloned().collect();
        (retry_loop_to_finding(r), e.into())
    }));
    out.extend(p.failure_runs.iter().map(|r| {
        let mut e = cx.turns(r.start_turn_index..=r.end_turn_index);
        e.tools = r.tools_involved.clone();
        (failure_run_to_finding(r), e.into())
    }));
    out.extend(p.cancelled_runs.iter().map(|r| {
        let mut e = cx.turns(r.start_turn_index..=r.end_turn_index);
        e.tools = r.tools_involved.clone();
        (cancellation_run_to_finding(r), e.into())
    }));
}

/// Compaction losses, edit reverts, and edit-heavy sessions.
fn edit_detections(p: &PatternsResult, cx: &FindingContext<'_>, out: &mut Vec<Detection>) {
    out.extend(p.compactions.iter().map(|c| {
        let e = FindingEvidence {
            turn_ids: c.preceding_message_id.iter().cloned().collect(),
            files: c
                .lost_work
                .as_ref()
                .map(|w| w.files.clone())
                .unwrap_or_default(),
            ..Default::default()
        };
        (compaction_loss_to_finding(c), e.into())
    }));
    out.extend(p.edit_reverts.iter().map(|c| {
        let mut e = cx.turns([c.first_edit_turn_index, c.revert_turn_index]);
        e.files = vec![c.file_path.clone()];
        (edit_revert_to_finding(c), e.into())
    }));
    out.extend(
        p.edit_heavy_sessions
            .iter()
            .map(|s| (edit_heavy_to_finding(s), FindingEvidence::default().into())),
    );
}

/// Skill recall duplicates, skill pruning protection, and system-prompt tax.
fn skill_detections(p: &PatternsResult, cx: &FindingContext<'_>, out: &mut Vec<Detection>) {
    out.extend(p.skill_recall_dups.iter().map(|s| {
        let mut e = cx.turns(s.first_turn_index..=s.last_turn_index);
        e.targets = vec![s.skill_name.clone()];
        (skill_recall_dup_to_finding(s), e.into())
    }));
    out.extend(p.skill_pruning_protection.iter().map(|s| {
        let mut e = cx.turns([s.invoked_turn_index, s.last_cached_turn_index]);
        e.targets = vec![s.skill_name.clone()];
        (skill_pruning_protection_to_finding(s), e.into())
    }));
    out.extend(p.system_prompt_taxes.iter().map(|s| {
        (
            system_prompt_tax_to_finding(s),
            FindingEvidence::default().into(),
        )
    }));
}

/// Tool-output bloat and ghost surface: findings about the installed
/// environment rather than turn ranges.
fn surface_detections(d: &HotspotDetections, out: &mut Vec<Detection>) {
    out.extend(d.tool_output_bloat.iter().map(|b| {
        let e = FindingEvidence {
            tools: vec![b.tool_name.clone()],
            ..Default::default()
        };
        (tool_output_bloat_to_finding(b), e.into())
    }));
    let ghost_options = GhostSurfaceFindingOptions::default();
    out.extend(d.ghost_surface.iter().map(|g| {
        let e = FindingEvidence {
            files: vec![g.path.clone()],
            ..Default::default()
        };
        (ghost_surface_to_finding(g, &ghost_options), e.into())
    }));
}

fn tool_call_detection(t: &ToolCallPatternFinding, cx: &FindingContext<'_>) -> Detection {
    let mut e = cx.turns(t.sample_turn_indexes.iter().copied());
    if t.category == ToolCallPatternCategory::EditCluster {
        e.files = t.evidence.clone();
    } else {
        e.targets = t.evidence.clone();
    }
    let detail = Detail {
        evidence: e,
        suggestion: Some(tool_call_suggestion(t.category)),
    };
    (tool_call_pattern_to_finding(t), detail)
}

/// A session's cache expiries, evidenced by the turns that re-wrote the
/// expired cache.
fn cache_expiry_detection(c: &CacheExpiry) -> Detection {
    let e = FindingEvidence {
        turn_ids: c.events.iter().map(|ev| ev.message_id.clone()).collect(),
        ..Default::default()
    };
    (cache_expiry_to_finding(c), e.into())
}

fn tool_call_suggestion(category: ToolCallPatternCategory) -> &'static str {
    match category {
        ToolCallPatternCategory::SearchSequence => {
            "Use one broader search (a single grep/rg with alternation or a glob) instead of a chain of narrow ones."
        }
        ToolCallPatternCategory::EditCluster => {
            "Make the related edits to the file in one multi-edit or patch instead of separate calls."
        }
        ToolCallPatternCategory::BashGitState => {
            "Ask for one bounded view (`git status -sb`, `git diff --stat`, `git log --oneline -n 10`) instead of repeated full status/diff/log output."
        }
        ToolCallPatternCategory::BashTestRun => {
            "Run tests with a quiet or summary reporter, or filter to the failing test, and tail the output instead of reading every suite's full log."
        }
        ToolCallPatternCategory::BashGhPr => {
            "Request only the fields the task reads (`gh pr view --json title,state`, `gh api --jq …`) instead of raw JSON."
        }
    }
}
