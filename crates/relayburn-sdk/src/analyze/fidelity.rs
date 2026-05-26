//! Fidelity summary aggregation — Rust port of
//! `packages/analyze/src/fidelity.ts`.
//!
//! Higher-level aggregators (compare, hotspots) use this to refuse stats on
//! undersized fixtures, so it lands before them.

use std::collections::HashMap;

use serde::Serialize;

use crate::reader::{
    classify_fidelity, Coverage, Fidelity, FidelityClass, TurnRecord, UsageGranularity,
};

/// Names of the boolean fields on [`Coverage`]. Mirrors the TS `keyof Coverage`
/// iteration order so the resulting map keys match the JSON shape callers
/// already serialize.
pub const COVERAGE_FIELDS: &[&str] = &[
    "hasInputTokens",
    "hasOutputTokens",
    "hasReasoningTokens",
    "hasCacheReadTokens",
    "hasCacheCreateTokens",
    "hasToolCalls",
    "hasToolResultEvents",
    "hasSessionRelationships",
    "hasRawContent",
];

fn coverage_get(c: &Coverage, field: &str) -> bool {
    match field {
        "hasInputTokens" => c.has_input_tokens,
        "hasOutputTokens" => c.has_output_tokens,
        "hasReasoningTokens" => c.has_reasoning_tokens,
        "hasCacheReadTokens" => c.has_cache_read_tokens,
        "hasCacheCreateTokens" => c.has_cache_create_tokens,
        "hasToolCalls" => c.has_tool_calls,
        "hasToolResultEvents" => c.has_tool_result_events,
        "hasSessionRelationships" => c.has_session_relationships,
        "hasRawContent" => c.has_raw_content,
        _ => unreachable!("unknown coverage field: {field}"),
    }
}

/// What every command needs to honestly describe a slice of turns:
///   - how many turns landed in each [`FidelityClass`]
///   - how many turns are missing each individual coverage field
///
/// The `unknown` bucket counts records that pre-date the fidelity field on
/// `TurnRecord` (older ledger writers, foreign sources). Treat them as
/// best-effort full fidelity for backward compatibility, but expose the count
/// so callers can show the gap if they care.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FidelitySummary {
    pub total: u64,
    pub by_class: HashMap<FidelityClass, u64>,
    pub by_granularity: HashMap<UsageGranularity, u64>,
    pub missing_coverage: HashMap<&'static str, u64>,
    /// Records with no `fidelity` field at all — emitted by older ledger
    /// writers. Counted separately so we don't pretend they're "full".
    pub unknown: u64,
}

pub fn empty_fidelity_summary() -> FidelitySummary {
    let mut by_class = HashMap::new();
    by_class.insert(FidelityClass::Full, 0);
    by_class.insert(FidelityClass::UsageOnly, 0);
    by_class.insert(FidelityClass::AggregateOnly, 0);
    by_class.insert(FidelityClass::CostOnly, 0);
    by_class.insert(FidelityClass::Partial, 0);

    let mut by_granularity = HashMap::new();
    by_granularity.insert(UsageGranularity::PerTurn, 0);
    by_granularity.insert(UsageGranularity::PerMessage, 0);
    by_granularity.insert(UsageGranularity::PerSessionAggregate, 0);
    by_granularity.insert(UsageGranularity::CostOnly, 0);

    let mut missing_coverage: HashMap<&'static str, u64> = HashMap::new();
    for field in COVERAGE_FIELDS {
        missing_coverage.insert(*field, 0);
    }

    FidelitySummary {
        total: 0,
        by_class,
        by_granularity,
        missing_coverage,
        unknown: 0,
    }
}

