//! Price already-counted token usage without reading any session.
//!
//! An embedder that has its own per-request token counts (a pipeline that
//! stores usage rows, a control plane that meters calls) passes them here and
//! gets the same per-request USD burn's other cost views compute: the same
//! model lookup, provider precedence, context tiers, cache-write TTLs and
//! per-source reasoning rules. Nothing is read from disk except the optional
//! pricing override; no ledger or session store is opened.
//!
//! A model burn has no tariff for comes back with `costUsdMicros: null`,
//! never zero, so callers can keep "unknown cost" distinct from "free".

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::analyze::{
    builtin_pricing_sha256, cost_for_usage, load_pricing_reporting, reasoning_mode_for_source,
    CostBreakdown, CostForUsageOptions, PricingTable,
};
use crate::reader::{SourceKind, Usage};
use crate::session_metrics::usd_to_micros;

pub const USAGE_PRICES_SCHEMA: &str = "burn.usage-prices.v1";

/// Token counts for one request (or one aggregate, see
/// [`UsagePriceRequest::aggregate`]). Missing counters read as zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePriceRequest {
    /// Model id as the harness logged it; aliases and `provider/` prefixes
    /// resolve the same way they do for transcript turns.
    pub model: String,
    /// Harness the usage came from. Selects the billing provider (Copilot
    /// CLI) and reasoning semantics (Codex/Copilot fold reasoning into
    /// output). `None` defers both to the model's tariff.
    #[serde(default)]
    pub source: Option<SourceKind>,
    #[serde(default)]
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
    pub model: String,
    /// `false` when burn has no tariff for `model` (cost unknown).
    pub priced: bool,
    /// Rounded total in micro-USD; `None` when the cost is unknown.
    pub cost_usd_micros: Option<u64>,
    /// Per-bucket USD; `None` when the cost is unknown.
    pub breakdown: Option<CostBreakdown>,
}

/// Which price table produced a [`UsagePrices`] document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingProvenance {
    /// Hex SHA-256 of the models.dev snapshot compiled into this build.
    pub builtin_snapshot_sha256: String,
    /// An override file was read and merged over the builtin table.
    pub override_applied: bool,
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
#[serde(rename_all = "camelCase")]
pub struct PriceUsageOptions {
    pub requests: Vec<UsagePriceRequest>,
    /// Optional models.dev-format override merged over the builtin table.
    pub pricing_path: Option<PathBuf>,
}

/// Price each request with burn's tariffs. Never fails: an unreadable
/// override falls back to the builtin table (`overrideApplied: false`) and an
/// unknown model yields a null cost.
pub fn price_usage(options: PriceUsageOptions) -> UsagePrices {
    let (pricing, override_applied) = load_pricing_reporting(options.pricing_path.as_deref());
    UsagePrices {
        schema: USAGE_PRICES_SCHEMA.to_string(),
        pricing: PricingProvenance {
            builtin_snapshot_sha256: builtin_pricing_sha256(),
            override_applied,
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
    UsagePrice {
        model: request.model.clone(),
        priced: breakdown.is_some(),
        cost_usd_micros: breakdown
            .as_ref()
            .and_then(|cost| usd_to_micros(cost.total)),
        breakdown,
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
    use std::io::Write;

    fn request(model: &str, usage: UsageTokens) -> UsagePriceRequest {
        UsagePriceRequest {
            model: model.to_string(),
            source: None,
            usage,
            aggregate: false,
            measured: false,
        }
    }

    fn million(input: u64, output: u64) -> UsageTokens {
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

    /// A models.dev-format override with one fixed-tariff fx model.
    fn override_file(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("pricing.json");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(
            br#"{"fx-lab":{"id":"fx-lab","models":{"fx-model":{"id":"fx-model",
            "cost":{"input":2,"output":10,"cache_read":0.5,"cache_write":4}}}}}"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn unknown_model_is_null_not_zero() {
        let out = price(vec![request("fx-no-such-model", million(1_000, 1_000))]);
        assert_eq!(out.schema, USAGE_PRICES_SCHEMA);
        let only = &out.prices[0];
        assert!(!only.priced);
        assert_eq!(only.cost_usd_micros, None);
        assert_eq!(only.breakdown, None);
    }

    #[test]
    fn override_tariff_prices_each_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let out = price_usage(PriceUsageOptions {
            requests: vec![request(
                "fx-model",
                UsageTokens {
                    input: 1_000_000,
                    output: 1_000_000,
                    cache_read: 1_000_000,
                    cache_create_5m: 1_000_000,
                    ..UsageTokens::default()
                },
            )],
            pricing_path: Some(override_file(dir.path())),
        });
        assert!(out.pricing.override_applied);
        let only = &out.prices[0];
        // 2 input + 10 output + 0.5 cache read + 4 cache write.
        assert_eq!(only.cost_usd_micros, Some(16_500_000));
        let breakdown = only.breakdown.as_ref().unwrap();
        assert_eq!(breakdown.input, 2.0);
        assert_eq!(breakdown.cache_create, 4.0);
    }

    #[test]
    fn measured_zero_usage_is_free_even_unpriced() {
        let mut measured = request("fx-no-such-model", UsageTokens::default());
        measured.measured = true;
        let unmeasured = request("fx-no-such-model", UsageTokens::default());
        let out = price(vec![measured, unmeasured]);
        assert_eq!(out.prices[0].cost_usd_micros, Some(0));
        assert!(out.prices[0].priced);
        assert_eq!(out.prices[1].cost_usd_micros, None);
    }

    #[test]
    fn matches_turn_pricing_for_a_builtin_model() {
        let usage = million(10_000, 2_000);
        let out = price(vec![request("claude-sonnet-4-5", usage.clone())]);
        let pricing = crate::analyze::load_pricing(None);
        let expected = cost_for_usage(
            &Usage::from(&usage),
            "claude-sonnet-4-5",
            &pricing,
            CostForUsageOptions::default(),
        )
        .expect("claude-sonnet-4-5 is in the bundled snapshot");
        assert_eq!(out.prices[0].cost_usd_micros, usd_to_micros(expected.total));
        assert!(out.prices[0].cost_usd_micros.unwrap() > 0);
    }

    #[test]
    fn codex_source_does_not_bill_reasoning_on_top() {
        let usage = UsageTokens {
            input: 1_000,
            output: 1_000,
            reasoning: 1_000_000,
            ..UsageTokens::default()
        };
        let mut codex = request("gpt-5", usage.clone());
        codex.source = Some(SourceKind::Codex);
        let out = price(vec![codex]);
        let breakdown = out.prices[0].breakdown.as_ref().expect("gpt-5 is priced");
        assert_eq!(breakdown.reasoning, 0.0);
    }

    #[test]
    fn missing_override_falls_back_and_reports_it() {
        let out = price_usage(PriceUsageOptions {
            requests: vec![],
            pricing_path: Some(PathBuf::from("/nonexistent/fx-pricing.json")),
        });
        assert!(!out.pricing.override_applied);
        assert_eq!(out.pricing.builtin_snapshot_sha256.len(), 64);
        assert!(out.prices.is_empty());
    }

    #[test]
    fn request_deserializes_with_defaults() {
        let parsed: UsagePriceRequest = serde_json::from_str(
            r#"{"model":"fx-model","source":"codex","usage":{"input":5,"cacheCreate1h":7}}"#,
        )
        .unwrap();
        assert_eq!(parsed.source, Some(SourceKind::Codex));
        assert_eq!(parsed.usage.input, 5);
        assert_eq!(parsed.usage.cache_create_1h, 7);
        assert!(!parsed.measured && !parsed.aggregate);
    }
}
