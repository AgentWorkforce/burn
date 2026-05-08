//! Per-record cost derivation — Rust port of `packages/analyze/src/cost.ts`.
//!
//! The math (per-token rate × token count, with cache-read / cache-create /
//! reasoning splits) is the precision-sensitive core of analyze. We use `f64`
//! to mirror the TS `number` type and keep accumulation order identical so
//! drift stays bounded by the documented 1e-9 USD precision contract that the
//! later overhead sub-issue depends on.

use serde::{Deserialize, Serialize};

use crate::reader::{SourceKind, TurnRecord, Usage};

use crate::analyze::pricing::{ModelCost, PricingTable, ReasoningMode};
use crate::analyze::provider_reattribution::resolve_provider;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostBreakdown {
    pub model: String,
    pub total: f64,
    pub input: f64,
    pub output: f64,
    pub reasoning: f64,
    pub cache_read: f64,
    pub cache_create: f64,
}

/// Override the reasoning-billing semantics for a `cost_for_usage` call. When
/// `reasoning_mode` is `None`, the mode is taken from the resolved
/// `ModelCost`; when `Some`, it wins — used by `cost_for_turn` to force
/// `IncludedInOutput` for sources whose transcripts already fold reasoning
/// into `output_tokens` (Codex).
#[derive(Debug, Clone, Copy, Default)]
pub struct CostForUsageOptions {
    pub reasoning_mode: Option<ReasoningMode>,
}

const PER_MILLION: f64 = 1_000_000.0;

pub fn cost_for_usage(
    usage: &Usage,
    model: &str,
    pricing: &PricingTable,
    options: CostForUsageOptions,
) -> Option<CostBreakdown> {
    let rate = lookup_model_rate(model, pricing)?;
    let mode = options.reasoning_mode.unwrap_or(rate.reasoning_mode);
    let input = (usage.input as f64 / PER_MILLION) * rate.input;
    let output = (usage.output as f64 / PER_MILLION) * rate.output;
    let reasoning = reasoning_cost(usage.reasoning, rate, mode);
    let cache_read = (usage.cache_read as f64 / PER_MILLION) * rate.cache_read;
    let cache_create = ((usage.cache_create_5m as f64 + usage.cache_create_1h as f64)
        / PER_MILLION)
        * rate.cache_write;
    Some(CostBreakdown {
        model: model.to_string(),
        total: input + output + reasoning + cache_read + cache_create,
        input,
        output,
        reasoning,
        cache_read,
        cache_create,
    })
}

pub fn cost_for_turn(turn: &TurnRecord, pricing: &PricingTable) -> Option<CostBreakdown> {
    let opts = CostForUsageOptions {
        reasoning_mode: reasoning_mode_for_source(turn.source),
    };
    cost_for_usage(&turn.usage, &turn.model, pricing, opts)
}

fn reasoning_cost(reasoning_tokens: u64, rate: &ModelCost, mode: ReasoningMode) -> f64 {
    match mode {
        // Already billed inside `usage.output` — informational only.
        ReasoningMode::IncludedInOutput => 0.0,
        // Use the model's distinct reasoning tariff. If the override forced
        // this mode but the model has no `rate.reasoning`, fall back to the
        // output rate so we never silently drop reasoning tokens.
        ReasoningMode::Separate => {
            (reasoning_tokens as f64 / PER_MILLION) * rate.reasoning.unwrap_or(rate.output)
        }
        ReasoningMode::SameAsOutput => (reasoning_tokens as f64 / PER_MILLION) * rate.output,
    }
}

/// Per-source reasoning-billing semantics override. Returning `None` means
/// "defer to the model's `reasoning_mode`".
///
/// - Codex: `output_tokens` already includes reasoning; never bill it on top.
/// - Everyone else: defer to the model.
fn reasoning_mode_for_source(source: SourceKind) -> Option<ReasoningMode> {
    match source {
        SourceKind::Codex => Some(ReasoningMode::IncludedInOutput),
        _ => None,
    }
}

/// Shared lookup: direct match → synthetic reattribution → generic
/// `provider/model` strip. Used by `cost_for_usage` and (later) by the
/// hotspots / claude-md attribution paths so synthetic-routed turns price
/// consistently across views.
pub fn lookup_model_rate<'a>(model: &str, pricing: &'a PricingTable) -> Option<&'a ModelCost> {
    if let Some(direct) = pricing.get(model) {
        return Some(direct);
    }
    let reattributed = resolve_provider(model);
    if reattributed.normalized_model != model {
        if let Some(via) = pricing.get(&reattributed.normalized_model) {
            return Some(via);
        }
    }
    let stripped = strip_provider_prefix(model);
    if stripped != model {
        if let Some(via) = pricing.get(stripped) {
            return Some(via);
        }
    }
    None
}

fn strip_provider_prefix(model: &str) -> &str {
    match model.find('/') {
        Some(i) => &model[i + 1..],
        None => model,
    }
}

