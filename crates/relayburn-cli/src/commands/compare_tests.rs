use super::*;

#[test]
fn format_pct_rounds_to_int() {
    assert_eq!(format_pct(0.0), "0%");
    assert_eq!(format_pct(0.5), "50%");
    assert_eq!(format_pct(1.0), "100%");
    assert_eq!(format_pct(2.0 / 3.0), "67%");
}

#[test]
fn round_json_matches_js_to_fixed() {
    // Whole numbers come out as integers (no `.0` suffix).
    let v = round_json(1.0, 4);
    assert_eq!(v.to_string(), "1");
    // Non-whole shorter than digit cap drops trailing zeros.
    let v = round_json(0.5, 4);
    assert_eq!(v.to_string(), "0.5");
    // Rounds to 6 digits.
    let v = round_json(0.0112499999, 6);
    assert_eq!(v.to_string(), "0.01125");
}

#[test]
fn parse_provider_filter_trims_lowercases_and_drops_empties() {
    // The CLI parser trims / lowercases / drops empties; deduping is left
    // to the verb's `normalize_provider_filter`, so the raw entries
    // (including the repeat) flow through as a `Vec`.
    let got = parse_provider_filter(Some(" Anthropic,OPENAI ,, anthropic"))
        .unwrap()
        .unwrap();
    assert_eq!(got, vec!["anthropic", "openai", "anthropic"]);
}

#[test]
fn parse_provider_filter_returns_none_when_flag_absent() {
    assert!(parse_provider_filter(None).unwrap().is_none());
}

#[test]
fn parse_provider_filter_rejects_all_empty_input() {
    let err = parse_provider_filter(Some(" , ,, ")).unwrap_err();
    assert!(format!("{err}").contains("--provider requires a value"));
}

#[test]
fn parse_fidelity_known_classes() {
    assert!(matches!(
        parse_fidelity("full").unwrap(),
        FidelityClass::Full
    ));
    assert!(matches!(
        parse_fidelity("usage-only").unwrap(),
        FidelityClass::UsageOnly
    ));
    assert!(parse_fidelity("nope").is_err());
}

#[test]
fn display_model_name_strips_provider_prefix() {
    assert_eq!(
        display_model_name("anthropic/claude-sonnet-4-6"),
        "claude-sonnet-4-6"
    );
    assert_eq!(display_model_name("claude-haiku-4-5"), "claude-haiku-4-5");
}

#[test]
fn format_model_total_cost_marks_unpriced_instead_of_zero() {
    assert_eq!(format_model_total_cost(0, 0.0, 0), "— total");
    assert_eq!(format_model_total_cost(4, 1.25, 0), "$1.25 total");
    assert_eq!(format_model_total_cost(3, 0.0, 3), "unpriced");
    assert_eq!(
        format_model_total_cost(5, 1.25, 2),
        "$1.25 total (2 unpriced)"
    );
}

fn sample_compare_result() -> CompareResult {
    use relayburn_sdk::CompareFidelityBlock;
    use relayburn_sdk::CompareModelTotal;
    use std::collections::BTreeMap;

    let mut totals = BTreeMap::new();
    totals.insert(
        "claude-sonnet-4-6".into(),
        CompareModelTotal {
            turns: 2,
            total_cost: 1.25,
        },
    );
    totals.insert(
        "future-model".into(),
        CompareModelTotal {
            turns: 3,
            total_cost: 0.0,
        },
    );
    totals.insert(
        "mixed-model".into(),
        CompareModelTotal {
            turns: 4,
            total_cost: 0.50,
        },
    );

    CompareResult {
        analyzed_turns: 9,
        min_sample: 5,
        models: vec![
            "claude-sonnet-4-6".into(),
            "future-model".into(),
            "mixed-model".into(),
        ],
        categories: vec!["coding".into()],
        totals,
        cells: vec![
            CompareCellResult {
                model: "claude-sonnet-4-6".into(),
                category: "coding".into(),
                turns: 2,
                edit_turns: 0,
                one_shot_turns: 0,
                priced_turns: 2,
                total_cost: 1.25,
                cost_per_turn: Some(0.625),
                one_shot_rate: None,
                cache_hit_rate: None,
                median_retries: None,
                no_data: false,
                insufficient_sample: true,
            },
            CompareCellResult {
                model: "future-model".into(),
                category: "coding".into(),
                turns: 3,
                edit_turns: 0,
                one_shot_turns: 0,
                priced_turns: 0,
                total_cost: 0.0,
                cost_per_turn: None,
                one_shot_rate: None,
                cache_hit_rate: None,
                median_retries: None,
                no_data: false,
                insufficient_sample: true,
            },
            CompareCellResult {
                model: "mixed-model".into(),
                category: "coding".into(),
                turns: 4,
                edit_turns: 0,
                one_shot_turns: 0,
                priced_turns: 3,
                total_cost: 0.50,
                cost_per_turn: Some(0.50 / 3.0),
                one_shot_rate: None,
                cache_hit_rate: None,
                median_retries: None,
                no_data: false,
                insufficient_sample: true,
            },
        ],
        fidelity: CompareFidelityBlock {
            minimum: FidelityClass::UsageOnly,
            excluded: CompareExcludedBreakdown {
                total: 0,
                aggregate_only: 0,
                cost_only: 0,
                partial: 0,
                usage_only: 0,
            },
            summary: FidelitySummary {
                total: 0,
                by_class: BTreeMap::new(),
                by_granularity: BTreeMap::new(),
                missing_coverage: BTreeMap::new(),
                unknown: 0,
            },
        },
    }
}