/// Walk a slice of turn records and emit a [`FidelitySummary`]. Pure
/// aggregation — no I/O, no caching, safe to call repeatedly. The input
/// iterator yields `Option<&Fidelity>` so callers can pass either real
/// `TurnRecord`s (via [`summarize_fidelity`]) or projected pairs.
pub fn summarize_fidelity_from_iter<'a, I>(fidelities: I) -> FidelitySummary
where
    I: IntoIterator<Item = Option<&'a Fidelity>>,
{
    let mut out = empty_fidelity_summary();
    for f in fidelities {
        out.total += 1;
        let Some(f) = f else {
            out.unknown += 1;
            continue;
        };
        // Trust whatever `class` was written, but re-derive when granularity
        // + coverage say something different — the older serializer might be
        // lying. (In Rust the field is non-optional so `class` is always
        // present; we re-derive defensively to match the TS semantic.)
        let cls = classify_fidelity(f.granularity, &f.coverage);
        // Prefer the recorded class if it matches the re-derivation; if they
        // diverge, use the recorded one (TS uses `f.class ?? classify(...)`,
        // i.e. recorded wins). Either way we increment exactly once.
        let cls = if f.class == cls { cls } else { f.class };
        *out.by_class.entry(cls).or_insert(0) += 1;
        *out.by_granularity.entry(f.granularity).or_insert(0) += 1;
        // Count records *missing* each field (so a non-zero number always
        // means "this many turns lack X").
        for field in COVERAGE_FIELDS {
            if !coverage_get(&f.coverage, field) {
                *out.missing_coverage.entry(*field).or_insert(0) += 1;
            }
        }
    }
    out
}

/// Convenience wrapper over [`summarize_fidelity_from_iter`] that takes a
/// slice of [`TurnRecord`]s.
pub fn summarize_fidelity(turns: &[TurnRecord]) -> FidelitySummary {
    summarize_fidelity_from_iter(turns.iter().map(|t| t.fidelity.as_ref()))
}

const FIDELITY_ORDER: &[FidelityClass] = &[
    FidelityClass::CostOnly,
    FidelityClass::AggregateOnly,
    FidelityClass::Partial,
    FidelityClass::UsageOnly,
    FidelityClass::Full,
];

fn rank(class: FidelityClass) -> usize {
    FIDELITY_ORDER
        .iter()
        .position(|c| *c == class)
        .expect("FidelityClass must be ranked")
}