pub fn sum_costs<I, B>(costs: I) -> CostBreakdown
where
    I: IntoIterator<Item = B>,
    B: std::borrow::Borrow<CostBreakdown>,
{
    let mut acc = CostBreakdown {
        model: "aggregate".to_string(),
        total: 0.0,
        input: 0.0,
        output: 0.0,
        reasoning: 0.0,
        cache_read: 0.0,
        cache_create: 0.0,
    };
    for c in costs {
        let c = c.borrow();
        acc.total += c.total;
        acc.input += c.input;
        acc.output += c.output;
        acc.reasoning += c.reasoning;
        acc.cache_read += c.cache_read;
        acc.cache_create += c.cache_create;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::pricing::{load_builtin_pricing, ModelCost, ReasoningMode};
    use crate::reader::{SourceKind, ToolCall, TurnRecord, Usage};

    fn turn(model: &str, usage: Usage, source: SourceKind) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: "s".into(),
            session_path: None,
            message_id: "m".into(),
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
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn usage_with(input: u64, output: u64, reasoning: u64) -> Usage {
        Usage {
            input,
            output,
            reasoning,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    #[test]
    fn loads_builtin_pricing_and_finds_claude_models() {
        let p = load_builtin_pricing();
        assert!(p.contains_key("claude-opus-4-7"), "opus-4-7 present");
        assert!(p.contains_key("claude-sonnet-4-6"), "sonnet-4-6 present");
        assert!(p.contains_key("claude-haiku-4-5"), "haiku-4-5 present");
    }

    #[test]
    fn computes_dollars_for_a_simple_sonnet_turn() {
        let p = load_builtin_pricing();
        let c = cost_for_turn(
            &turn(
                "claude-sonnet-4-6",
                usage_with(1_000_000, 1_000_000, 0),
                SourceKind::ClaudeCode,
            ),
            &p,
        )
        .expect("priced");
        let rate = p.get("claude-sonnet-4-6").unwrap();
        assert_eq!(c.input, rate.input);
        assert_eq!(c.output, rate.output);
        assert_eq!(c.total, rate.input + rate.output);
    }

    #[test]
    fn applies_cache_write_rate_to_both_5m_and_1h_cache_creation() {
        let p = load_builtin_pricing();
        let c = cost_for_usage(
            &Usage {
                input: 0,
                output: 0,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 500_000,
                cache_create_1h: 500_000,
            },
            "claude-opus-4-7",
            &p,
            CostForUsageOptions::default(),
        )
        .expect("priced");
        assert_eq!(
            c.cache_create,
            p.get("claude-opus-4-7").unwrap().cache_write
        );
    }

    #[test]
    fn bills_reasoning_at_output_rate_for_claude_same_as_output() {
        let p = load_builtin_pricing();
        let c = cost_for_turn(
            &turn(
                "claude-sonnet-4-6",
                usage_with(0, 1_000_000, 1_000_000),
                SourceKind::ClaudeCode,
            ),
            &p,
        )
        .expect("priced");
        let rate = p.get("claude-sonnet-4-6").unwrap();
        assert_eq!(rate.reasoning_mode, ReasoningMode::SameAsOutput);
        assert_eq!(c.output, rate.output);
        assert_eq!(c.reasoning, rate.output);
        assert_eq!(c.total, rate.output * 2.0);
    }

    #[test]
    fn does_not_double_bill_reasoning_for_codex_turns() {
        // Acceptance criterion from issue #32: a Codex turn with
        //   input = 1_000_000, output = 500_000, reasoning = 200_000
        // and a model priced input=2.5/output=15 should bill 10.0, not 13.0.
        let mut p = PricingTable::new();
        p.insert(
            "gpt-5-codex".into(),
            ModelCost {
                input: 2.5,
                output: 15.0,
                cache_read: 0.0,
                cache_write: 2.5,
                reasoning: None,
                reasoning_mode: ReasoningMode::SameAsOutput,
            },
        );
        let c = cost_for_turn(
            &turn(
                "gpt-5-codex",
                usage_with(1_000_000, 500_000, 200_000),
                SourceKind::Codex,
            ),
            &p,
        )
        .expect("priced");
        assert_eq!(c.input, 2.5);
        assert_eq!(c.output, 7.5);
        assert_eq!(
            c.reasoning, 0.0,
            "reasoning is informational for Codex, not billed",
        );
        assert_eq!(c.total, 10.0);
    }

    #[test]
    fn codex_regression_11_3_percent_overstatement_scenario() {
        // 10 Codex turns aggregated: input 660_698, output 52_676,
        // reasoning 29_070, cacheRead 5_618_688. The issue documents
        // $4.282607 (current/wrong) vs $3.846557 (corrected) at gpt-5-codex
        // pricing (input=1.25, output=10, cacheRead=0.125). We assert the
        // corrected number to within 1e-9.
        let mut p = PricingTable::new();
        p.insert(
            "gpt-5-codex".into(),
            ModelCost {
                input: 1.25,
                output: 10.0,
                cache_read: 0.125,
                cache_write: 1.25,
                reasoning: None,
                reasoning_mode: ReasoningMode::SameAsOutput,
            },
        );
        let c = cost_for_turn(
            &turn(
                "gpt-5-codex",
                Usage {
                    input: 660_698,
                    output: 52_676,
                    reasoning: 29_070,
                    cache_read: 5_618_688,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                SourceKind::Codex,
            ),
            &p,
        )
        .expect("priced");

        let expected = (660_698.0_f64 / 1_000_000.0) * 1.25
            + (52_676.0_f64 / 1_000_000.0) * 10.0
            + (5_618_688.0_f64 / 1_000_000.0) * 0.125;
        assert!(
            (c.total - expected).abs() < 1e-9,
            "expected {expected}, got {}",
            c.total
        );
        assert_eq!(c.reasoning, 0.0);
    }

    #[test]
    fn honors_a_separate_reasoning_tariff_when_models_dev_provides_one() {
        // Acceptance criterion from issue #32: a model with input=1, output=4,
        // reasoning=8 and 1M tokens of each should bill 13.
        let mut p = PricingTable::new();
        p.insert(
            "synthetic-reasoner".into(),
            ModelCost {
                input: 1.0,
                output: 4.0,
                cache_read: 0.0,
                cache_write: 1.0,
                reasoning: Some(8.0),
                reasoning_mode: ReasoningMode::Separate,
            },
        );
        let c = cost_for_usage(
            &usage_with(1_000_000, 1_000_000, 1_000_000),
            "synthetic-reasoner",
            &p,
            CostForUsageOptions::default(),
        )
        .expect("priced");
        assert_eq!(c.input, 1.0);
        assert_eq!(c.output, 4.0);
        assert_eq!(c.reasoning, 8.0);
        assert_eq!(c.total, 13.0);
    }

    #[test]
    fn explicit_reasoning_mode_option_overrides_the_model_default() {
        let mut p = PricingTable::new();
        p.insert(
            "override-test".into(),
            ModelCost {
                input: 1.0,
                output: 10.0,
                cache_read: 0.0,
                cache_write: 1.0,
                reasoning: None,
                reasoning_mode: ReasoningMode::SameAsOutput,
            },
        );
        let usage = usage_with(0, 0, 1_000_000);
        let billed = cost_for_usage(&usage, "override-test", &p, CostForUsageOptions::default())
            .expect("billed priced");
        let skipped = cost_for_usage(
            &usage,
            "override-test",
            &p,
            CostForUsageOptions {
                reasoning_mode: Some(ReasoningMode::IncludedInOutput),
            },
        )
        .expect("skipped priced");
        assert_eq!(billed.reasoning, 10.0);
        assert_eq!(skipped.reasoning, 0.0);
    }

    #[test]
    fn returns_none_for_unknown_model() {
        let p = load_builtin_pricing();
        let c = cost_for_turn(
            &turn(
                "definitely-not-a-model",
                usage_with(100, 0, 0),
                SourceKind::ClaudeCode,
            ),
            &p,
        );
        assert!(c.is_none());
    }

    #[test]
    fn cache_read_is_much_cheaper_than_input() {
        let p = load_builtin_pricing();
        let rate = p.get("claude-opus-4-7").unwrap();
        assert!(rate.cache_read < rate.input);
    }

    #[test]
    fn lookup_model_rate_falls_back_to_synthetic_normalization() {
        // If the bundled snapshot lists e.g. `qwen3-coder` but a session logs
        // `synthetic/qwen3-coder`, the synthetic normalization branch should
        // route the lookup to the bare id without forcing callers to
        // pre-strip.
        let mut p = PricingTable::new();
        p.insert(
            "qwen3-coder".into(),
            ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.0,
                cache_write: 1.0,
                reasoning: None,
                reasoning_mode: ReasoningMode::SameAsOutput,
            },
        );
        let rate = lookup_model_rate("synthetic/qwen3-coder", &p).expect("synthetic prefix routes");
        assert_eq!(rate.input, 1.0);
        assert_eq!(rate.output, 2.0);
    }

    #[test]
    fn sum_costs_aggregates_each_field_and_labels_aggregate() {
        let a = CostBreakdown {
            model: "x".into(),
            total: 1.0,
            input: 0.5,
            output: 0.5,
            reasoning: 0.0,
            cache_read: 0.0,
            cache_create: 0.0,
        };
        let b = CostBreakdown {
            model: "y".into(),
            total: 2.0,
            input: 1.0,
            output: 1.0,
            reasoning: 0.0,
            cache_read: 0.0,
            cache_create: 0.0,
        };
        let s = sum_costs([&a, &b]);
        assert_eq!(s.model, "aggregate");
        assert_eq!(s.total, 3.0);
        assert_eq!(s.input, 1.5);
        assert_eq!(s.output, 1.5);
    }

    #[test]
    fn sum_costs_on_empty_input_returns_zero_aggregate() {
        let empty: Vec<CostBreakdown> = vec![];
        let s = sum_costs(empty.iter());
        assert_eq!(s.model, "aggregate");
        assert_eq!(s.total, 0.0);
    }
}
