//! JSON results that cross to JS with their `u64` counters as `BigInt`.

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
pub(crate) const BIGINT_FIELDS: &[&str] = &[
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
    // measureSession + analyzeSession
    "turnCount",
    "userTurnCount",
    "toolCallCount",
    "compactionCount",
    "attributableTurns",
    "calls",
    "errors",
    "inferences",
    "toolUses",
    "skills",
    "rails",
    "edges",
    "peakContextTokens",
    "inputTokens",
    "outputTokens",
    "cacheReadTokens",
    "cacheWriteTokens",
    "reasoningTokens",
    "totalTokens",
    "costUsdMicros",
    "unpricedTurns",
    // analyzeSession subagent tree, flow summary, stop-reason counts
    "selfTurns",
    "selfTokens",
    "cumulativeTurns",
    "cumulativeTokens",
    "subagents",
    "endTurn",
    "maxTokens",
    "pauseTurn",
    "stopSequence",
    "toolUse",
    "refusal",
    "aborted",
    "silent",
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
        promote_value(env, val.0, /*key=*/ None)
    }
}

unsafe fn promote_value(
    env: sys::napi_env,
    val: JsonValue,
    key: Option<&str>,
) -> NapiResult<sys::napi_value> {
    match val {
        JsonValue::Number(n) => {
            if let (Some(k), Some(u)) = (key, n.as_u64()) {
                if is_bigint_field(k) {
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
                let child = promote_value(env, v, Some(&k))?;
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
                let child = promote_value(env, v, /*key=*/ None)?;
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
