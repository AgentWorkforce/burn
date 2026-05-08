//! Effective-provider helpers + per-provider aggregator. Rust port of
//! `packages/analyze/src/provider.ts`.
//!
//! [`provider_for`] resolves the canonical provider label for a turn,
//! preferring synthetic-style router prefixes (handled by
//! [`crate::provider_reattribution`]) before falling back to a raw
//! `provider/model` model prefix and finally to the collector-implied
//! provider. [`aggregate_by_provider`] rolls turns up into a
//! cost-descending list of [`ProviderAggregateRow`]s for the per-provider
//! views (`compare`, `summary --by-provider`).

use std::cmp::Ordering;
use std::collections::BTreeSet;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::reader::{Coverage, SourceKind, TurnRecord, Usage};

use crate::analyze::cost::{cost_for_turn, CostBreakdown};
use crate::analyze::pricing::PricingTable;
use crate::analyze::provider_reattribution::{
    default_rules, resolve_provider_with_rules, ProviderRule,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnProvider {
    pub provider: String,
    pub raw_model: String,
    pub normalized_model: String,
    pub matched_rule: Option<String>,
}

/// Lower-cased provider labels used to filter turns. `BTreeSet` (vs the TS
/// `ReadonlySet`) gives deterministic iteration, which matters for any
/// future byte-equivalent output.
pub type ProviderFilter = BTreeSet<String>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldCoverage {
    pub known: u64,
    pub missing: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CoverageField {
    Input,
    Output,
    Reasoning,
    CacheRead,
    CacheCreate,
}

pub const COVERAGE_FIELDS: [CoverageField; 5] = [
    CoverageField::Input,
    CoverageField::Output,
    CoverageField::Reasoning,
    CoverageField::CacheRead,
    CoverageField::CacheCreate,
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowCoverage {
    pub input: FieldCoverage,
    pub output: FieldCoverage,
    pub reasoning: FieldCoverage,
    pub cache_read: FieldCoverage,
    pub cache_create: FieldCoverage,
}

impl RowCoverage {
    pub fn field_mut(&mut self, f: CoverageField) -> &mut FieldCoverage {
        match f {
            CoverageField::Input => &mut self.input,
            CoverageField::Output => &mut self.output,
            CoverageField::Reasoning => &mut self.reasoning,
            CoverageField::CacheRead => &mut self.cache_read,
            CoverageField::CacheCreate => &mut self.cache_create,
        }
    }

    pub fn field(&self, f: CoverageField) -> &FieldCoverage {
        match f {
            CoverageField::Input => &self.input,
            CoverageField::Output => &self.output,
            CoverageField::Reasoning => &self.reasoning,
            CoverageField::CacheRead => &self.cache_read,
            CoverageField::CacheCreate => &self.cache_create,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCostAggregateRow {
    pub label: String,
    pub turns: u64,
    pub usage: Usage,
    pub cost: CostBreakdown,
    pub coverage: RowCoverage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAggregateRow {
    pub provider: String,
    pub label: String,
    pub turns: u64,
    pub usage: Usage,
    pub cost: CostBreakdown,
    pub coverage: RowCoverage,
}

#[derive(Debug, Clone, Copy)]
pub struct AggregateByProviderOptions<'a> {
    pub pricing: &'a PricingTable,
    /// `None` defers to [`default_rules`].
    pub rules: Option<&'a [ProviderRule]>,
}

impl<'a> AggregateByProviderOptions<'a> {
    pub fn new(pricing: &'a PricingTable) -> Self {
        Self {
            pricing,
            rules: None,
        }
    }
}

/// Resolve the effective provider for a turn.
///
/// Synthetic-style router prefixes win first. Otherwise we keep provider
/// semantics aligned with CLI rendering by using a raw `provider/model`
/// model prefix when present, then falling back to the collector-implied
/// provider.
pub fn provider_for(turn: &TurnRecord) -> TurnProvider {
    provider_for_with_rules(turn, default_rules())
}

pub fn provider_for_with_rules(turn: &TurnRecord, rules: &[ProviderRule]) -> TurnProvider {
    provider_for_model_with_rules(&turn.model, Some(turn.source), rules)
}

/// Alias of [`provider_for`] — TS exports both `providerFor` and
/// `providerForTurn` so callers can pick the spelling that reads best.
pub fn provider_for_turn(turn: &TurnRecord) -> TurnProvider {
    provider_for(turn)
}

/// Alias of [`provider_for`].
pub fn resolve_turn_provider(turn: &TurnRecord) -> TurnProvider {
    provider_for(turn)
}

pub fn provider_for_model(model: &str, source: Option<SourceKind>) -> TurnProvider {
    provider_for_model_with_rules(model, source, default_rules())
}

pub fn provider_for_model_with_rules(
    model: &str,
    source: Option<SourceKind>,
    rules: &[ProviderRule],
) -> TurnProvider {
    let resolved = resolve_provider_with_rules(model, rules);
    if let Some(provider) = resolved.provider {
        return TurnProvider {
            provider,
            raw_model: model.to_string(),
            normalized_model: resolved.normalized_model,
            matched_rule: resolved.matched_rule,
        };
    }

    if let Some(prefix) = provider_from_model_prefix(model) {
        return TurnProvider {
            provider: prefix,
            raw_model: model.to_string(),
            normalized_model: strip_provider_prefix(model).to_string(),
            matched_rule: None,
        };
    }

    TurnProvider {
        provider: source
            .map(provider_from_source)
            .unwrap_or_else(|| "unknown".into()),
        raw_model: model.to_string(),
        normalized_model: model.to_string(),
        matched_rule: None,
    }
}

/// Filter turns to those whose effective provider (lower-cased) is in
/// `filter`. Returns the input slice unchanged when `filter` is `None`.
pub fn filter_turns_by_provider<'a, T>(
    turns: &'a [T],
    filter: Option<&ProviderFilter>,
) -> Vec<&'a T>
where
    T: AsTurnLike,
{
    filter_turns_by_provider_with_rules(turns, filter, default_rules())
}

pub fn filter_turns_by_provider_with_rules<'a, T>(
    turns: &'a [T],
    filter: Option<&ProviderFilter>,
    rules: &[ProviderRule],
) -> Vec<&'a T>
where
    T: AsTurnLike,
{
    let Some(filter) = filter else {
        return turns.iter().collect();
    };
    turns
        .iter()
        .filter(|t| {
            let resolved =
                provider_for_model_with_rules(t.model_str(), Some(t.source_kind()), rules);
            filter.contains(&resolved.provider.to_lowercase())
        })
        .collect()
}

/// Minimal trait equivalent of TS's structural `Pick<TurnRecord, 'model' |
/// 'source'>`. Implemented for [`TurnRecord`] out of the box; downstream
/// callers can implement it for their own row types if they want to share
/// [`filter_turns_by_provider`].
pub trait AsTurnLike {
    fn model_str(&self) -> &str;
    fn source_kind(&self) -> SourceKind;
}

impl AsTurnLike for TurnRecord {
    fn model_str(&self) -> &str {
        &self.model
    }
    fn source_kind(&self) -> SourceKind {
        self.source
    }
}

pub fn aggregate_by_provider(
    turns: &[TurnRecord],
    opts: AggregateByProviderOptions<'_>,
) -> Vec<ProviderAggregateRow> {
    let rules: &[ProviderRule] = match opts.rules {
        Some(r) => r,
        None => default_rules(),
    };
    // `IndexMap` (vs `HashMap`) preserves first-seen insertion order so the
    // final stable sort by descending cost keeps cross-language tie-breaks
    // matching the TS implementation, where the underlying `Map` already
    // iterates in insertion order. Without this, two providers tied at the
    // same `cost.total` (e.g. multiple unpriced providers all at $0) would
    // surface in arbitrary order from run to run.
    let mut by_provider: IndexMap<String, ProviderAggregateRow> = IndexMap::new();
    for t in turns {
        let provider = provider_for_with_rules(t, rules).provider;
        let provider = if provider.is_empty() {
            "unknown".to_string()
        } else {
            provider
        };
        let row = by_provider
            .entry(provider.clone())
            .or_insert_with(|| empty_provider_row(&provider));
        row.turns += 1;
        row.usage.input += t.usage.input;
        row.usage.output += t.usage.output;
        row.usage.reasoning += t.usage.reasoning;
        row.usage.cache_read += t.usage.cache_read;
        row.usage.cache_create_5m += t.usage.cache_create_5m;
        row.usage.cache_create_1h += t.usage.cache_create_1h;
        accumulate_coverage(&mut row.coverage, t.fidelity.as_ref().map(|f| &f.coverage));
        if let Some(c) = cost_for_turn(t, opts.pricing) {
            row.cost.total += c.total;
            row.cost.input += c.input;
            row.cost.output += c.output;
            row.cost.reasoning += c.reasoning;
            row.cost.cache_read += c.cache_read;
            row.cost.cache_create += c.cache_create;
        }
    }
    let mut rows: Vec<ProviderAggregateRow> = by_provider.into_values().collect();
    rows.sort_by(|a, b| {
        b.cost
            .total
            .partial_cmp(&a.cost.total)
            .unwrap_or(Ordering::Equal)
    });
    rows
}

fn empty_provider_row(provider: &str) -> ProviderAggregateRow {
    ProviderAggregateRow {
        provider: provider.to_string(),
        label: provider.to_string(),
        turns: 0,
        usage: Usage::default(),
        cost: CostBreakdown {
            model: provider.to_string(),
            total: 0.0,
            input: 0.0,
            output: 0.0,
            reasoning: 0.0,
            cache_read: 0.0,
            cache_create: 0.0,
        },
        coverage: RowCoverage::default(),
    }
}

fn accumulate_coverage(target: &mut RowCoverage, coverage: Option<&Coverage>) {
    for f in COVERAGE_FIELDS {
        let known = match coverage {
            None => true, // mirror TS: missing coverage object → assume known
            Some(c) => match f {
                CoverageField::Input => c.has_input_tokens,
                CoverageField::Output => c.has_output_tokens,
                CoverageField::Reasoning => c.has_reasoning_tokens,
                CoverageField::CacheRead => c.has_cache_read_tokens,
                CoverageField::CacheCreate => c.has_cache_create_tokens,
            },
        };
        let slot = target.field_mut(f);
        if known {
            slot.known += 1;
        } else {
            slot.missing += 1;
        }
    }
}

fn provider_from_model_prefix(model: &str) -> Option<String> {
    let i = model.find('/')?;
    if i == 0 {
        return None;
    }
    Some(model[..i].to_lowercase())
}

fn strip_provider_prefix(model: &str) -> &str {
    match model.find('/') {
        Some(i) => &model[i + 1..],
        None => model,
    }
}

fn provider_from_source(source: SourceKind) -> String {
    match source {
        SourceKind::ClaudeCode | SourceKind::AnthropicApi => "anthropic".into(),
        SourceKind::Codex | SourceKind::OpenaiApi => "openai".into(),
        SourceKind::GeminiApi => "google".into(),
        SourceKind::Opencode => "opencode".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::pricing::{ModelCost, ReasoningMode};
    use crate::reader::{ToolCall, Usage};

    fn pricing_fixture() -> PricingTable {
        let mut p = PricingTable::new();
        p.insert(
            "deepseek-r1".into(),
            ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 1.25,
                reasoning: None,
                reasoning_mode: ReasoningMode::SameAsOutput,
            },
        );
        p.insert(
            "gpt-5".into(),
            ModelCost {
                input: 3.0,
                output: 6.0,
                cache_read: 0.3,
                cache_write: 3.75,
                reasoning: None,
                reasoning_mode: ReasoningMode::SameAsOutput,
            },
        );
        p
    }

    fn turn(model: &str, source: SourceKind, usage: Usage) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: "s-provider".into(),
            session_path: None,
            message_id: "m-provider".into(),
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

    fn one_million_in_one_million_out() -> Usage {
        Usage {
            input: 1_000_000,
            output: 1_000_000,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    fn input_only(input: u64) -> Usage {
        Usage {
            input,
            output: 0,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    fn output_only(output: u64) -> Usage {
        Usage {
            input: 0,
            output,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    #[test]
    fn reattributes_synthetic_routed_models_before_source_fallback() {
        let p = provider_for(&turn(
            "hf:deepseek-ai/deepseek-r1",
            SourceKind::ClaudeCode,
            one_million_in_one_million_out(),
        ));
        assert_eq!(p.provider, "synthetic");
        assert_eq!(p.normalized_model, "deepseek-r1");
        assert_eq!(p.matched_rule.as_deref(), Some("synthetic-huggingface"));
    }

    #[test]
    fn falls_through_to_collector_implied_providers() {
        assert_eq!(
            provider_for(&turn(
                "gpt-5",
                SourceKind::Codex,
                one_million_in_one_million_out()
            ))
            .provider,
            "openai"
        );
        assert_eq!(
            provider_for(&turn(
                "anthropic/claude-sonnet-4-6",
                SourceKind::Opencode,
                one_million_in_one_million_out(),
            ))
            .provider,
            "anthropic"
        );
    }

    #[test]
    fn aggregate_groups_synthetic_turns_regardless_of_routing_prefix() {
        let pricing = pricing_fixture();
        let rows = aggregate_by_provider(
            &[
                turn(
                    "hf:deepseek-ai/deepseek-r1",
                    SourceKind::ClaudeCode,
                    input_only(1_000_000),
                ),
                turn(
                    "synthetic/deepseek-r1",
                    SourceKind::ClaudeCode,
                    output_only(1_000_000),
                ),
            ],
            AggregateByProviderOptions::new(&pricing),
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "synthetic");
        assert_eq!(rows[0].turns, 2);
        assert_eq!(rows[0].usage.input, 1_000_000);
        assert_eq!(rows[0].usage.output, 1_000_000);
        assert_eq!(rows[0].cost.total, 3.0);
    }

    #[test]
    fn aggregate_falls_through_to_collector_for_non_synthetic_turns() {
        let pricing = pricing_fixture();
        let rows = aggregate_by_provider(
            &[turn(
                "gpt-5",
                SourceKind::Codex,
                one_million_in_one_million_out(),
            )],
            AggregateByProviderOptions::new(&pricing),
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "openai");
        assert_eq!(rows[0].cost.total, 9.0);
    }

    #[test]
    fn aggregate_returns_empty_for_empty_input() {
        let pricing = pricing_fixture();
        assert!(aggregate_by_provider(&[], AggregateByProviderOptions::new(&pricing)).is_empty());
    }

    #[test]
    fn aggregate_orders_rows_by_descending_total_cost() {
        let pricing = pricing_fixture();
        let rows = aggregate_by_provider(
            &[
                // openai: 1M in @ $3 + 1M out @ $6 → $9
                turn("gpt-5", SourceKind::Codex, one_million_in_one_million_out()),
                // synthetic: 1M in @ $1 → $1
                turn(
                    "hf:deepseek-ai/deepseek-r1",
                    SourceKind::ClaudeCode,
                    input_only(1_000_000),
                ),
            ],
            AggregateByProviderOptions::new(&pricing),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].provider, "openai");
        assert_eq!(rows[1].provider, "synthetic");
        assert!(rows[0].cost.total > rows[1].cost.total);
    }

    #[test]
    fn aggregate_preserves_first_seen_order_for_cost_ties() {
        // Two unpriced providers both end up at $0; the stable sort must
        // preserve first-seen insertion order so output is deterministic and
        // matches the TS `Map`-based implementation.
        let pricing = PricingTable::new();
        let rows = aggregate_by_provider(
            &[
                turn(
                    "unknown-model-a",
                    SourceKind::AnthropicApi,
                    input_only(1_000),
                ),
                turn("unknown-model-b", SourceKind::OpenaiApi, input_only(1_000)),
            ],
            AggregateByProviderOptions::new(&pricing),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cost.total, 0.0);
        assert_eq!(rows[1].cost.total, 0.0);
        // First-seen wins on ties: anthropic before openai.
        assert_eq!(rows[0].provider, "anthropic");
        assert_eq!(rows[1].provider, "openai");
    }

    #[test]
    fn filter_returns_input_unchanged_when_filter_is_none() {
        let turns = vec![turn(
            "claude-sonnet-4-6",
            SourceKind::ClaudeCode,
            one_million_in_one_million_out(),
        )];
        let filtered = filter_turns_by_provider(&turns, None);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_keeps_only_matching_providers() {
        let turns = vec![
            turn("gpt-5", SourceKind::Codex, one_million_in_one_million_out()),
            turn(
                "claude-sonnet-4-6",
                SourceKind::ClaudeCode,
                one_million_in_one_million_out(),
            ),
        ];
        let mut want = ProviderFilter::new();
        want.insert("openai".into());
        let filtered = filter_turns_by_provider(&turns, Some(&want));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].model, "gpt-5");
    }
}

#[cfg(test)]
mod cost_lookup_via_reattribution_tests {
    //! Conformance tests ported from the `costForTurn — reattribution-aware
    //! pricing lookup` describe block in
    //! `packages/analyze/src/provider-reattribution.test.ts`. They live next
    //! to the provider tests so the conformance gate in #267 is colocated.

    use super::*;
    use crate::analyze::cost::cost_for_turn;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::reader::{ToolCall, Usage};

    fn turn(model: &str, usage: Usage) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
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

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    #[test]
    fn prices_hf_deepseek_r1_against_deepseek_r1() {
        let p = load_builtin_pricing();
        assert!(
            p.contains_key("deepseek-r1"),
            "deepseek-r1 expected in builtin pricing"
        );
        let t = turn("hf:deepseek-ai/deepseek-r1", usage(1_000_000, 1_000_000));
        let c = cost_for_turn(&t, &p).expect("non-null cost for synthetic-routed deepseek-r1");
        let rate = p.get("deepseek-r1").unwrap();
        assert_eq!(c.input, rate.input);
        assert_eq!(c.output, rate.output);
        assert_eq!(c.total, rate.input + rate.output);
    }

    #[test]
    fn prices_fireworks_deepseek_r1_against_deepseek_r1() {
        let p = load_builtin_pricing();
        let t = turn("accounts/fireworks/models/deepseek-r1", usage(500_000, 0));
        let c = cost_for_turn(&t, &p).expect("priced");
        let rate = p.get("deepseek-r1").unwrap();
        assert_eq!(c.input, rate.input * 0.5);
    }

    #[test]
    fn returns_none_for_synthetic_prefixed_unknown_models() {
        let p = load_builtin_pricing();
        let c = cost_for_turn(&turn("hf:org/totally-fake-model-xyz", usage(1000, 0)), &p);
        assert!(c.is_none());
    }

    #[test]
    fn does_not_regress_direct_priced_models() {
        let p = load_builtin_pricing();
        let c = cost_for_turn(&turn("claude-sonnet-4-6", usage(1_000_000, 0)), &p).expect("priced");
        assert_eq!(c.input, p.get("claude-sonnet-4-6").unwrap().input);
    }
}
