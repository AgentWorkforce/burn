//! Per-`(model, activity)` cost rollup — Rust port of
//! `packages/analyze/src/compare.ts`.
//!
//! `build_compare_table` aggregates a slice of [`EnrichedTurn`]s into a
//! deterministic [`CompareTable`] keyed by `(model, category)`. Cells carry
//! the same metrics the TS path emits: turn / edit-turn / one-shot counts,
//! priced-turn count, total cost, mean cost, one-shot rate, cache-hit rate,
//! median retries, plus the mutually exclusive `no_data` / `insufficient_sample`
//! flags so JSON consumers can tell "we never saw this combination" apart
//! from "we have data but the sample is small."

use std::collections::BTreeMap;

use crate::ledger::EnrichedTurn;
use crate::reader::ActivityCategory;

use crate::analyze::cost::cost_for_turn;
use crate::analyze::pricing::PricingTable;

/// Activity category label, or `"unclassified"` for turns the classifier
/// couldn't bucket. Mirrors the TS `ActivityCategory | "unclassified"`
/// union. Exposed as a [`String`] because callers consume it as a key into
/// [`CompareTable::cells`].
pub type CompareCategory = String;

/// Default minimum sample count below which a non-empty cell is flagged as
/// `insufficient_sample`. Matches the TS `DEFAULT_MIN_SAMPLE`.
pub const DEFAULT_MIN_SAMPLE: u64 = 5;

const UNCLASSIFIED: &str = "unclassified";

#[derive(Debug, Clone, PartialEq)]
pub struct CompareCell {
    pub turns: u64,
    pub edit_turns: u64,
    pub one_shot_turns: u64,
    /// Number of turns whose model had pricing in the active table. When
    /// this is less than `turns`, `total_cost` / `cost_per_turn`
    /// under-count what the cell actually consumed.
    pub priced_turns: u64,
    pub total_cost: f64,
    /// `None` when no priced turns; cells with unpriced models render as
    /// "—", never as "$0.00".
    pub cost_per_turn: Option<f64>,
    /// `None` for categories with no edits and for empty cells.
    pub one_shot_rate: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub median_retries: Option<f64>,
    /// True when the cell has zero turns. Distinct from `insufficient_sample`
    /// so JSON consumers can tell "we never saw this combination" apart
    /// from "we have data but the sample is small." Only one of `no_data` /
    /// `insufficient_sample` is ever true at a time.
    pub no_data: bool,
    /// True when `0 < turns < min_sample`. A cell with `no_data == true`
    /// always has `insufficient_sample == false`.
    pub insufficient_sample: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompareTotals {
    pub turns: u64,
    pub total_cost: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompareTable {
    pub models: Vec<String>,
    pub categories: Vec<String>,
    pub cells: BTreeMap<String, BTreeMap<String, CompareCell>>,
    pub totals: BTreeMap<String, CompareTotals>,
    pub min_sample: u64,
}

#[derive(Debug, Clone)]
pub struct CompareOptions<'a> {
    pub pricing: &'a PricingTable,
    /// Optional explicit model allow-list. When set, turns whose model is
    /// not in this list are dropped, and any listed model that produced
    /// zero matching turns is still rendered as an all-empty column.
    pub models: Option<Vec<String>>,
    pub min_sample: Option<u64>,
}

impl<'a> CompareOptions<'a> {
    pub fn new(pricing: &'a PricingTable) -> Self {
        Self {
            pricing,
            models: None,
            min_sample: None,
        }
    }
}

#[derive(Debug, Default)]
struct Accum {
    turns: u64,
    edit_turns: u64,
    one_shot_turns: u64,
    priced_turns: u64,
    total_cost: f64,
    retries_samples: Vec<u64>,
    cache_read: u64,
    token_denominator: u64,
}

pub fn build_compare_table(turns: &[EnrichedTurn], opts: &CompareOptions<'_>) -> CompareTable {
    let min_sample = opts.min_sample.unwrap_or(DEFAULT_MIN_SAMPLE);
    let model_filter: Option<Vec<String>> = opts.models.as_ref().filter(|v| !v.is_empty()).cloned();

    let mut by_model_category: BTreeMap<String, BTreeMap<String, Accum>> = BTreeMap::new();
    let mut model_totals: BTreeMap<String, CompareTotals> = BTreeMap::new();
    let mut model_set: BTreeMap<String, ()> = BTreeMap::new();
    let mut category_set: BTreeMap<String, ()> = BTreeMap::new();

    // Pre-seed model_set from the model filter so a model the user
    // explicitly asked about stays visible (as an all-empty column with
    // coverage notes) even if zero turns matched. Without this, the "no
    // <model> data" coverage signal silently disappears for filtered-but-
    // absent models.
    if let Some(ref filter) = model_filter {
        for m in filter {
            model_set.insert(m.clone(), ());
            model_totals.insert(m.clone(), CompareTotals::default());
        }
    }

    for et in turns {
        let t = &et.turn;
        let model = if t.model.is_empty() {
            "unknown".to_string()
        } else {
            t.model.clone()
        };
        if let Some(ref filter) = model_filter {
            if !filter.iter().any(|m| m == &model) {
                continue;
            }
        }
        let cat = activity_label(t.activity);
        model_set.insert(model.clone(), ());
        category_set.insert(cat.clone(), ());

        let by_cat = by_model_category.entry(model.clone()).or_default();
        let acc = by_cat.entry(cat).or_default();

        acc.turns += 1;
        let mt = model_totals.entry(model.clone()).or_default();
        mt.turns += 1;
        if let Some(c) = cost_for_turn(t, opts.pricing) {
            acc.priced_turns += 1;
            acc.total_cost += c.total;
            mt.total_cost += c.total;
        }
        if t.has_edits.unwrap_or(false) {
            acc.edit_turns += 1;
            let r = t.retries.unwrap_or(0);
            acc.retries_samples.push(r);
            if r == 0 {
                acc.one_shot_turns += 1;
            }
        }
        acc.cache_read += t.usage.cache_read;
        acc.token_denominator +=
            t.usage.input + t.usage.cache_read + t.usage.cache_create_5m + t.usage.cache_create_1h;
    }

    // Sort models by total cost DESC, then ASCII-lex on the id. The TS path
    // uses `localeCompare` here; for the model-id corpus that is in practice
    // ASCII alphanumeric+`-`/`/`, the two orderings agree.
    let mut models: Vec<String> = model_set.keys().cloned().collect();
    models.sort_by(|a, b| {
        let ca = model_totals.get(a).map(|v| v.total_cost).unwrap_or(0.0);
        let cb = model_totals.get(b).map(|v| v.total_cost).unwrap_or(0.0);
        match cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal) {
            std::cmp::Ordering::Equal => a.cmp(b),
            other => other,
        }
    });