#[test]
fn render_tty_uses_cell_priced_turns_for_model_totals() {
    let tty = render_tty(&sample_compare_result());
    assert!(
        tty.contains("claude-sonnet-4-6: 2 turns, $1.25 total"),
        "{tty}"
    );
    assert!(tty.contains("future-model: 3 turns, unpriced"), "{tty}");
    assert!(
        tty.contains("mixed-model: 4 turns, $0.500 total (1 unpriced)"),
        "{tty}"
    );
    assert!(
        !tty.contains("future-model: 3 turns, $0.00 total"),
        "unpriced model must not look free:\n{tty}"
    );
}

#[test]
fn unpriced_compare_totals_follow_model_order() {
    let (turns, models) = unpriced_compare_totals(&sample_compare_result());
    assert_eq!(turns, 4);
    assert_eq!(models, vec!["future-model", "mixed-model"]);
}

fn excluded(
    total: u64,
    aggregate_only: u64,
    cost_only: u64,
    partial: u64,
    usage_only: u64,
) -> CompareExcludedBreakdown {
    CompareExcludedBreakdown {
        total,
        aggregate_only,
        cost_only,
        partial,
        usage_only,
    }
}

#[test]
fn format_excluded_note_lists_nonzero_buckets_in_order() {
    assert_eq!(
        format_excluded_note(&excluded(1_234, 1, 2, 3, 4), FidelityClass::Full),
        "excluded 1,234 turns below full fidelity (1 aggregate-only, 2 cost-only, 3 partial, 4 usage-only)"
    );
    assert_eq!(
        format_excluded_note(&excluded(5, 0, 5, 0, 0), FidelityClass::UsageOnly),
        "excluded 5 turns below usage-only fidelity (5 cost-only)"
    );
    assert_eq!(
        format_excluded_note(&excluded(2, 0, 0, 2, 0), FidelityClass::Full),
        "excluded 2 turns below full fidelity (2 partial)"
    );
    assert_eq!(
        format_excluded_note(&excluded(3, 3, 0, 0, 0), FidelityClass::Full),
        "excluded 3 turns below full fidelity (3 aggregate-only)"
    );
    assert_eq!(
        format_excluded_note(&excluded(4, 0, 0, 0, 4), FidelityClass::Full),
        "excluded 4 turns below full fidelity (4 usage-only)"
    );
}

#[test]
fn format_excluded_note_singular_and_no_breakdown() {
    assert_eq!(
        format_excluded_note(&excluded(1, 0, 0, 0, 0), FidelityClass::Full),
        "excluded 1 turn below full fidelity"
    );
    assert_eq!(
        format_excluded_note(&excluded(0, 0, 0, 0, 0), FidelityClass::Full),
        "excluded 0 turns below full fidelity"
    );
}

