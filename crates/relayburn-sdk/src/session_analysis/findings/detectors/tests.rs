//! The evidence each hotspot detection names.

use serde_json::json;

use super::*;
use crate::analyze::{CacheExpiryEvent, CacheTtl, ExpiryCause};
use crate::reader::TurnRecord;

fn turn(index: u64) -> TurnRecord {
    serde_json::from_value(json!({
        "v": 1, "source": "codex", "sessionId": "s1", "messageId": format!("m{index}"),
        "turnIndex": index, "ts": "2026-04-20T00:00:00.000Z", "model": "gpt-5.4",
        "usage": {"input": 10, "output": 1, "reasoning": 0, "cacheRead": 0,
                  "cacheCreate5m": 0, "cacheCreate1h": 0},
        "toolCalls": []
    }))
    .unwrap()
}

fn from<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn tool_call_pattern(category: &str, samples: &[u64], evidence: &[&str]) -> ToolCallPatternFinding {
    from(json!({
        "source": "codex", "sessionId": "s1", "category": category, "occurrenceCount": 3,
        "estimatedTokensSaved": 100, "estimatedUsdSaved": 0.01,
        "sampleTurnIndexes": samples, "evidence": evidence
    }))
}

fn detected() -> (Vec<WasteFinding>, Vec<Detail>) {
    let patterns: PatternsResult = from(json!({
        "retryLoops": [], "failureRuns": [], "cancelledRuns": [],
        "compactions": [{
            "sessionId": "s1", "ts": "2026-04-20T00:00:00.000Z", "precedingMessageId": "m3",
            "tokensBeforeCompact": 1000, "cacheLostCost": 0.1,
            "lostWork": {"files": ["src/lib.rs"], "bashCount": 1, "editCount": 1, "readCount": 0}
        }],
        "editReverts": [], "editHeavySessions": [],
        "skillRecallDups": [{
            "sessionId": "s1", "skillName": "deploy", "callCount": 2,
            "firstTurnIndex": 1, "lastTurnIndex": 2, "cost": 0.1
        }],
        "skillPruningProtection": [], "systemPromptTaxes": [], "sessionSummaries": []
    }));
    let found = HotspotDetections {
        patterns,
        tool_output_bloat: vec![from(json!({
            "source": "claude-code", "kind": "observed-bloat", "toolName": "Bash",
            "evidencedMaxOutput": 90000, "occurrenceCount": 2, "cost": 0.2, "evidence": []
        }))],
        ghost_surface: vec![from(json!({
            "source": "claude-code", "kind": "ghost-skill", "path": "/skills/unused/SKILL.md",
            "sizeTokens": 500, "cost": 0.05, "costPerSession": 0.05, "sessionCount": 1
        }))],
        tool_call_patterns: vec![
            tool_call_pattern("edit-cluster", &[4], &["src/a.rs"]),
            tool_call_pattern("search-sequence", &[5], &["rg foo"]),
        ],
        cache_expiry: vec![CacheExpiry {
            session_id: "s1".into(),
            events: vec![CacheExpiryEvent {
                message_id: "m6".into(),
                gap_ms: 400_000,
                ttl: CacheTtl::FiveMinutes,
                cause: ExpiryCause::UserIdle,
                recreated_tokens: 5000,
                penalty_usd: 0.02,
            }],
        }],
        context_output_ratio: Vec::new(),
    };
    let turns: Vec<TurnRecord> = (0..8).map(turn).collect();
    detections(&found, &FindingContext::new(&turns))
}

#[test]
fn each_detection_names_what_it_found() {
    let (findings, details) = detected();
    let named: Vec<(&str, &FindingEvidence)> = findings
        .iter()
        .map(|f| f.kind.as_str())
        .zip(details.iter().map(|d| &d.evidence))
        .collect();
    let evidence = |turns: &[u64], ids: &[&str]| FindingEvidence {
        turn_indexes: turns.to_vec(),
        turn_ids: ids.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let expected = [
        FindingEvidence {
            turn_ids: vec!["m3".into()],
            files: vec!["src/lib.rs".into()],
            ..Default::default()
        },
        FindingEvidence {
            targets: vec!["deploy".into()],
            ..evidence(&[1, 2], &["m1", "m2"])
        },
        FindingEvidence {
            tools: vec!["Bash".into()],
            ..Default::default()
        },
        FindingEvidence {
            files: vec!["/skills/unused/SKILL.md".into()],
            ..Default::default()
        },
        FindingEvidence {
            files: vec!["src/a.rs".into()],
            ..evidence(&[4], &["m4"])
        },
        FindingEvidence {
            targets: vec!["rg foo".into()],
            ..evidence(&[5], &["m5"])
        },
        FindingEvidence {
            turn_ids: vec!["m6".into()],
            ..Default::default()
        },
    ];
    assert_eq!(named.len(), expected.len(), "{named:?}");
    for ((kind, got), want) in named.iter().zip(&expected) {
        assert_eq!(*got, want, "{kind}");
    }
}

#[test]
fn tool_call_patterns_suggest_a_fix_for_their_category() {
    let (_, details) = detected();
    let suggestions: Vec<Option<&str>> = details.iter().map(|d| d.suggestion).collect();
    assert_eq!(
        suggestions,
        [
            None,
            None,
            None,
            None,
            Some("Make the related edits to the file in one multi-edit or patch instead of separate calls."),
            Some("Use one broader search (a single grep/rg with alternation or a glob) instead of a chain of narrow ones."),
            None,
        ]
    );
}