/// Convenience predicate for the "default exclude aggregate-only / cost-only"
/// filtering pattern used by `burn compare` and friends. Records without
/// fidelity are treated as best-effort full (older ledger writers) — a strict
/// mode that drops unknown is a separate decision the caller can layer on top.
pub fn has_minimum_fidelity(fidelity: Option<&Fidelity>, minimum: FidelityClass) -> bool {
    let Some(f) = fidelity else {
        return true;
    };
    rank(f.class) >= rank(minimum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::opencode::{parse_opencode_session, ParseOpencodeOptions};
    use crate::reader::{Coverage, Fidelity, FidelityClass, UsageGranularity};
    use std::path::PathBuf;

    fn full_fidelity() -> Fidelity {
        Fidelity::new(
            UsageGranularity::PerTurn,
            Coverage {
                has_input_tokens: true,
                has_output_tokens: true,
                has_cache_read_tokens: true,
                has_tool_calls: true,
                has_tool_result_events: true,
                has_session_relationships: true,
                ..Coverage::EMPTY
            },
        )
    }

    fn partial_fidelity() -> Fidelity {
        // missing output / cache-read / tool-result events → "partial"
        Fidelity::new(
            UsageGranularity::PerTurn,
            Coverage {
                has_input_tokens: true,
                ..Coverage::EMPTY
            },
        )
    }

    fn aggregate_fidelity() -> Fidelity {
        Fidelity::new(
            UsageGranularity::PerSessionAggregate,
            Coverage {
                has_input_tokens: true,
                has_output_tokens: true,
                ..Coverage::EMPTY
            },
        )
    }

    #[test]
    fn returns_empty_summary_for_empty_turn_list() {
        let s = summarize_fidelity_from_iter(std::iter::empty::<Option<&Fidelity>>());
        assert_eq!(s, empty_fidelity_summary());
    }

    #[test]
    fn counts_each_turn_into_by_class_and_by_granularity() {
        let full = full_fidelity();
        let partial = partial_fidelity();
        let agg = aggregate_fidelity();
        let s = summarize_fidelity_from_iter(vec![
            Some(&full),
            Some(&full),
            Some(&partial),
            Some(&agg),
        ]);
        assert_eq!(s.total, 4);
        assert_eq!(s.by_class[&FidelityClass::Full], 2);
        assert_eq!(s.by_class[&FidelityClass::Partial], 1);
        assert_eq!(s.by_class[&FidelityClass::AggregateOnly], 1);
        assert_eq!(s.by_granularity[&UsageGranularity::PerTurn], 3);
        assert_eq!(s.by_granularity[&UsageGranularity::PerSessionAggregate], 1);
        assert_eq!(s.unknown, 0);
    }

    #[test]
    fn reports_records_without_fidelity_in_unknown_bucket() {
        let full = full_fidelity();
        let s = summarize_fidelity_from_iter(vec![None, Some(&full), None]);
        assert_eq!(s.total, 3);
        assert_eq!(s.unknown, 2);
        assert_eq!(s.by_class[&FidelityClass::Full], 1);
        // Unknown records do not get classified or counted as missing —
        // they're an explicit "we don't know" rather than "we know it's
        // incomplete".
        assert_eq!(s.missing_coverage["hasOutputTokens"], 0);
    }

    #[test]
    fn counts_missing_fields_correctly_across_mixed_fidelity() {
        let full = full_fidelity();
        let partial = partial_fidelity();
        let s = summarize_fidelity_from_iter(vec![Some(&full), Some(&partial)]);
        // FULL has input+output+cacheRead+toolCalls+toolResultEvents+
        // sessionRelationships; PARTIAL has only input. So:
        //   hasOutputTokens missing on 1 turn (PARTIAL)
        //   hasCacheReadTokens missing on 1 turn (PARTIAL)
        //   hasToolResultEvents missing on 1 turn (PARTIAL)
        //   hasSessionRelationships missing on 1 turn (PARTIAL)
        assert_eq!(s.missing_coverage["hasInputTokens"], 0);
        assert_eq!(s.missing_coverage["hasOutputTokens"], 1);
        assert_eq!(s.missing_coverage["hasCacheReadTokens"], 1);
        assert_eq!(s.missing_coverage["hasToolResultEvents"], 1);
        assert_eq!(s.missing_coverage["hasSessionRelationships"], 1);
        // hasReasoningTokens missing on both (no source surfaces it in
        // these synthetic fixtures).
        assert_eq!(s.missing_coverage["hasReasoningTokens"], 2);
    }

    #[test]
    fn over_an_opencode_session_unknown_is_zero() {
        // Conformance with `fidelity.test.ts`'s OpenCode regression: every
        // turn produced by the parser carries fidelity, so `unknown` is 0.
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p.push("tests/fixtures/opencode/multi-turn/storage/session/global/ses_multi.json");
        let result = parse_opencode_session(&p, &ParseOpencodeOptions::default())
            .expect("opencode fixture parses");
        assert!(!result.turns.is_empty());
        let summary = summarize_fidelity(&result.turns);
        assert_eq!(summary.unknown, 0);
        assert_eq!(summary.total as usize, result.turns.len());
        // Every OpenCode turn carries per-turn granularity.
        assert_eq!(
            summary.by_granularity[&UsageGranularity::PerTurn] as usize,
            result.turns.len(),
        );
    }

    #[test]
    fn has_minimum_fidelity_treats_undefined_as_passing() {
        assert!(has_minimum_fidelity(None, FidelityClass::Full));
        assert!(has_minimum_fidelity(None, FidelityClass::UsageOnly));
    }

    #[test]
    fn has_minimum_fidelity_orders_classes_from_cost_only_up_to_full() {
        let full = full_fidelity();
        let partial = partial_fidelity();
        let agg = aggregate_fidelity();
        assert!(has_minimum_fidelity(Some(&full), FidelityClass::UsageOnly));
        assert!(has_minimum_fidelity(Some(&full), FidelityClass::Full));
        assert!(!has_minimum_fidelity(
            Some(&partial),
            FidelityClass::UsageOnly
        ));
        assert!(!has_minimum_fidelity(Some(&partial), FidelityClass::Full));
        assert!(has_minimum_fidelity(
            Some(&agg),
            FidelityClass::AggregateOnly
        ));
        assert!(!has_minimum_fidelity(Some(&agg), FidelityClass::UsageOnly));
    }
}
