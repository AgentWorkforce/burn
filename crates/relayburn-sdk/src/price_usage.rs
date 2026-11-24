//! Price already-counted token usage without reading any session.
//!
//! An embedder that has its own per-request token counts (a pipeline that
//! stores usage rows, a control plane that meters calls) passes them here and
//! gets the same per-request USD burn's other cost views compute: the same
//! model lookup, provider precedence, context tiers, cache-write TTLs and
//! per-source reasoning rules. Nothing is read from disk except the optional
//! pricing override; no ledger or session store is opened.
//!
//! A cost burn cannot establish comes back as `costUsdMicros: null`, never
//! zero, so callers can keep "unknown cost" distinct from "free". Requests
//! reject unknown fields, so a misspelled counter fails loudly instead of
//! pricing as zero tokens.

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};

use crate::analyze::{
    builtin_pricing_sha256, cost_for_usage, load_pricing_reporting, reasoning_mode_for_source,
    CostBreakdown, CostForUsageOptions, PricingTable,
};
use crate::reader::{SourceKind, Usage};
use crate::session_metrics::usd_to_micros;

pub const USAGE_PRICES_SCHEMA: &str = "burn.usage-prices.v1";

/// Token counts for one request (or one aggregate, see
/// [`UsagePriceRequest::aggregate`]). Missing counters read as zero;
/// unknown counter names are an error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct UsageTokens {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    /// Cache writes at the default (5-minute) TTL.
    pub cache_create_5m: u64,
    /// Cache writes at the 1-hour TTL.
    pub cache_create_1h: u64,
}

impl From<&UsageTokens> for Usage {
    fn from(tokens: &UsageTokens) -> Self {
        Usage {
            input: tokens.input,
            output: tokens.output,
            reasoning: tokens.reasoning,
            cache_read: tokens.cache_read,
            cache_create_5m: tokens.cache_create_5m,
            cache_create_1h: tokens.cache_create_1h,
        }
    }
}