    // Sort categories by total turns across all (already-sorted) models DESC,
    // then by label ASC. Mirrors the TS path so the on-the-wire row ordering
    // is byte-identical for the same fixture.
    let mut categories: Vec<String> = category_set.keys().cloned().collect();
    categories.sort_by(|a, b| {
        let ta: u64 = models
            .iter()
            .map(|m| {
                by_model_category
                    .get(m)
                    .and_then(|by_cat| by_cat.get(a))
                    .map(|acc| acc.turns)
                    .unwrap_or(0)
            })
            .sum();
        let tb: u64 = models
            .iter()
            .map(|m| {
                by_model_category
                    .get(m)
                    .and_then(|by_cat| by_cat.get(b))
                    .map(|acc| acc.turns)
                    .unwrap_or(0)
            })
            .sum();
        match tb.cmp(&ta) {
            std::cmp::Ordering::Equal => a.cmp(b),
            other => other,
        }
    });

    let mut cells: BTreeMap<String, BTreeMap<String, CompareCell>> = BTreeMap::new();
    for m in &models {
        let mut row: BTreeMap<String, CompareCell> = BTreeMap::new();
        for cat in &categories {
            let acc = by_model_category.get(m).and_then(|by_cat| by_cat.get(cat));
            row.insert(cat.clone(), to_cell(acc, min_sample));
        }
        cells.insert(m.clone(), row);
    }

    CompareTable {
        models,
        categories,
        cells,
        totals: model_totals,
        min_sample,
    }
}

