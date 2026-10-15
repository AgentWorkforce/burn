//! Refusal and degradation rules of hotspot attribution.

use super::*;
use crate::analyze::hotspots::{HotspotsResult as Attributed, SessionTotals};
use crate::analyze::pricing::load_builtin_pricing;
use crate::reader::{Coverage, Fidelity, SourceKind, Usage, UsageGranularity};

/// A turn whose record says it carries no tool-call records.
fn uncovered_turn(message_id: &str) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::Codex,
        session_id: "s1".into(),
        session_path: None,
        message_id: message_id.into(),
        turn_index: 0,
        request_count: None,
        ts: "2026-04-20T00:00:00.000Z".into(),
        model: "gpt-5.4".into(),
        project: None,
        project_key: None,
        usage: Usage::default(),
        tool_calls: Vec::new(),
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: Some(Fidelity::new(
            UsageGranularity::PerTurn,
            Coverage {
                has_tool_calls: false,
                ..Coverage::default()
            },
        )),
        reasoning: None,
    }
}

fn attribution(turns: &[TurnRecord]) -> HotspotsAttributionResult {
    let side = HotspotSideRecords {
        user_turns: &[],
        tool_result_events: &[],
        session_detail: None,
    };
    let pricing = load_builtin_pricing();
    match hotspots_attribution(turns, &side, &pricing, None) {
        HotspotsResult::Attribution(result) => *result,
        other => panic!("expected attribution, got {other:?}"),
    }
}

#[test]
fn hotspots_are_refused_only_when_every_turn_lacks_coverage() {
    let refused = attribution(&[uncovered_turn("m1"), uncovered_turn("m2")]);
    assert_eq!(refused.refused, Some(true));
    assert_eq!(
        refused.refusal_reason.as_deref(),
        Some("2/2 turns lack tool-call/tool-result coverage required for hotspots attribution")
    );
    let empty = attribution(&[]);
    assert_ne!(empty.refused, Some(true), "no turns is nothing to refuse");
}

fn session(method: AttributionMethod) -> SessionTotals {
    SessionTotals {
        session_id: "s".into(),
        grand_cost: 1.0,
        attributed_cost: 1.0,
        unattributed_cost: 0.0,
        attribution_method: method,
    }
}

fn degraded(methods: &[AttributionMethod]) -> bool {
    let result = Attributed {
        attributions: Vec::new(),
        session_totals: methods.iter().copied().map(session).collect(),
        grand_total: 0.0,
        attributed_total: 0.0,
        unattributed_total: 0.0,
    };
    let fidelity = HotspotsFidelityBlock {
        analyzed: 0,
        excluded: 0,
        summary: serde_json::Value::Null,
        refused: false,
        excluded_by_source: HotspotsExcludedBreakdown::default(),
    };
    attribution_result(result, fidelity).attribution_degraded
}

#[test]
fn attribution_is_degraded_when_half_the_sessions_split_evenly() {
    use AttributionMethod::{EvenSplit, Sized};
    assert!(!degraded(&[]), "no sessions, nothing degraded");
    assert!(degraded(&[EvenSplit, Sized]));
    assert!(!degraded(&[EvenSplit, Sized, Sized]));
    assert!(!degraded(&[Sized]));
}
