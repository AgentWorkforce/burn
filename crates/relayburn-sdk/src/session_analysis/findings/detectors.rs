//! Hotspot detector output paired with the evidence each detection names.

use super::super::document::FindingEvidence;
use super::FindingContext;
use crate::analyze::findings::{
    cancellation_run_to_finding, compaction_loss_to_finding, edit_heavy_to_finding,
    edit_revert_to_finding, failure_run_to_finding, retry_loop_to_finding,
    skill_pruning_protection_to_finding, skill_recall_dup_to_finding, system_prompt_tax_to_finding,
};
use crate::analyze::tool_call_patterns::ToolCallPatternCategory;
use crate::analyze::{
    ghost_surface_to_finding, tool_call_pattern_to_finding, tool_output_bloat_to_finding,
    GhostSurfaceFindingOptions, WasteFinding,
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

/// Every detection as a finding plus its detail, index-aligned.
pub(super) fn detections(
    d: &HotspotDetections,
    cx: &FindingContext<'_>,
) -> (Vec<WasteFinding>, Vec<Detail>) {
    let mut out: Vec<(WasteFinding, Detail)> = Vec::new();
    let p = &d.patterns;
    for r in &p.retry_loops {
        let mut e = cx.turns(r.start_turn_index..=r.end_turn_index);
        e.tools = vec![r.tool.clone()];
        e.targets = r.target.iter().cloned().collect();
        out.push((retry_loop_to_finding(r), e.into()));
    }
    for r in &p.failure_runs {
        let mut e = cx.turns(r.start_turn_index..=r.end_turn_index);
        e.tools = r.tools_involved.clone();
        out.push((failure_run_to_finding(r), e.into()));
    }
    for r in &p.cancelled_runs {
        let mut e = cx.turns(r.start_turn_index..=r.end_turn_index);
        e.tools = r.tools_involved.clone();
        out.push((cancellation_run_to_finding(r), e.into()));
    }
    for c in &p.compactions {
        let e = FindingEvidence {
            turn_ids: c.preceding_message_id.iter().cloned().collect(),
            files: c
                .lost_work
                .as_ref()
                .map(|w| w.files.clone())
                .unwrap_or_default(),
            ..Default::default()
        };
        out.push((compaction_loss_to_finding(c), e.into()));
    }
    for c in &p.edit_reverts {
        let mut e = cx.turns([c.first_edit_turn_index, c.revert_turn_index]);
        e.files = vec![c.file_path.clone()];
        out.push((edit_revert_to_finding(c), e.into()));
    }
    for s in &p.edit_heavy_sessions {
        out.push((edit_heavy_to_finding(s), FindingEvidence::default().into()));
    }
    for s in &p.skill_recall_dups {
        let mut e = cx.turns(s.first_turn_index..=s.last_turn_index);
        e.targets = vec![s.skill_name.clone()];
        out.push((skill_recall_dup_to_finding(s), e.into()));
    }
    for s in &p.skill_pruning_protection {
        let mut e = cx.turns([s.invoked_turn_index, s.last_cached_turn_index]);
        e.targets = vec![s.skill_name.clone()];
        out.push((skill_pruning_protection_to_finding(s), e.into()));
    }
    for s in &p.system_prompt_taxes {
        out.push((
            system_prompt_tax_to_finding(s),
            FindingEvidence::default().into(),
        ));
    }
    for b in &d.tool_output_bloat {
        let e = FindingEvidence {
            tools: vec![b.tool_name.clone()],
            ..Default::default()
        };
        out.push((tool_output_bloat_to_finding(b), e.into()));
    }
    let ghost_options = GhostSurfaceFindingOptions::default();
    for g in &d.ghost_surface {
        let e = FindingEvidence {
            files: vec![g.path.clone()],
            ..Default::default()
        };
        out.push((ghost_surface_to_finding(g, &ghost_options), e.into()));
    }
    for t in &d.tool_call_patterns {
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
        out.push((tool_call_pattern_to_finding(t), detail));
    }
    out.into_iter().unzip()
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