fn to_cell(acc: Option<&Accum>, min_sample: u64) -> CompareCell {
    let Some(acc) = acc else {
        return empty_cell();
    };
    if acc.turns == 0 {
        return empty_cell();
    }
    CompareCell {
        turns: acc.turns,
        edit_turns: acc.edit_turns,
        one_shot_turns: acc.one_shot_turns,
        priced_turns: acc.priced_turns,
        total_cost: acc.total_cost,
        // `cost_per_turn` is `None` when none of the turns in this cell
        // have pricing — emitting 0 would silently misrepresent unknown
        // cost as free.
        cost_per_turn: if acc.priced_turns > 0 {
            Some(acc.total_cost / acc.priced_turns as f64)
        } else {
            None
        },
        one_shot_rate: if acc.edit_turns > 0 {
            Some(acc.one_shot_turns as f64 / acc.edit_turns as f64)
        } else {
            None
        },
        cache_hit_rate: if acc.token_denominator > 0 {
            Some(acc.cache_read as f64 / acc.token_denominator as f64)
        } else {
            None
        },
        median_retries: if acc.edit_turns > 0 {
            Some(median(&acc.retries_samples))
        } else {
            None
        },
        no_data: false,
        insufficient_sample: acc.turns < min_sample,
    }
}

fn empty_cell() -> CompareCell {
    CompareCell {
        turns: 0,
        edit_turns: 0,
        one_shot_turns: 0,
        priced_turns: 0,
        total_cost: 0.0,
        cost_per_turn: None,
        one_shot_rate: None,
        cache_hit_rate: None,
        median_retries: None,
        no_data: true,
        insufficient_sample: false,
    }
}