#[test]
fn fidelity_summary_to_value_fills_every_key_in_fixed_order() {
    use std::collections::BTreeMap;
    let mut by_class = BTreeMap::new();
    by_class.insert(FidelityClass::Full, 7);
    by_class.insert(FidelityClass::Partial, 2);
    let mut by_granularity = BTreeMap::new();
    by_granularity.insert(UsageGranularity::PerMessage, 4);
    by_granularity.insert(UsageGranularity::CostOnly, 1);
    let mut missing_coverage = BTreeMap::new();
    missing_coverage.insert("hasToolCalls", 3);
    missing_coverage.insert("hasRawContent", 9);
    missing_coverage.insert("notARealKey", 99);
    let summary = FidelitySummary {
        total: 11,
        by_class,
        by_granularity,
        missing_coverage,
        unknown: 5,
    };
    let value = fidelity_summary_to_value(&summary);
    assert_eq!(
        value,
        json!({
            "total": 11,
            "byClass": {
                "full": 7, "usage-only": 0, "aggregate-only": 0, "cost-only": 0, "partial": 2
            },
            "byGranularity": {
                "per-turn": 0, "per-message": 4, "per-session-aggregate": 0, "cost-only": 1
            },
            "missingCoverage": {
                "hasInputTokens": 0, "hasOutputTokens": 0, "hasReasoningTokens": 0,
                "hasCacheReadTokens": 0, "hasCacheCreateTokens": 0, "hasToolCalls": 3,
                "hasToolResultEvents": 0, "hasSessionRelationships": 0, "hasRawContent": 9
            },
            "unknown": 5
        })
    );
    let obj = value.as_object().unwrap();
    assert_eq!(
        obj.keys().collect::<Vec<_>>(),
        [
            "total",
            "byClass",
            "byGranularity",
            "missingCoverage",
            "unknown"
        ]
    );
    assert_eq!(
        obj["byClass"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        [
            "full",
            "usage-only",
            "aggregate-only",
            "cost-only",
            "partial"
        ]
    );
    assert_eq!(
        obj["byGranularity"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        [
            "per-turn",
            "per-message",
            "per-session-aggregate",
            "cost-only"
        ]
    );
    assert_eq!(
        obj["missingCoverage"].as_object().unwrap().len(),
        9,
        "unknown coverage keys are dropped"
    );
}

#[test]
fn fidelity_summary_to_value_maps_each_granularity() {
    use std::collections::BTreeMap;
    let mut by_granularity = BTreeMap::new();
    by_granularity.insert(UsageGranularity::PerTurn, 1);
    by_granularity.insert(UsageGranularity::PerSessionAggregate, 3);
    let summary = FidelitySummary {
        total: 0,
        by_class: BTreeMap::new(),
        by_granularity,
        missing_coverage: BTreeMap::new(),
        unknown: 0,
    };
    let value = fidelity_summary_to_value(&summary);
    assert_eq!(
        value["byGranularity"],
        json!({ "per-turn": 1, "per-message": 0, "per-session-aggregate": 3, "cost-only": 0 })
    );
}

#[test]
fn build_json_emits_cells_totals_and_fidelity() {
    let mut result = sample_compare_result();
    result.categories.push("review".into());
    result.models.push("ghost-model".into());
    result.cells[0].edit_turns = 2;
    result.cells[0].one_shot_turns = 1;
    result.cells[0].one_shot_rate = Some(0.5);
    result.cells[0].cache_hit_rate = Some(0.123_456);
    result.cells[0].median_retries = Some(1.5);
    result.cells[0].total_cost = 1.234_567_89;
    result.fidelity.excluded = excluded(6, 1, 2, 0, 3);
    result.fidelity.summary.total = 9;

    let value = build_json(&result);
    assert_eq!(value["analyzedTurns"], json!(9));
    assert_eq!(value["minSample"], json!(5));
    assert_eq!(
        value["models"],
        json!([
            "claude-sonnet-4-6",
            "future-model",
            "mixed-model",
            "ghost-model"
        ])
    );
    assert_eq!(value["categories"], json!(["coding", "review"]));

    let totals = value["totals"].as_object().unwrap();
    assert_eq!(
        totals.keys().collect::<Vec<_>>(),
        [
            "claude-sonnet-4-6",
            "future-model",
            "mixed-model",
            "ghost-model"
        ]
    );
    assert_eq!(
        totals["claude-sonnet-4-6"],
        json!({ "turns": 2, "totalCost": 1.25 })
    );
    assert_eq!(
        totals["future-model"],
        json!({ "turns": 3, "totalCost": 0 })
    );
    assert_eq!(totals["ghost-model"], json!({ "turns": 0, "totalCost": 0 }));

    let cells = value["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 8);
    assert_eq!(
        cells[0],
        json!({
            "model": "claude-sonnet-4-6",
            "category": "coding",
            "turns": 2,
            "editTurns": 2,
            "oneShotTurns": 1,
            "pricedTurns": 2,
            "totalCost": 1.234568,
            "costPerTurn": 0.625,
            "oneShotRate": 0.5,
            "cacheHitRate": 0.1235,
            "medianRetries": 1.5,
            "noData": false,
            "insufficientSample": true,
        })
    );
    assert_eq!(cells[1]["model"], json!("claude-sonnet-4-6"));
    assert_eq!(cells[1]["category"], json!("review"));
    assert_eq!(cells[2]["model"], json!("future-model"));
    assert_eq!(cells[2]["costPerTurn"], Value::Null);
    assert_eq!(cells[2]["oneShotRate"], Value::Null);
    assert_eq!(cells[2]["cacheHitRate"], Value::Null);
    assert_eq!(cells[2]["medianRetries"], Value::Null);
    assert_eq!(cells[4]["costPerTurn"], json!(0.166667));
    assert_eq!(
        cells[7],
        json!({
            "model": "ghost-model",
            "category": "review",
            "turns": 0,
            "editTurns": 0,
            "oneShotTurns": 0,
            "pricedTurns": 0,
            "totalCost": 0,
            "costPerTurn": null,
            "oneShotRate": null,
            "cacheHitRate": null,
            "medianRetries": null,
            "noData": true,
            "insufficientSample": false,
        })
    );

    assert_eq!(
        value["fidelity"],
        json!({
            "minimum": "usage-only",
            "excluded": {
                "total": 6, "aggregateOnly": 1, "costOnly": 2, "partial": 0, "usageOnly": 3
            },
            "summary": fidelity_summary_to_value(&result.fidelity.summary),
        })
    );
    assert_eq!(value["fidelity"]["summary"]["total"], json!(9));
    assert_eq!(
        value.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "analyzedTurns",
            "minSample",
            "models",
            "categories",
            "totals",
            "cells",
            "fidelity"
        ]
    );
}
