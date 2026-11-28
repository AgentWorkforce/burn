//! `priceUsage`: price already-counted token usage with burn's tariffs.
//! Reads no session and opens no ledger.

use napi::Error as NapiError;
use napi_derive::napi;
use serde_json::Value as JsonValue;

use relayburn_sdk as sdk;

use crate::{invalid_arg, BigIntPromoting, BurnError, SDK_ERROR_CODE};

fn price_usage_options_from_value(opts: JsonValue) -> Result<sdk::PriceUsageOptions, BurnError> {
    serde_json::from_value(opts)
        .map_err(|e| invalid_arg(format!("invalid priceUsage options: {e}")))
}

/// One price per request, in order; returns `burn.usage-prices.v1`. An
/// unknown model prices as `costUsdMicros: null`, never zero.
#[napi(
    js_name = "priceUsage",
    ts_return_type = "import('./index').UsagePrices"
)]
pub fn price_usage(opts: JsonValue) -> Result<BigIntPromoting, BurnError> {
    let result = sdk::price_usage(price_usage_options_from_value(opts)?);
    serde_json::to_value(&result)
        .map(BigIntPromoting)
        .map_err(|e| NapiError::new(SDK_ERROR_CODE, format!("serialize priceUsage: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn options_parse_camel_case_requests() {
        let opts = price_usage_options_from_value(json!({
            "requests": [{"model": "fx-model", "source": "claude-code",
                          "usage": {"input": 3, "cacheCreate5m": 4}, "measured": true}],
            "pricingPath": "/fx/pricing.json"
        }))
        .unwrap();
        assert_eq!(opts.requests.len(), 1);
        assert_eq!(opts.requests[0].usage.cache_create_5m, 4);
        assert!(opts.requests[0].measured);
        assert!(opts.pricing_path.is_some());
    }

    #[test]
    fn options_reject_a_bad_source() {
        let err = price_usage_options_from_value(json!({
            "requests": [{"model": "fx-model", "source": "fx-harness", "usage": {}}]
        }));
        assert!(err.is_err());
    }
}