/// A present-but-nullable field: absent is an error, `null` is `None`.
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsagePriceRequest {
    /// Model id as the harness logged it; aliases and `provider/` prefixes
    /// resolve the same way they do for transcript turns.
    pub model: String,
    /// Harness the usage came from. Required (may be `null`): it selects the
    /// billing provider (Copilot CLI) and reasoning semantics (Codex and
    /// Copilot fold reasoning into output), so leaving it out silently would
    /// misprice those sources. `null` means "not a burn harness": the
    /// model's own tariff and reasoning rule apply.
    #[serde(deserialize_with = "required_nullable")]
    pub source: Option<SourceKind>,
    pub usage: UsageTokens,
    /// The usage sums several requests of unknown size, so no context tier
    /// is applied (base rates only).
    #[serde(default)]
    pub aggregate: bool,
    /// The caller measured these counters. All-zero measured usage costs $0
    /// even for an unpriced model; all-zero unmeasured usage stays unknown.
    #[serde(default)]
    pub measured: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePrice {
    /// Whether burn established this request's cost: a tariff priced it, or
    /// it was measured zero usage. When `false`, `costUsdMicros` and
    /// `breakdown` are `null`.
    pub cost_known: bool,
    /// Total rounded to micro-USD for this request alone. To reconcile with
    /// `measureSession` (which rounds once per model), sum
    /// `breakdown.total` and round the sum instead of adding these.
    pub cost_usd_micros: Option<u64>,
    /// Unrounded per-bucket USD, labelled with the request's model.
    pub breakdown: Option<CostBreakdown>,
}

/// Which price table produced a [`UsagePrices`] document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingProvenance {
    /// `relayburn-sdk` version: identifies the code-level pricing rules
    /// (aliases, provider precedence, cache-TTL and reasoning rules).
    pub sdk_version: String,
    /// Hex SHA-256 of the bundled pricing data compiled into this build
    /// (models.dev snapshot plus protected first-party model ids).
    pub builtin_pricing_sha256: String,
    /// Hex SHA-256 of the override file merged over the builtin table;
    /// `null` when none was given, it failed to read or parse (burn then
    /// prices with the builtin alone), or it priced no model.
    pub override_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePrices {
    pub schema: String,
    pub pricing: PricingProvenance,
    /// One entry per request, in request order.
    pub prices: Vec<UsagePrice>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceUsageOptions {
    pub requests: Vec<UsagePriceRequest>,
    /// Optional models.dev-format override merged over the builtin table.
    #[serde(default)]
    pub pricing_path: Option<PathBuf>,
}

/// Price each request with burn's tariffs. Never fails: an unreadable
/// override falls back to the builtin table (`overrideSha256: null`) and a
/// cost burn cannot establish is `null`.
pub fn price_usage(options: PriceUsageOptions) -> UsagePrices {
    let (pricing, override_sha256) = load_pricing_reporting(options.pricing_path.as_deref());
    UsagePrices {
        schema: USAGE_PRICES_SCHEMA.to_string(),
        pricing: PricingProvenance {
            sdk_version: env!("CARGO_PKG_VERSION").to_string(),
            builtin_pricing_sha256: builtin_pricing_sha256().to_string(),
            override_sha256,
        },
        prices: options
            .requests
            .iter()
            .map(|request| price_one(request, &pricing))
            .collect(),
    }
}

fn price_one(request: &UsagePriceRequest, pricing: &PricingTable) -> UsagePrice {
    let usage = Usage::from(&request.usage);
    let breakdown = if request.measured && usage == Usage::default() {
        Some(zero_breakdown(&request.model))
    } else {
        cost_for_usage(
            &usage,
            &request.model,
            pricing,
            CostForUsageOptions {
                reasoning_mode: request.source.and_then(reasoning_mode_for_source),
                base_tier: request.aggregate,
                source: request.source,
            },
        )
    };
    // A total that is not a finite, non-negative micro-USD amount (a bad
    // override rate) is not a known cost either.
    match breakdown.and_then(|cost| usd_to_micros(cost.total).map(|micros| (cost, micros))) {
        Some((cost, micros)) => UsagePrice {
            cost_known: true,
            cost_usd_micros: Some(micros),
            breakdown: Some(cost),
        },
        None => UsagePrice {
            cost_known: false,
            cost_usd_micros: None,
            breakdown: None,
        },
    }
}

fn zero_breakdown(model: &str) -> CostBreakdown {
    CostBreakdown {
        model: model.to_string().into(),
        total: 0.0,
        input: 0.0,
        output: 0.0,
        reasoning: 0.0,
        cache_read: 0.0,
        cache_create: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::cost_for_turn;
    use crate::reader::{ToolCall, TurnRecord};
    use std::path::Path;

    fn request(model: &str, source: Option<SourceKind>, usage: UsageTokens) -> UsagePriceRequest {
        UsagePriceRequest {
            model: model.to_string(),
            source,
            usage,
            aggregate: false,
            measured: false,
        }
    }

    fn tokens(input: u64, output: u64) -> UsageTokens {
        UsageTokens {
            input,
            output,
            ..UsageTokens::default()
        }
    }

    fn price(requests: Vec<UsagePriceRequest>) -> UsagePrices {
        price_usage(PriceUsageOptions {
            requests,
            pricing_path: None,
        })
    }

    fn price_with(path: &Path, requests: Vec<UsagePriceRequest>) -> UsagePrices {
        price_usage(PriceUsageOptions {
            requests,
            pricing_path: Some(path.to_path_buf()),
        })
    }

    fn write(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("pricing.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// One fx model: base 2/10/0.5/4 (+1h write 6), and 4/20 above 1000
    /// context tokens.
    const FX_PRICING: &str = r#"{"fx-lab":{"models":{"fx-model":{"cost":{
        "input":2,"output":10,"cache_read":0.5,"cache_write":4,"cache_write_1h":6,
        "tiers":[{"input":4,"output":20,"cache_read":1,"cache_write":8,"cache_write_1h":12,
                  "tier":{"type":"context","size":1000}}]}}}}}"#;

    fn turn(model: &str, usage: Usage, source: SourceKind) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: "fx-session".into(),
            session_path: None,
            message_id: "fx-message".into(),
            turn_index: 0,
            request_count: None,
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
            reasoning: None,
        }
    }

    #[test]
    fn unknown_model_is_null_not_zero() {
        let out = price(vec![request(
            "fx-no-such-model",
            None,
            tokens(1_000, 1_000),
        )]);
        assert_eq!(out.schema, USAGE_PRICES_SCHEMA);
        let only = &out.prices[0];
        assert!(!only.cost_known);
        assert_eq!(only.cost_usd_micros, None);
        assert_eq!(only.breakdown, None);
    }

    #[test]
    fn override_tariff_prices_each_bucket_and_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), FX_PRICING);
        let usage = UsageTokens {
            input: 100,
            output: 100,
            cache_read: 100,
            cache_create_5m: 100,
            cache_create_1h: 100,
            ..UsageTokens::default()
        };
        let out = price_with(&path, vec![request("fx-model", None, usage)]);
        assert_eq!(
            out.pricing.override_sha256.as_deref().map(str::len),
            Some(64)
        );
        // Under the 1000-token tier: 100 tokens each at 2, 10, 0.5, 4, 6 per M.
        assert_eq!(out.prices[0].cost_usd_micros, Some(2_250));
        let breakdown = out.prices[0].breakdown.as_ref().unwrap();
        assert!((breakdown.cache_create - 0.001).abs() < 1e-12);
    }

    #[test]
    fn context_tier_applies_per_request_but_not_to_aggregates() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), FX_PRICING);
        let big = tokens(2_000, 0);
        let mut aggregate = request("fx-model", None, big.clone());
        aggregate.aggregate = true;
        let out = price_with(&path, vec![request("fx-model", None, big), aggregate]);
        assert_eq!(out.prices[0].cost_usd_micros, Some(8_000));
        assert_eq!(out.prices[1].cost_usd_micros, Some(4_000));
    }

    #[test]
    fn measured_zero_usage_is_free_even_unpriced() {
        let mut measured = request("fx-no-such-model", None, UsageTokens::default());
        measured.measured = true;
        let unmeasured = request("fx-no-such-model", None, UsageTokens::default());
        let out = price(vec![measured, unmeasured]);
        assert!(out.prices[0].cost_known);
        assert_eq!(out.prices[0].cost_usd_micros, Some(0));
        assert!(!out.prices[1].cost_known);
        assert_eq!(out.prices[1].cost_usd_micros, None);
    }

    #[test]
    fn matches_cost_for_turn_per_source() {
        let pricing = crate::analyze::load_pricing(None);
        let usage = UsageTokens {
            input: 12_000,
            output: 3_000,
            reasoning: 40_000,
            cache_read: 50_000,
            cache_create_5m: 7_000,
            cache_create_1h: 2_000,
        };
        for (model, source) in [
            ("claude-sonnet-4-5", SourceKind::ClaudeCode),
            ("gpt-5", SourceKind::Codex),
            ("gpt-5", SourceKind::CopilotCli),
        ] {
            let expected = cost_for_turn(&turn(model, Usage::from(&usage), source), &pricing)
                .expect("bundled snapshot prices the model");
            let out = price(vec![request(model, Some(source), usage.clone())]);
            let breakdown = out.prices[0].breakdown.as_ref().unwrap();
            assert_eq!(breakdown.total, expected.total, "{model} via {source}");
            assert_eq!(out.prices[0].cost_usd_micros, usd_to_micros(expected.total));
        }
    }

    #[test]
    fn codex_source_does_not_bill_reasoning_on_top() {
        let usage = UsageTokens {
            input: 1_000,
            output: 1_000,
            reasoning: 1_000_000,
            ..UsageTokens::default()
        };
        let out = price(vec![
            request("gpt-5", Some(SourceKind::Codex), usage.clone()),
            request("gpt-5", None, usage),
        ]);
        let codex = out.prices[0].breakdown.as_ref().expect("gpt-5 is priced");
        let unattributed = out.prices[1].breakdown.as_ref().expect("gpt-5 is priced");
        assert_eq!(codex.reasoning, 0.0);
        assert!(unattributed.reasoning > 0.0);
    }

    #[test]
    fn copilot_source_takes_the_billing_providers_tariff() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            r#"{"fx-lab":{"models":{"fx-model":{"cost":{"input":2,"output":10}}}},
                "github-copilot":{"models":{"fx-model":{"cost":{"input":1,"output":5}}}}}"#,
        );
        let usage = tokens(1_000_000, 0);
        let out = price_with(
            &path,
            vec![
                request("fx-model", Some(SourceKind::CopilotCli), usage.clone()),
                request("fx-model", Some(SourceKind::ClaudeCode), usage),
            ],
        );
        assert_eq!(out.prices[0].cost_usd_micros, Some(1_000_000));
        assert_eq!(out.prices[1].cost_usd_micros, Some(2_000_000));
    }

    #[test]
    fn override_provenance_is_null_unless_it_priced_something() {
        let dir = tempfile::tempdir().unwrap();
        let empty = write(dir.path(), "{}");
        assert_eq!(price_with(&empty, vec![]).pricing.override_sha256, None);
        let missing = dir.path().join("fx-missing.json");
        assert_eq!(price_with(&missing, vec![]).pricing.override_sha256, None);
        let out = price(vec![]);
        assert_eq!(out.pricing.override_sha256, None);
        assert_eq!(out.pricing.builtin_pricing_sha256.len(), 64);
        assert_eq!(out.pricing.sdk_version, env!("CARGO_PKG_VERSION"));
        assert!(out.prices.is_empty());
    }

    #[test]
    fn a_non_finite_total_is_not_a_known_cost() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            r#"{"fx-lab":{"models":{"fx-model":{"cost":{"input":-5,"output":0}}}}}"#,
        );
        let out = price_with(&path, vec![request("fx-model", None, tokens(1_000, 0))]);
        assert!(!out.prices[0].cost_known);
        assert_eq!(out.prices[0].breakdown, None);
    }

    #[test]
    fn request_json_requires_source_and_rejects_unknown_fields() {
        let parsed: UsagePriceRequest = serde_json::from_str(
            r#"{"model":"fx-model","source":"codex","usage":{"input":5,"cacheCreate1h":7}}"#,
        )
        .unwrap();
        assert_eq!(parsed.source, Some(SourceKind::Codex));
        assert_eq!(parsed.usage.cache_create_1h, 7);
        assert!(!parsed.measured && !parsed.aggregate);

        let null_source: UsagePriceRequest =
            serde_json::from_str(r#"{"model":"fx-model","source":null,"usage":{}}"#).unwrap();
        assert_eq!(null_source.source, None);

        for bad in [
            r#"{"model":"fx-model","usage":{}}"#,
            r#"{"model":"fx-model","source":null}"#,
            r#"{"model":"fx-model","source":null,"usage":{"cacheWrite":5}}"#,
            r#"{"model":"fx-model","source":null,"usage":{},"extra":1}"#,
        ] {
            assert!(
                serde_json::from_str::<UsagePriceRequest>(bad).is_err(),
                "{bad}"
            );
        }
    }
}