fn median(xs: &[u64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut s: Vec<u64> = xs.to_vec();
    s.sort_unstable();
    let mid = s.len() / 2;
    if s.len().is_multiple_of(2) {
        (s[mid - 1] as f64 + s[mid] as f64) / 2.0
    } else {
        s[mid] as f64
    }
}

fn activity_label(activity: Option<ActivityCategory>) -> String {
    match activity {
        Some(a) => match serde_json::to_value(a) {
            Ok(serde_json::Value::String(s)) => s,
            _ => UNCLASSIFIED.to_string(),
        },
        None => UNCLASSIFIED.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::ledger::EnrichedTurn;
    use crate::reader::{ActivityCategory, SourceKind, ToolCall, TurnRecord, Usage};
    use std::collections::BTreeMap;

    fn turn(
        model: &str,
        activity: Option<ActivityCategory>,
        usage: Usage,
        has_edits: Option<bool>,
        retries: Option<u64>,
    ) -> EnrichedTurn {
        let id = format!("m-{}", rand_suffix());
        EnrichedTurn {
            turn: TurnRecord {
                v: 1,
                source: SourceKind::ClaudeCode,
                session_id: "s".into(),
                session_path: None,
                message_id: id,
                turn_index: 0,
                ts: "2026-04-20T00:00:00.000Z".into(),
                model: model.into(),
                project: None,
                project_key: None,
                usage,
                tool_calls: Vec::<ToolCall>::new(),
                files_touched: None,
                subagent: None,
                stop_reason: None,
                activity,
                retries,
                has_edits,
                fidelity: None,
            },
            enrichment: BTreeMap::new(),
        }
    }

    fn rand_suffix() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed).to_string()
    }

    fn default_usage() -> Usage {
        Usage {
            input: 1000,
            output: 500,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    #[test]
    fn buckets_turns_by_model_and_activity_with_per_cell_metrics() {
        let pricing = load_builtin_pricing();
        let mut turns: Vec<EnrichedTurn> = Vec::new();
        // 6 Sonnet coding turns, 4 one-shot.
        for _ in 0..4 {
            turns.push(turn(
                "claude-sonnet-4-6",
                Some(ActivityCategory::Coding),
                default_usage(),
                Some(true),
                Some(0),
            ));
        }
        turns.push(turn(
            "claude-sonnet-4-6",
            Some(ActivityCategory::Coding),
            default_usage(),
            Some(true),
            Some(2),
        ));
        turns.push(turn(
            "claude-sonnet-4-6",
            Some(ActivityCategory::Coding),
            default_usage(),
            Some(true),
            Some(1),
        ));
        // 5 Haiku coding turns, 2 one-shot.
        for r in [0u64, 0, 1, 2, 1] {
            turns.push(turn(
                "claude-haiku-4-5",
                Some(ActivityCategory::Coding),
                default_usage(),
                Some(true),
                Some(r),
            ));
        }
        // Sonnet exploration, no edits — Haiku exploration cell stays no-data.
        turns.push(turn(
            "claude-sonnet-4-6",
            Some(ActivityCategory::Exploration),
            default_usage(),
            Some(false),
            None,
        ));
        turns.push(turn(
            "claude-sonnet-4-6",
            Some(ActivityCategory::Exploration),
            default_usage(),
            Some(false),
            None,
        ));

        let opts = CompareOptions::new(&pricing);
        let t = build_compare_table(&turns, &opts);
        let mut sorted = t.models.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["claude-haiku-4-5", "claude-sonnet-4-6"]);
        assert!(t.categories.iter().any(|c| c == "coding"));
        assert!(t.categories.iter().any(|c| c == "exploration"));

        let sonnet_coding = &t.cells["claude-sonnet-4-6"]["coding"];
        assert_eq!(sonnet_coding.turns, 6);
        assert_eq!(sonnet_coding.edit_turns, 6);
        assert_eq!(sonnet_coding.one_shot_turns, 4);
        assert_eq!(sonnet_coding.one_shot_rate, Some(4.0 / 6.0));

        let haiku_coding = &t.cells["claude-haiku-4-5"]["coding"];
        assert_eq!(haiku_coding.turns, 5);
        assert_eq!(haiku_coding.one_shot_rate, Some(2.0 / 5.0));

        let haiku_exploration = &t.cells["claude-haiku-4-5"]["exploration"];
        assert_eq!(haiku_exploration.turns, 0);
        assert!(haiku_exploration.no_data);
        assert!(!haiku_exploration.insufficient_sample);
        assert_eq!(haiku_exploration.cost_per_turn, None);
        assert_eq!(haiku_exploration.one_shot_rate, None);
    }

    #[test]
    fn returns_none_one_shot_rate_for_categories_with_no_edit_turns() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            turn(
                "claude-sonnet-4-6",
                Some(ActivityCategory::Exploration),
                default_usage(),
                Some(false),
                None,
            ),
            turn(
                "claude-sonnet-4-6",
                Some(ActivityCategory::Exploration),
                default_usage(),
                Some(false),
                None,
            ),
        ];
        let t = build_compare_table(&turns, &CompareOptions::new(&pricing));
        let cell = &t.cells["claude-sonnet-4-6"]["exploration"];
        assert_eq!(cell.turns, 2);
        assert_eq!(cell.edit_turns, 0);
        assert_eq!(cell.one_shot_rate, None);
        assert_eq!(cell.median_retries, None);
    }

    #[test]
    fn flags_low_sample_cells_as_insufficient_not_no_data() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            turn(
                "claude-sonnet-4-6",
                Some(ActivityCategory::Refactoring),
                default_usage(),
                Some(true),
                Some(0),
            ),
            turn(
                "claude-sonnet-4-6",
                Some(ActivityCategory::Refactoring),
                default_usage(),
                Some(true),
                Some(0),
            ),
        ];
        let opts = CompareOptions {
            pricing: &pricing,
            models: None,
            min_sample: Some(5),
        };
        let t = build_compare_table(&turns, &opts);
        let cell = &t.cells["claude-sonnet-4-6"]["refactoring"];
        assert!(cell.insufficient_sample);
        assert!(!cell.no_data);
        assert_eq!(cell.turns, 2);
    }

    #[test]
    fn applies_models_filter() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            turn(
                "claude-sonnet-4-6",
                Some(ActivityCategory::Coding),
                default_usage(),
                Some(true),
                Some(0),
            ),
            turn(
                "claude-haiku-4-5",
                Some(ActivityCategory::Coding),
                default_usage(),
                Some(true),
                Some(0),
            ),
            turn(
                "claude-opus-4-7",
                Some(ActivityCategory::Coding),
                default_usage(),
                Some(true),
                Some(0),
            ),
        ];
        let opts = CompareOptions {
            pricing: &pricing,
            models: Some(vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()]),
            min_sample: None,
        };
        let t = build_compare_table(&turns, &opts);
        let mut sorted = t.models.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["claude-haiku-4-5", "claude-sonnet-4-6"]);
    }

    #[test]
    fn keeps_explicitly_requested_models_visible_with_zero_turns() {
        let pricing = load_builtin_pricing();
        let turns = vec![turn(
            "claude-sonnet-4-6",
            Some(ActivityCategory::Coding),
            default_usage(),
            Some(true),
            Some(0),
        )];
        let opts = CompareOptions {
            pricing: &pricing,
            models: Some(vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()]),
            min_sample: None,
        };
        let t = build_compare_table(&turns, &opts);
        assert!(
            t.models.iter().any(|m| m == "claude-haiku-4-5"),
            "requested model must remain in the table"
        );
        let haiku_cell = &t.cells["claude-haiku-4-5"]["coding"];
        assert!(haiku_cell.no_data);
        assert_eq!(haiku_cell.turns, 0);
        assert_eq!(t.totals["claude-haiku-4-5"].turns, 0);
    }

    #[test]
    fn renders_cost_per_turn_as_none_when_no_priced_turn() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            turn(
                "definitely-not-a-model",
                Some(ActivityCategory::Coding),
                default_usage(),
                Some(true),
                Some(0),
            ),
            turn(
                "definitely-not-a-model",
                Some(ActivityCategory::Coding),
                default_usage(),
                Some(true),
                Some(1),
            ),
        ];
        let t = build_compare_table(&turns, &CompareOptions::new(&pricing));
        let cell = &t.cells["definitely-not-a-model"]["coding"];
        assert_eq!(cell.turns, 2);
        assert_eq!(cell.priced_turns, 0);
        assert_eq!(cell.total_cost, 0.0);
        assert_eq!(cell.cost_per_turn, None);
    }

    #[test]
    fn uses_priced_turns_as_cost_per_turn_denominator() {
        let pricing = load_builtin_pricing();
        let usage = Usage {
            input: 1_000_000,
            output: 0,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        };
        let turns = vec![
            turn(
                "claude-sonnet-4-6",
                Some(ActivityCategory::Coding),
                usage.clone(),
                Some(true),
                Some(0),
            ),
            turn(
                "claude-sonnet-4-6",
                Some(ActivityCategory::Coding),
                usage,
                Some(true),
                Some(0),
            ),
        ];
        let t = build_compare_table(&turns, &CompareOptions::new(&pricing));
        let cell = &t.cells["claude-sonnet-4-6"]["coding"];
        assert_eq!(cell.priced_turns, 2);
        assert_eq!(cell.turns, 2);
        let cpt = cell.cost_per_turn.expect("priced");
        assert!((cpt - cell.total_cost / 2.0).abs() < 1e-9);
    }

    #[test]
    fn groups_unclassified_turns_under_unclassified() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            turn(
                "claude-sonnet-4-6",
                None,
                default_usage(),
                Some(false),
                None,
            ),
            turn(
                "claude-sonnet-4-6",
                None,
                default_usage(),
                Some(false),
                None,
            ),
        ];
        let t = build_compare_table(&turns, &CompareOptions::new(&pricing));
        assert!(t.categories.iter().any(|c| c == "unclassified"));
        assert_eq!(t.cells["claude-sonnet-4-6"]["unclassified"].turns, 2);
    }

    #[test]
    fn total_cost_per_model_matches_sum_of_its_cells() {
        let pricing = load_builtin_pricing();
        let turns = vec![
            turn(
                "claude-sonnet-4-6",
                Some(ActivityCategory::Coding),
                Usage {
                    input: 500_000,
                    output: 100_000,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                Some(true),
                Some(0),
            ),
            turn(
                "claude-sonnet-4-6",
                Some(ActivityCategory::Debugging),
                Usage {
                    input: 1_000_000,
                    output: 100_000,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                Some(true),
                Some(1),
            ),
        ];
        let t = build_compare_table(&turns, &CompareOptions::new(&pricing));
        let sum = t.cells["claude-sonnet-4-6"]["coding"].total_cost
            + t.cells["claude-sonnet-4-6"]["debugging"].total_cost;
        assert!((sum - t.totals["claude-sonnet-4-6"].total_cost).abs() < 1e-9);
    }

    #[test]
    fn computes_cache_hit_rate_across_input_and_cache_buckets() {
        let pricing = load_builtin_pricing();
        let turns = vec![turn(
            "claude-sonnet-4-6",
            Some(ActivityCategory::Coding),
            Usage {
                input: 1000,
                output: 200,
                reasoning: 0,
                cache_read: 3000,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            Some(true),
            Some(0),
        )];
        let t = build_compare_table(&turns, &CompareOptions::new(&pricing));
        let cell = &t.cells["claude-sonnet-4-6"]["coding"];
        let rate = cell.cache_hit_rate.expect("rate");
        assert!((rate - 3000.0 / 4000.0).abs() < 1e-9);
    }
}
