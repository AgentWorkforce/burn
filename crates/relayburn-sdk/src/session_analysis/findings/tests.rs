//! Findings derived from turn outcomes, coverage and context growth.

use serde_json::json;

use super::*;
use crate::analyze::pricing::load_builtin_pricing;

fn turn(index: u64, stop: Option<&str>, input_known: bool, output_known: bool) -> TurnRecord {
    serde_json::from_value(json!({
        "v": 1, "source": "claude-code", "sessionId": "s1", "messageId": format!("m{index}"),
        "turnIndex": index, "ts": "2026-04-20T00:00:00.000Z", "model": "claude-sonnet-4-6",
        "usage": {"input": 100, "output": 10, "reasoning": 0, "cacheRead": 0,
                  "cacheCreate5m": 0, "cacheCreate1h": 0},
        "toolCalls": [],
        "stopReason": stop,
        "fidelity": {
            "granularity": "per-turn",
            "class": "usage-only",
            "coverage": {
                "hasInputTokens": input_known, "hasOutputTokens": output_known,
                "hasReasoningTokens": false, "hasCacheReadTokens": false,
                "hasCacheCreateTokens": false, "hasToolCalls": true,
                "hasToolResultEvents": true, "hasSessionRelationships": true,
                "hasRawContent": true
            }
        }
    }))
    .unwrap()
}

/// `(code, turn ids)` per finding.
fn named(findings: &[Finding]) -> Vec<(&str, Vec<&str>)> {
    findings
        .iter()
        .map(|f| {
            let ids = f.evidence.turn_ids.iter().map(String::as_str).collect();
            (f.code.as_str(), ids)
        })
        .collect()
}

#[test]
fn max_token_and_refusal_stops_are_findings_of_their_own_turns() {
    let turns = vec![
        turn(0, Some("max_tokens"), true, true),
        turn(1, Some("end_turn"), true, true),
        turn(2, Some("refusal"), true, true),
        turn(3, Some("max_tokens"), true, true),
    ];
    let cx = FindingContext::new(&turns);
    let found = stop_findings(&turns, &cx, &load_builtin_pricing());
    assert_eq!(
        named(&found),
        [
            ("max-tokens-stop", vec!["m0", "m3"]),
            ("refusal", vec!["m2"])
        ]
    );
    assert_eq!(found[0].title, "2 turn(s) hit the output token limit");
}

#[test]
fn turns_missing_either_token_count_are_unrecorded() {
    let turns = vec![
        turn(0, None, true, true),
        turn(1, None, false, false),
        turn(2, None, true, false),
    ];
    let cx = FindingContext::new(&turns);
    let found = fidelity_findings(&turns, &cx, None);
    assert_eq!(named(&found), [("usage-unrecorded", vec!["m1", "m2"])]);
    assert!(found[0]
        .explanation
        .starts_with("2 of 3 turns have no recorded input/output token counts."));

    let complete = vec![turn(0, None, true, true)];
    let cx = FindingContext::new(&complete);
    assert!(fidelity_findings(&complete, &cx, None).is_empty());
    let refused = fidelity_findings(&complete, &cx, Some("no tool coverage"));
    assert_eq!(named(&refused), [("attribution-unavailable", vec![])]);
}

fn growth(turn_id: &str, delta: i64, tools: &[&str]) -> crate::analyze::ContextDelta {
    let intervening: Vec<_> = tools
        .iter()
        .enumerate()
        .map(|(i, tool)| {
            json!({"kind": "tool-result", "toolUseId": format!("call{i}"), "toolName": tool,
                   "approxTokens": 10, "approxBytes": 40, "truncated": false})
        })
        .collect();
    serde_json::from_value(json!({
        "sessionId": "s1", "turnId": turn_id, "inferenceIdx": 2, "ownerRail": {"kind": "main"},
        "priorContextTokens": 1000, "currentContextTokens": 1000 + delta,
        "deltaTokens": delta, "intervening": intervening, "attributedCostUSD": 0.5
    }))
    .unwrap()
}

#[test]
fn large_context_growth_names_its_turn_and_the_tools_behind_it() {
    let report = ContextReport {
        peak_context_tokens: 0,
        peak_turn_id: None,
        compactions: Vec::new(),
        largest_growth: vec![
            growth(
                "m5",
                CONTEXT_GROWTH_FINDING_TOKENS,
                &["Bash", "Read", "Bash"],
            ),
            growth("m6", CONTEXT_GROWTH_FINDING_TOKENS - 1, &["Bash"]),
        ],
    };
    let found = context_findings(&report, true);
    assert_eq!(named(&found), [("context-growth", vec!["m5"])]);
    assert_eq!(found[0].evidence.tools, ["Bash", "Read"]);
    assert_eq!(
        found[0].impact.tokens,
        Some(CONTEXT_GROWTH_FINDING_TOKENS as u64)
    );
}

fn overhead_report() -> OverheadReport {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("CLAUDE.md"),
        format!(
            "# Build\n\n{}\n\n# Style\n\n{}\n",
            "Run the full build before committing. ".repeat(6),
            "Prefer small functions. ".repeat(4)
        ),
    )
    .unwrap();
    let mut options = crate::AnalyzeSessionOptions::new(crate::SessionLocator::Path {
        harness: crate::reader::Harness::ClaudeCode,
        path: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/claude/retry-loop.jsonl"),
    });
    options.project_dir = Some(project.path().to_path_buf());
    let analysis = crate::analyze_session(options).unwrap();
    analysis.overhead.data().expect("overhead section").clone()
}

#[test]
fn instruction_overhead_is_priced_per_turn_that_carried_the_section() {
    let report = overhead_report();
    let rides: Vec<u64> = report
        .attribution
        .per_file
        .iter()
        .flat_map(|f| &f.attribution.session_costs)
        .map(|s| s.riding_turns)
        .collect();
    assert_eq!(rides.len(), 1);
    let rides = rides[0];
    assert!(rides > 1, "the fixture carries CLAUDE.md on several turns");
    let found = overhead_findings(&report, true);
    assert_eq!(found.len(), report.trim.recommendations.len());
    for (finding, rec) in found.iter().zip(&report.trim.recommendations) {
        assert_eq!(finding.code, "instruction-overhead");
        assert_eq!(finding.evidence.files, [rec.file.as_str()]);
        assert_eq!(finding.evidence.targets, [rec.section.heading.as_str()]);
        assert_eq!(finding.impact.tokens, Some(rec.section.tokens * rides));
    }

    let mut unread = report.clone();
    for file in &mut unread.attribution.per_file {
        for session in &mut file.attribution.session_costs {
            session.riding_turns = 0;
        }
    }
    assert!(
        overhead_findings(&unread, true).is_empty(),
        "a section no turn carried costs nothing"
    );
}
