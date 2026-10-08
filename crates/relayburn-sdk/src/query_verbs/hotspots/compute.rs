//! Hotspot analyzers over in-memory records. The ledger verb and the
//! single-session analysis both feed these the same slices.

use super::*;
use crate::analyze::findings::PatternsResult;
use crate::analyze::ghost_surface::{GhostSurfaceFinding, GhostSurfaceInputs};
use crate::analyze::tool_call_patterns::ToolCallPatternFinding;
use crate::analyze::tool_output_bloat::ToolOutputBloat;
use crate::reader::{CompactionEvent, ContentRecord, ToolResultEventRecord};

/// The non-turn records hotspot analyzers read.
pub(crate) struct HotspotSideRecords<'a> {
    pub user_turns: &'a [UserTurnRecord],
    pub tool_result_events: &'a [ToolResultEventRecord],
    /// Content and compaction sidecars of a complete single-session read.
    /// With it, the pattern detectors use tool-result chronology, compaction
    /// losses, and content-enriched signatures; the ledger verbs pass `None`.
    pub session_detail: Option<SessionDetail<'a>>,
}

/// Sidecars only a complete single-session read carries.
pub(crate) struct SessionDetail<'a> {
    pub content: &'a [ContentRecord],
    pub compactions: &'a [CompactionEvent],
}

/// Inputs to [`detect_hotspots`] that come from the environment rather than
/// the session records.
pub(crate) struct HotspotEnvironment<'a> {
    /// Claude settings files the tool-output-bloat static check reads.
    pub settings: &'a [LoadedClaudeSettings],
    /// Installed-surface inventory for the ghost-surface detector; `None`
    /// skips it.
    pub ghost_surface: Option<GhostSurfaceInputs>,
    /// Finding kinds to detect (see `DEFAULT_HOTSPOTS_FINDING_KINDS`).
    pub wanted: &'a HashSet<String>,
}

/// Raw detector output, before it is flattened into [`WasteFinding`]s.
#[derive(Debug, Default)]
pub(crate) struct HotspotDetections {
    pub patterns: PatternsResult,
    pub tool_output_bloat: Vec<ToolOutputBloat>,
    pub ghost_surface: Vec<GhostSurfaceFinding>,
    pub tool_call_patterns: Vec<ToolCallPatternFinding>,
}

/// Run every wanted hotspot detector over `turns`.
pub(crate) fn detect_hotspots(
    turns: &[TurnRecord],
    side: &HotspotSideRecords<'_>,
    pricing: &PricingTable,
    env: &HotspotEnvironment<'_>,
) -> HotspotDetections {
    let user_turns_by_session = group_by_session(side.user_turns, |u| &u.session_id, None);
    let content_by_session = side
        .session_detail
        .as_ref()
        .map(|d| group_by_session(d.content, |c| &c.session_id, None));
    let patterns = detect_patterns(
        turns,
        &DetectPatternsOptions {
            pricing,
            compactions: side.session_detail.as_ref().map(|d| d.compactions),
            user_turns_by_session: Some(&user_turns_by_session),
            content_by_session: content_by_session.as_ref(),
            tool_result_events: side
                .session_detail
                .as_ref()
                .map(|_| side.tool_result_events),
        },
    );
    let wants = |kind: &str| env.wanted.contains(kind);
    HotspotDetections {
        patterns,
        tool_output_bloat: if wants("tool-output-bloat") {
            detect_tool_output_bloat(&DetectToolOutputBloatOptions {
                settings: env.settings,
                tool_result_events: side.tool_result_events,
                user_turns: side.user_turns,
                turns,
                pricing,
                threshold: None,
                min_occurrences: None,
            })
        } else {
            Vec::new()
        },
        ghost_surface: env
            .ghost_surface
            .as_ref()
            .filter(|_| wants("ghost-surface"))
            .map(detect_ghost_surface)
            .unwrap_or_default(),
        tool_call_patterns: if wants("tool-call-pattern") {
            detect_tool_call_patterns(turns, &DetectToolCallPatternsOptions { pricing })
        } else {
            Vec::new()
        },
    }
}

impl HotspotDetections {
    /// The wanted detections as one ranked [`WasteFinding`] list, with
    /// unpriced sessions marked and an `unpriced-usage` finding when wanted.
    pub(crate) fn waste_findings(
        &self,
        turns: &[TurnRecord],
        pricing: &PricingTable,
        wanted: &HashSet<String>,
    ) -> Vec<WasteFinding> {
        let mut findings: Vec<WasteFinding> = findings_from_patterns(&self.patterns)
            .into_iter()
            .filter(|f| wanted.contains(&f.kind))
            .collect();
        findings.extend(
            self.tool_output_bloat
                .iter()
                .map(tool_output_bloat_to_finding),
        );
        let options = GhostSurfaceFindingOptions::default();
        findings.extend(
            self.ghost_surface
                .iter()
                .map(|g| ghost_surface_to_finding(g, &options)),
        );
        findings.extend(
            self.tool_call_patterns
                .iter()
                .map(tool_call_pattern_to_finding),
        );
        if wanted.contains("unpriced-usage") {
            findings.extend(unpriced_usage_findings(turns, pricing));
        }
        mark_findings_with_unpriced_sessions(&mut findings, turns, pricing);
        sort_findings(&mut findings);
        findings
    }
}

