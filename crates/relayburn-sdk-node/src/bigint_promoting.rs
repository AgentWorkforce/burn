//! `BigIntPromoting`: the JSON → JS walker that carries `u64` counters
//! across the napi boundary as `BigInt` (see the crate docs).

use std::ptr;

use napi::bindgen_prelude::{BigInt, Error as NapiError, Result as NapiResult, ToNapiValue};
use napi::sys;
use serde_json::Value as JsonValue;

use crate::u64_to_bigint;

// ---------------------------------------------------------------------------
// BigIntPromoting — JsonValue → JS value walker that emits BigInt for the
// well-known u64 field names below.
//
// `overhead`, `overheadTrim`, `hotspots`, `compare`, span trees, flow
// graphs, and context deltas return shapes that
// are too recursive (or, in `hotspots`'s case, a discriminated union) to mirror
// cleanly as a single `#[napi(object)]` struct. We keep them on the
// `serde_json::Value` boundary but wrap the result so the standard
// number→JsNumber conversion in napi-rs's serde-json bridge gets
// overridden for the named fields. Anything not in this list rides
// through as a plain JS number, matching the existing TS contract.
// ---------------------------------------------------------------------------

/// Field names that carry `u64` values in the SDK and therefore must be
/// surfaced as JS `BigInt`. Names are camelCased (matching `serde(rename_all
/// = "camelCase")` on the SDK structs); the walker matches these literally
/// against the JSON object's key list.
///
/// Audit checklist when adding a new u64 field to the SDK: drop its
/// camelCase name here so the napi-rs bindings keep the BigInt contract.
const BIGINT_FIELDS: &[&str] = &[
    // overhead + overhead_trim
    "tokens",
    "bytes",
    "totalLines",
    "sessionCount",
    "startLine",
    "endLine",
    "filesAnalyzed",
    "filesWithRecommendations",
    "totalRecommendations",
    "tokensPerSession",
    // hotspots aggregations
    "callCount",
    "distinctCommands",
    "ridingTurns",
    "firstEmitTurnIndex",
    "toolCallCount",
    "turnsAnalyzed",
    "analyzed",
    "excluded",
    // compare
    "analyzedTurns",
    "minSample",
    "turns",
    "editTurns",
    "oneShotTurns",
    "pricedTurns",
    "total",
    "aggregateOnly",
    "costOnly",
    "partial",
    "usageOnly",
    "unknown",
    // measureSession
    "turnCount",
    "inputTokens",
    "outputTokens",
    "cacheReadTokens",
    "cacheWriteTokens",
    "reasoningTokens",
    "totalTokens",
    "costUsdMicros",
    // export_ledger / export_stamps record bodies — every camelCased
    // u64 field on TurnRecord / UserTurnRecord / ToolResultEventRecord /
    // CompactionEvent / nested Usage and ToolCall payloads. These values
    // already round-trip as u64 inside the SDK; without explicit
    // promotion the serde-json bridge emits them as JS `number` (f64)
    // and silently truncates anything above 2^53 when crossing the
    // napi boundary.
    "turnIndex",
    "eventIndex",
    "callIndex",
    "contentLength",
    "tokensBeforeCompact",
    "byteLen",
    "approxTokens",
    "retries",
    "collapsedCalls",
    // nested `usage` shape on TurnRecord / ToolResultEventRecord —
    // every field is u64, all six need promotion.
    "input",
    "output",
    "reasoning",
    "cacheRead",
    "cacheCreate5m",
    "cacheCreate1h",
    // span-tree attribute keys: untagged `AttrValue::Int` serializes as a
    // JSON number under the raw attribute name (dots included).
    "tokens.input",
    "tokens.output",
    "tokens.cache_read",
    "tokens.cache_write",
    "tokens.reasoning",
    // flow-graph `TurnTokens` + context-delta counters
    "cacheWrite",
    "priorContextTokens",
    "currentContextTokens",
    "deltaTokens",
    "approxBytes",
    "tokensFreed",
];

pub(crate) fn is_bigint_field(name: &str) -> bool {
    BIGINT_FIELDS.contains(&name)
}

/// Whether a numeric leaf under `key` crosses as `BigInt`.
fn promotes_to_bigint(key: Option<&str>, in_subtree: bool) -> bool {
    in_subtree || key.is_some_and(is_bigint_field)
}

/// Whether the children of `key` sit inside a [`BIGINT_SUBTREES`] subtree.
fn enters_bigint_subtree(in_subtree: bool, key: &str) -> bool {
    in_subtree || BIGINT_SUBTREES.contains(&key)
}

