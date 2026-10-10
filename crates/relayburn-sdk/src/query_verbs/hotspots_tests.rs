use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;

use super::*;

fn breakdown() -> HotspotsExcludedBreakdown {
    HotspotsExcludedBreakdown {
        sources: BTreeMap::from([(
            "codex".to_string(),
            HotspotsExcludedSourceRow {
                count: 3,
                missing: BTreeSet::from(["tool-call records".to_string()]),
                granularities: BTreeSet::from(["per-turn".to_string()]),
            },
        )]),
    }
}

fn refuse(group: HotspotsGroupBy) -> HotspotsResult {
    refused_for_group(
        group,
        "too sparse".to_string(),
        7,
        json!({"total": 9}),
        breakdown(),
    )
}

fn assert_refused_rows(rows_empty: bool, refused: Option<bool>, reason: Option<String>) {
    assert!(rows_empty);
    assert_eq!(refused, Some(true));
    assert_eq!(reason.as_deref(), Some("too sparse"));
}

#[test]
fn refused_row_groups_carry_reason_and_no_rows() {
    match refuse(HotspotsGroupBy::Bash) {
        HotspotsResult::Bash {
            rows,
            refused,
            refusal_reason,
        } => assert_refused_rows(rows.is_empty(), refused, refusal_reason),
        other => panic!("expected bash, got {other:?}"),
    }
    match refuse(HotspotsGroupBy::BashVerb) {
        HotspotsResult::BashVerb {
            rows,
            refused,
            refusal_reason,
        } => assert_refused_rows(rows.is_empty(), refused, refusal_reason),
        other => panic!("expected bash-verb, got {other:?}"),
    }
    match refuse(HotspotsGroupBy::File) {
        HotspotsResult::File {
            rows,
            refused,
            refusal_reason,
        } => assert_refused_rows(rows.is_empty(), refused, refusal_reason),
        other => panic!("expected file, got {other:?}"),
    }
    match refuse(HotspotsGroupBy::Subagent) {
        HotspotsResult::Subagent {
            rows,
            refused,
            refusal_reason,
        } => assert_refused_rows(rows.is_empty(), refused, refusal_reason),
        other => panic!("expected subagent, got {other:?}"),
    }
}

#[test]
fn refused_findings_group_carries_summary_only() {
    match refuse(HotspotsGroupBy::Findings) {
        HotspotsResult::Findings { findings, summary } => {
            assert!(findings.is_empty());
            assert_eq!(summary, json!({"total": 9}));
        }
        other => panic!("expected findings, got {other:?}"),
    }
}

#[test]
fn refused_attribution_group_reports_zeroed_totals_and_fidelity() {
    let HotspotsResult::Attribution(r) = refuse(HotspotsGroupBy::Attribution) else {
        panic!("expected attribution");
    };
    assert_eq!(r.turns_analyzed, 0);
    assert_eq!(r.grand_total, 0.0);
    assert_eq!(r.attributed_total, 0.0);
    assert_eq!(r.unattributed_total, 0.0);
    assert!(!r.attribution_degraded);
    assert!(r.sessions.is_empty());
    assert!(r.files.is_empty());
    assert!(r.bash_verbs.is_empty());
    assert!(r.bash.is_empty());
    assert!(r.subagents.is_empty());
    assert!(r.mcp_servers.is_empty());
    assert_eq!(r.fidelity.analyzed, 0);
    assert_eq!(r.fidelity.excluded, 7);
    assert_eq!(r.fidelity.summary, json!({"total": 9}));
    assert!(r.fidelity.refused);
    let codex = &r.fidelity.excluded_by_source.sources["codex"];
    assert_eq!(codex.count, 3);
    assert!(codex.missing.contains("tool-call records"));
    assert_eq!(r.refused, Some(true));
    assert_eq!(r.refusal_reason.as_deref(), Some("too sparse"));
}