/// Cost attribution of `turns` to files, bash commands, subagents and MCP
/// servers, shaped for `group_by`. Turns lacking tool-call / tool-result
/// coverage are excluded; when every turn lacks it the result is refused.
pub(crate) fn hotspots_attribution(
    turns: &[TurnRecord],
    side: &HotspotSideRecords<'_>,
    pricing: &PricingTable,
    group_by: Option<HotspotsGroupBy>,
) -> HotspotsResult {
    let mut eligible: Vec<TurnRecord> = Vec::new();
    let mut excluded_by_source = HotspotsExcludedBreakdown::default();
    for t in turns {
        if turn_passes_hotspots_coverage(t) {
            eligible.push(t.clone());
        } else {
            record_excluded_source(&mut excluded_by_source, t);
        }
    }
    let excluded = (turns.len() - eligible.len()) as u64;
    let summary_value = fidelity_summary_to_value(&summarize_fidelity(turns));
    let group = group_by.unwrap_or(HotspotsGroupBy::Attribution);

    if !turns.is_empty() && eligible.is_empty() {
        let refusal = format!(
            "{}/{} turns lack tool-call/tool-result coverage required for hotspots attribution",
            turns.len(),
            turns.len()
        );
        return refused_for_group(group, refusal, excluded, summary_value, excluded_by_source);
    }

    let session_ids: HashSet<String> = eligible.iter().map(|t| t.session_id.clone()).collect();
    let user_turns_by_session =
        group_by_session(side.user_turns, |u| &u.session_id, Some(&session_ids));
    // Bytes plumbing (#436): per-session tool-result events stamp
    // `output_bytes` / `output_truncated` onto each attribution row.
    let tool_result_events_by_session = group_by_session(
        side.tool_result_events,
        |e| &e.session_id,
        Some(&session_ids),
    );
    let content_by_session = side
        .session_detail
        .as_ref()
        .map(|d| group_by_session(d.content, |c| &c.session_id, Some(&session_ids)));
    let result = attribute_hotspots(
        &eligible,
        &AnalyzeHotspotsOptions {
            pricing,
            content_by_session: content_by_session.as_ref(),
            user_turns_by_session: Some(&user_turns_by_session),
            tool_result_events_by_session: Some(&tool_result_events_by_session),
        },
    );

    match group {
        HotspotsGroupBy::Bash => HotspotsResult::Bash {
            rows: aggregate_by_bash(&result.attributions),
            refused: None,
            refusal_reason: None,
        },
        HotspotsGroupBy::BashVerb => HotspotsResult::BashVerb {
            rows: aggregate_by_bash_verb(&result.attributions, parse_bash_verb),
            refused: None,
            refusal_reason: None,
        },
        HotspotsGroupBy::File => HotspotsResult::File {
            rows: aggregate_by_file(&result.attributions),
            refused: None,
            refusal_reason: None,
        },
        HotspotsGroupBy::Subagent => HotspotsResult::Subagent {
            rows: aggregate_by_subagent(&result.attributions),
            refused: None,
            refusal_reason: None,
        },
        HotspotsGroupBy::Findings => unreachable!("findings is handled before attribution"),
        HotspotsGroupBy::Attribution => HotspotsResult::Attribution(Box::new(attribution_result(
            result,
            HotspotsFidelityBlock {
                analyzed: eligible.len() as u64,
                excluded,
                summary: summary_value,
                refused: false,
                excluded_by_source,
            },
        ))),
    }
}

fn attribution_result(
    result: crate::analyze::hotspots::HotspotsResult,
    fidelity: HotspotsFidelityBlock,
) -> HotspotsAttributionResult {
    let even_split = result
        .session_totals
        .iter()
        .filter(|s| matches!(s.attribution_method, AttributionMethod::EvenSplit))
        .count();
    let degraded = !result.session_totals.is_empty()
        && (even_split as f64 / result.session_totals.len() as f64) >= 0.5;
    HotspotsAttributionResult {
        turns_analyzed: fidelity.analyzed,
        grand_total: result.grand_total,
        attributed_total: result.attributed_total,
        unattributed_total: result.unattributed_total,
        attribution_degraded: degraded,
        files: aggregate_by_file(&result.attributions),
        bash_verbs: aggregate_by_bash_verb(&result.attributions, parse_bash_verb),
        bash: aggregate_by_bash(&result.attributions),
        subagents: aggregate_by_subagent(&result.attributions),
        mcp_servers: aggregate_by_mcp_server(&result.attributions),
        sessions: result
            .session_totals
            .into_iter()
            .map(|s| HotspotsSessionTotal {
                session_id: s.session_id,
                grand_cost: s.grand_cost,
                attributed_cost: s.attributed_cost,
                unattributed_cost: s.unattributed_cost,
                attribution_method: s.attribution_method,
            })
            .collect(),
        fidelity,
        refused: None,
        refusal_reason: None,
    }
}