/// Keys whose subtrees carry integers only as `u64` counters, including maps
/// keyed by data values (fidelity `byClass`, `byGranularity`,
/// `missingCoverage`) that a field-name list cannot enumerate. Every unsigned
/// integer leaf under one of these keys is promoted; floats and strings pass
/// through. `report` / `timeseries` / `bucket` are the summary envelope bodies.
const BIGINT_SUBTREES: &[&str] = &["fidelity", "report", "timeseries", "bucket"];

/// Wraps a `serde_json::Value` so that, when napi-rs converts it to a JS
/// value, leaf u64 numbers under the [`BIGINT_FIELDS`] keys come out as
/// `BigInt` instead of `number`. Used for the `overhead`, `overheadTrim`,
/// `hotspots`, `compare`, `exportLedger`, and `exportStamps` verbs whose
/// result shapes are documented in `packages/sdk-node/src/index.d.ts`.
/// Also used by `turnSpanTree`, `sessionSpanTrees`, `flowGraph`, and
/// `contextDelta`.
pub struct BigIntPromoting(pub(crate) JsonValue);

impl ToNapiValue for BigIntPromoting {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> NapiResult<sys::napi_value> {
        promote_value(env, val.0, /*key=*/ None, /*in_subtree=*/ false)
    }
}

/// `in_subtree` is true below a [`BIGINT_SUBTREES`] key.
unsafe fn promote_value(
    env: sys::napi_env,
    val: JsonValue,
    key: Option<&str>,
    in_subtree: bool,
) -> NapiResult<sys::napi_value> {
    match val {
        JsonValue::Number(n) => {
            if let Some(u) = n.as_u64() {
                if promotes_to_bigint(key, in_subtree) {
                    return BigInt::to_napi_value(env, u64_to_bigint(u));
                }
            }
            // Fall back to napi-rs's default serde number conversion.
            serde_json::Number::to_napi_value(env, n)
        }
        JsonValue::Object(map) => {
            // Build a JS object, recursing per-value with the field name
            // so `is_bigint_field` can match.
            let mut obj: sys::napi_value = ptr::null_mut();
            napi::check_status!(
                sys::napi_create_object(env, &mut obj),
                "promote_value: napi_create_object"
            )?;
            for (k, v) in map.into_iter() {
                let child = promote_value(env, v, Some(&k), enters_bigint_subtree(in_subtree, &k))?;
                let key_buf = std::ffi::CString::new(k.as_str()).map_err(|e| {
                    NapiError::new(
                        napi::Status::GenericFailure,
                        format!("invalid object key (contains NUL): {e}"),
                    )
                })?;
                napi::check_status!(
                    sys::napi_set_named_property(env, obj, key_buf.as_ptr(), child),
                    "promote_value: napi_set_named_property"
                )?;
            }
            Ok(obj)
        }
        JsonValue::Array(arr) => {
            // Arrays don't carry a key context for their elements — the
            // outer object's key (e.g. `sections`) doesn't apply to each
            // element's leaf scalars; pass `None` so per-element
            // promotion is decided by the inner object's keys.
            let mut js_arr: sys::napi_value = ptr::null_mut();
            napi::check_status!(
                sys::napi_create_array_with_length(env, arr.len(), &mut js_arr),
                "promote_value: napi_create_array_with_length"
            )?;
            for (i, v) in arr.into_iter().enumerate() {
                let child = promote_value(env, v, /*key=*/ None, in_subtree)?;
                napi::check_status!(
                    sys::napi_set_element(env, js_arr, i as u32, child),
                    "promote_value: napi_set_element"
                )?;
            }
            Ok(js_arr)
        }
        // Booleans / strings / nulls — defer to napi-rs's standard
        // serde_json::Value conversion via the leaf wrappers.
        JsonValue::Bool(b) => bool::to_napi_value(env, b),
        JsonValue::String(s) => String::to_napi_value(env, s),
        JsonValue::Null => {
            napi::bindgen_prelude::Null::to_napi_value(env, napi::bindgen_prelude::Null)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_u64_fields_promote_outside_subtrees() {
        assert!(promotes_to_bigint(Some("tokens"), false));
        assert!(!promotes_to_bigint(Some("label"), false));
        assert!(!promotes_to_bigint(None, false));
    }

    #[test]
    fn every_leaf_promotes_inside_a_subtree() {
        assert!(promotes_to_bigint(Some("usage-only"), true));
        assert!(promotes_to_bigint(None, true));
    }

    #[test]
    fn subtree_keys_open_and_nested_keys_stay_inside() {
        for key in ["fidelity", "report", "timeseries", "bucket"] {
            assert!(enters_bigint_subtree(false, key), "{key}");
        }
        assert!(!enters_bigint_subtree(false, "byClass"));
        assert!(enters_bigint_subtree(true, "byClass"));
    }
}
