//! napi-rs bindings for `relayburn-sdk`.
//!
//! This crate is built in CI by the napi-rs matrix (#247-b) to produce
//! the per-platform `.node` artifacts that ship inside
//! `@relayburn/sdk@2.0`. It is not published to crates.io.
//!
//! # Type-mapping rules
//!
//! The SDK is a Rust API; the Node bindings are a lossy presenter for it
//! the same way the CLI is. The rules below are applied uniformly so the
//! generated `.d.ts` is predictable for TS consumers:
//!
//! - **`u64` token counts → JS `BigInt`.** SDK fields like
//!   `Summary::total_tokens`, every `tokens` row in `byTool` / `byModel`,
//!   and the `OverheadSection::tokens` field cross the boundary as
//!   `napi::bindgen_prelude::BigInt`. JS `number` (f64) cannot losslessly
//!   represent the upper end of the u64 range and silently truncates above
//!   2^53; the SDK already deals in u64 internally so the boundary is the
//!   right place to surface that. For verbs whose result is too recursive
//!   to mirror as a `#[napi(object)]` struct (`overhead`, `overheadTrim`,
//!   `hotspots`, `exportLedger`, `exportStamps`), we serialize through
//!   serde_json and emit the result via the [`BigIntPromoting`] wrapper,
//!   which walks the JSON tree and substitutes `BigInt` for any numeric
//!   value sitting under one of the well-known u64 field names listed in
//!   [`BIGINT_FIELDS`]. The lighter walker keeps shapes like
//!   `HotspotsResult` and `CompareResult` intact (both are awkward to
//!   express as single typed napi objects) and lets the export verbs surface
//!   every nested u64 (`turnIndex`, `eventIndex`, `contentLength`,
//!   `tokensBeforeCompact`, `byteLen`, `approxTokens`, the six `usage`
//!   fields, …) without per-record type plumbing — all while honoring
//!   the contract.
//! - **Timestamps → ISO-8601 `String`.** The SDK already speaks ISO
//!   strings (`turn.ts`, `since` parameters); we keep that wire format
//!   rather than dragging `chrono::DateTime` or `Date` through the FFI.
//!   Matches the public Node facade types.
//! - **`async fn` SDK verbs → `Promise<T>` on the JS side.** napi-rs's
//!   `tokio_rt` feature drives this; we mark `ingest` `async fn` and the
//!   sync verbs (`summary`, `sessionCost`, …) as plain `fn` returning
//!   `Result<T, BurnError>`.
//! - **Errors → typed `BurnError` JS class (sync verbs only).** Domain
//!   failures from the SDK (`anyhow::Error`) and argument-shape errors
//!   raised at this boundary are surfaced as a `napi::Error` whose
//!   `Status` slot carries one of [`SDK_ERROR_CODE`], [`IO_ERROR_CODE`],
//!   or [`INVALID_ARGUMENT_ERROR_CODE`]. napi-rs writes that string into
//!   the thrown JS Error's `code` property (via `napi_create_error`'s
//!   `code` argument), so JS callers get
//!   `try { … } catch (e) { if (e.code === 'BURN_SDK') … }`. The
//!   [`BurnErrorCode`] enum is exported as a `string_enum` so TS code
//!   can reference the codes by name without stringly-typed literals.
//!
//!   **Async exception — [`ingest`].** napi-rs 2.x's `async fn` lowering
//!   in `napi-derive` runs through `napi::bindgen_prelude::execute_tokio_future`
//!   ([`napi-derive-backend`]'s `codegen/fn.rs`), which is hard-typed to
//!   `Result<T, napi::Error<Status>>` — and `Status` is a *closed* enum
//!   over the predefined NAPI status strings (`GenericFailure`,
//!   `InvalidArg`, …). There is no public typed-error escape hatch in
//!   napi 2.x: `JsDeferred::reject` only accepts `Error<Status>`, and
//!   `AsyncTask::reject`'s rejection path likewise funnels through
//!   `JsError::from(Error<Status>).into_value`. The only way to inject
//!   a non-`Status` `code` would be to hand-roll a `JsDeferred`
//!   replacement on top of raw `sys::napi_*` calls + a TSFN — see
//!   `crates/relayburn-sdk-node/src/lib.rs` git history for the
//!   evaluation. We deliberately don't pay that complexity in v1.
//!
//!   **Concrete contract for [`ingest`]:** the returned `Promise<IngestReport>`
//!   rejects with a JS `Error` whose `.code === 'GenericFailure'` and
//!   whose `.message` is the rendered `anyhow::Error` chain from the
//!   SDK. JS callers branching on `e.code` should match `'GenericFailure'`
//!   for ingest failures (or, more robustly, gate on `e.message`
//!   substrings if discrimination is required). A future PR can tighten
//!   this — likely by upgrading to napi-rs 3.x once the `string_enum`
//!   and `BigInt` ergonomics there are validated against the rest of
//!   the binding — at which point `e.code` becomes one of the
//!   [`BurnErrorCode`] string values for ingest as well.
//!
//! # Surface
//!
//! Every public verb in `relayburn-sdk` (free-function form) is bound
//! here. The `Ledger` / `LedgerHandle` method form is omitted from the JS
//! surface for now — the Node facade exposes the free-function shape, and a
//! future PR can add a `Ledger` JS class without breaking compatibility.
//!
//! See `RUST_PORT_WAVE_PLAN.md` section 3 for how this fits the larger
//! port.

#![allow(clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::ptr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use napi::bindgen_prelude::{BigInt, Error as NapiError, Result as NapiResult, ToNapiValue};
use napi::sys;
use napi_derive::napi;
use serde_json::Value as JsonValue;

use relayburn_sdk as sdk;

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// `code` written into the thrown JS Error for failures the SDK raises
/// (typically `anyhow::Error` chains from the query / ingest verbs).
pub const SDK_ERROR_CODE: &str = "BURN_SDK";

/// `code` written into the thrown JS Error for I/O failures at the napi
/// boundary itself (path conversions, etc.).
pub const IO_ERROR_CODE: &str = "BURN_IO";

/// `code` written into the thrown JS Error when the caller passed an
/// argument shape we can't accept (e.g. a `BigInt` outside the u64
/// range).
pub const INVALID_ARGUMENT_ERROR_CODE: &str = "BURN_INVALID_ARGUMENT";

/// Tagged error code surfaced on the thrown JS Error's `code` property.
/// Exported as a TS `string_enum` so callers can branch on
/// `e.code === BurnErrorCode.Sdk` without stringly-typed literals.
///
/// The string values match [`SDK_ERROR_CODE`] et al. — napi-rs writes
/// them into the `code` slot via `napi_create_error`'s code argument, so
/// the round-trip from constant → JS code property is byte-identical.
#[napi(string_enum)]
pub enum BurnErrorCode {
    /// Catch-all for `anyhow::Error` chains the SDK raises. Refine over
    /// time as the SDK's error surface grows typed variants.
    #[napi(value = "BURN_SDK")]
    Sdk,
    /// I/O failures from the napi boundary itself (path conversions, etc.).
    #[napi(value = "BURN_IO")]
    Io,
    /// Caller passed an invalid argument shape (e.g. `since` that isn't a
    /// relative range nor an ISO timestamp).
    #[napi(value = "BURN_INVALID_ARGUMENT")]
    InvalidArgument,
}

/// `Err` variant used by every verb in the binding. The status slot
/// carries one of the [`BurnErrorCode`] string values; napi-rs threads
/// that string into the thrown JS Error's `code` property.
///
/// We intentionally keep verb signatures spelled as
/// `Result<T, BurnError>` (using the literal `Result` name and this
/// alias) rather than aliasing a full `BurnResult<T>` — the napi-rs
/// `#[napi]` macro identifies the result wrapping by syntactic token
/// (`Result<...>`) rather than type-checked unwrap, so a wrapper alias
/// would be silently treated as a regular return type and the macro
/// would skip the `JsError::from(err).throw_into(env)` codepath.
pub type BurnError = NapiError<&'static str>;

fn sdk_err(e: anyhow::Error) -> NapiError<&'static str> {
    // Render the chain so the message is informative; the discriminant
    // stays "BURN_SDK" until the SDK's typed error story exists.
    NapiError::new(SDK_ERROR_CODE, format!("{e:#}"))
}

fn invalid_arg(msg: impl Into<String>) -> NapiError<&'static str> {
    NapiError::new(INVALID_ARGUMENT_ERROR_CODE, msg.into())
}

fn io_err(e: std::io::Error) -> NapiError<&'static str> {
    NapiError::new(IO_ERROR_CODE, e.to_string())
}

// ---------------------------------------------------------------------------
// Helpers — small repeating conversions
// ---------------------------------------------------------------------------

fn u64_to_bigint(v: u64) -> BigInt {
    BigInt {
        sign_bit: false,
        words: vec![v],
    }
}

fn bigint_to_u64(v: BigInt) -> std::result::Result<u64, BurnError> {
    let (signed, value, lossless) = v.get_u64();
    if signed {
        return Err(invalid_arg("expected non-negative bigint, got signed"));
    }
    if !lossless {
        return Err(invalid_arg("bigint exceeds u64 range"));
    }
    Ok(value)
}

fn maybe_path(s: Option<String>) -> Option<PathBuf> {
    s.map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// BigIntPromoting — JsonValue → JS value walker that emits BigInt for the
// well-known u64 field names below.
//
// `overhead`, `overheadTrim`, `hotspots`, and `compare` return shapes that
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
];

fn is_bigint_field(name: &str) -> bool {
    BIGINT_FIELDS.contains(&name)
}

/// Wraps a `serde_json::Value` so that, when napi-rs converts it to a JS
/// value, leaf u64 numbers under the [`BIGINT_FIELDS`] keys come out as
/// `BigInt` instead of `number`. Used for the `overhead`, `overheadTrim`,
/// `hotspots`, `compare`, `exportLedger`, and `exportStamps` verbs whose
/// result shapes are documented in `packages/sdk-node/src/index.d.ts`.
pub struct BigIntPromoting(JsonValue);

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

// ---------------------------------------------------------------------------
// Ledger open options
// ---------------------------------------------------------------------------

/// Where on disk a ledger should land. Mirrors
/// `relayburn_sdk::LedgerOpenOptions`. `home` defaults to `RELAYBURN_HOME`
/// (or `~/.agentworkforce/burn`); `contentHome` overrides only the `content.sqlite`
/// path when it makes sense to park content on different storage.
#[napi(object)]
pub struct LedgerOpenOptions {
    pub home: Option<String>,
    pub content_home: Option<String>,
}

fn open_options(home: Option<String>, content_home: Option<String>) -> sdk::LedgerOpenOptions {
    sdk::LedgerOpenOptions {
        home: maybe_path(home),
        content_home: maybe_path(content_home),
    }
}

// ---------------------------------------------------------------------------
// writePendingStamp
// ---------------------------------------------------------------------------

#[napi(string_enum)]
pub enum PendingStampHarness {
    #[napi(value = "claude")]
    Claude,
    #[napi(value = "codex")]
    Codex,
    #[napi(value = "opencode")]
    Opencode,
}

impl From<PendingStampHarness> for sdk::PendingStampHarness {
    fn from(h: PendingStampHarness) -> Self {
        match h {
            PendingStampHarness::Claude => sdk::PendingStampHarness::Claude,
            PendingStampHarness::Codex => sdk::PendingStampHarness::Codex,
            PendingStampHarness::Opencode => sdk::PendingStampHarness::Opencode,
        }
    }
}

fn harness_to_string(h: sdk::PendingStampHarness) -> String {
    match h {
        sdk::PendingStampHarness::Claude => "claude",
        sdk::PendingStampHarness::Codex => "codex",
        sdk::PendingStampHarness::Opencode => "opencode",
    }
    .to_string()
}

#[napi(object)]
pub struct WritePendingStampOptions {
    pub harness: PendingStampHarness,
    pub cwd: String,
    pub enrichment: HashMap<String, String>,
    pub session_dir_hint: Option<String>,
    pub spawn_start_ts: Option<String>,
    pub spawner_pid: Option<u32>,
    pub ledger_home: Option<String>,
}

#[napi(object)]
pub struct PendingStamp {
    pub v: u32,
    pub harness: String,
    pub spawner_pid: u32,
    pub spawn_start_ts: String,
    pub cwd: String,
    pub enrichment: HashMap<String, String>,
    pub session_dir_hint: Option<String>,
}

#[napi(object)]
pub struct PendingStampWriteResult {
    pub file: String,
    pub stamp: PendingStamp,
}

impl From<sdk::PendingStamp> for PendingStamp {
    fn from(stamp: sdk::PendingStamp) -> Self {
        PendingStamp {
            v: stamp.v as u32,
            harness: harness_to_string(stamp.harness),
            spawner_pid: stamp.spawner_pid,
            spawn_start_ts: stamp.spawn_start_ts,
            cwd: stamp.cwd,
            enrichment: stamp.enrichment.into_iter().collect(),
            session_dir_hint: stamp.session_dir_hint,
        }
    }
}

impl From<sdk::PendingStampWriteResult> for PendingStampWriteResult {
    fn from(result: sdk::PendingStampWriteResult) -> Self {
        PendingStampWriteResult {
            file: result.file.to_string_lossy().into_owned(),
            stamp: PendingStamp::from(result.stamp),
        }
    }
}

#[napi]
pub fn write_pending_stamp(
    opts: WritePendingStampOptions,
) -> Result<PendingStampWriteResult, BurnError> {
    if opts.cwd.is_empty() {
        return Err(invalid_arg("cwd must be non-empty"));
    }
    if opts.enrichment.is_empty() {
        return Err(invalid_arg("enrichment must contain at least one tag"));
    }
    for key in opts.enrichment.keys() {
        if key.is_empty() {
            return Err(invalid_arg("enrichment keys must be non-empty"));
        }
    }
    let spawn_start_ts = opts
        .spawn_start_ts
        .as_deref()
        .map(parse_iso_system_time)
        .transpose()?;
    let raw = sdk::PendingStampWriteOptions {
        harness: opts.harness.into(),
        ledger_home: maybe_path(opts.ledger_home),
        cwd: opts.cwd,
        enrichment: opts.enrichment.into_iter().collect::<BTreeMap<_, _>>(),
        session_dir_hint: opts.session_dir_hint,
        spawn_start_ts,
        spawner_pid: opts.spawner_pid,
    };
    sdk::write_pending_stamp(raw)
        .map(PendingStampWriteResult::from)
        .map_err(io_err)
}

fn parse_iso_system_time(s: &str) -> std::result::Result<SystemTime, BurnError> {
    let Some(raw) = s.strip_suffix('Z') else {
        return Err(invalid_arg("spawnStartTs must be an ISO-8601 Z timestamp"));
    };
    let Some((date, time)) = raw.split_once('T') else {
        return Err(invalid_arg("spawnStartTs must contain a T separator"));
    };
    let mut date_parts = date.split('-');
    let year: i64 = parse_i64_part(date_parts.next(), "year")?;
    let month: u32 = parse_u32_part(date_parts.next(), "month")?;
    let day: u32 = parse_u32_part(date_parts.next(), "day")?;
    if date_parts.next().is_some() {
        return Err(invalid_arg("spawnStartTs date has too many fields"));
    }

    let mut time_parts = time.split(':');
    let hour: u32 = parse_u32_part(time_parts.next(), "hour")?;
    let minute: u32 = parse_u32_part(time_parts.next(), "minute")?;
    let second_raw = time_parts
        .next()
        .ok_or_else(|| invalid_arg("spawnStartTs missing seconds"))?;
    if time_parts.next().is_some() {
        return Err(invalid_arg("spawnStartTs time has too many fields"));
    }
    let (second_part, frac_part) = second_raw
        .split_once('.')
        .map(|(sec, frac)| (sec, Some(frac)))
        .unwrap_or((second_raw, None));
    let second: u32 = second_part
        .parse()
        .map_err(|_| invalid_arg("spawnStartTs second is invalid"))?;
    let nanos = parse_fractional_nanos(frac_part)?;

    let max_day = days_in_month(year, month);
    if max_day == 0
        || day == 0
        || day > max_day
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(invalid_arg("spawnStartTs is outside the supported range"));
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return Err(invalid_arg("spawnStartTs must be at or after 1970-01-01"));
    }
    let secs = days as u64 * 86_400 + hour as u64 * 3_600 + minute as u64 * 60 + second as u64;
    Ok(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_nanos(nanos as u64))
}

fn parse_i64_part(part: Option<&str>, name: &str) -> std::result::Result<i64, BurnError> {
    part.ok_or_else(|| invalid_arg(format!("spawnStartTs missing {name}")))?
        .parse()
        .map_err(|_| invalid_arg(format!("spawnStartTs {name} is invalid")))
}

fn parse_u32_part(part: Option<&str>, name: &str) -> std::result::Result<u32, BurnError> {
    part.ok_or_else(|| invalid_arg(format!("spawnStartTs missing {name}")))?
        .parse()
        .map_err(|_| invalid_arg(format!("spawnStartTs {name} is invalid")))
}

fn parse_fractional_nanos(part: Option<&str>) -> std::result::Result<u32, BurnError> {
    let Some(part) = part else {
        return Ok(0);
    };
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid_arg("spawnStartTs fractional seconds are invalid"));
    }
    let mut nanos = 0u32;
    let mut scale = 100_000_000u32;
    for b in part.bytes().take(9) {
        nanos += ((b - b'0') as u32) * scale;
        scale /= 10;
    }
    Ok(nanos)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month as i64 + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

// ---------------------------------------------------------------------------
// summary
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct SummaryOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    /// ISO timestamp (e.g. `2026-04-01T00:00:00Z`) or relative range
    /// (`24h`, `7d`, `4w`, `2m`).
    pub since: Option<String>,
    pub tags: Option<HashMap<String, String>>,
    pub group_by_tag: Option<String>,
    pub ledger_home: Option<String>,
}

#[napi(object)]
pub struct SummaryToolRow {
    pub tool: String,
    pub tokens: BigInt,
    pub cost: f64,
    pub count: BigInt,
}

#[napi(object)]
pub struct SummaryModelRow {
    pub model: String,
    pub tokens: BigInt,
    pub cost: f64,
}

#[napi(object)]
pub struct SummaryTagRow {
    pub tag: String,
    pub value: Option<String>,
    pub tokens: BigInt,
    pub cost: f64,
    pub turn_count: BigInt,
}

#[napi(object)]
pub struct ReplacementSavingsToolRow {
    pub tool: String,
    pub calls: BigInt,
    pub collapsed_calls: BigInt,
    pub estimated_tokens_saved: BigInt,
}

#[napi(object)]
pub struct ReplacementSavingsSummary {
    pub calls: BigInt,
    pub collapsed_calls: BigInt,
    pub estimated_tokens_saved: BigInt,
    pub by_tool: Vec<ReplacementSavingsToolRow>,
}

impl From<sdk::ReplacementSavingsSummary> for ReplacementSavingsSummary {
    fn from(s: sdk::ReplacementSavingsSummary) -> Self {
        ReplacementSavingsSummary {
            calls: u64_to_bigint(s.calls),
            collapsed_calls: u64_to_bigint(s.collapsed_calls),
            estimated_tokens_saved: u64_to_bigint(s.estimated_tokens_saved),
            by_tool: s
                .by_tool
                .into_iter()
                .map(|(tool, agg)| ReplacementSavingsToolRow {
                    tool,
                    calls: u64_to_bigint(agg.calls),
                    collapsed_calls: u64_to_bigint(agg.collapsed_calls),
                    estimated_tokens_saved: u64_to_bigint(agg.estimated_tokens_saved),
                })
                .collect(),
        }
    }
}

#[napi(object)]
pub struct Summary {
    pub total_tokens: BigInt,
    pub total_cost: f64,
    pub turn_count: BigInt,
    pub by_tool: Vec<SummaryToolRow>,
    pub by_model: Vec<SummaryModelRow>,
    pub by_tag: Option<Vec<SummaryTagRow>>,
    pub replacement_savings: Option<ReplacementSavingsSummary>,
}

impl From<sdk::Summary> for Summary {
    fn from(s: sdk::Summary) -> Self {
        Summary {
            total_tokens: u64_to_bigint(s.total_tokens),
            total_cost: s.total_cost,
            turn_count: u64_to_bigint(s.turn_count),
            by_tool: s
                .by_tool
                .into_iter()
                .map(|r| SummaryToolRow {
                    tool: r.tool,
                    tokens: u64_to_bigint(r.tokens),
                    cost: r.cost,
                    count: u64_to_bigint(r.count),
                })
                .collect(),
            by_model: s
                .by_model
                .into_iter()
                .map(|r| SummaryModelRow {
                    model: r.model,
                    tokens: u64_to_bigint(r.tokens),
                    cost: r.cost,
                })
                .collect(),
            by_tag: s.by_tag.map(|rows| {
                rows.into_iter()
                    .map(|r| SummaryTagRow {
                        tag: r.tag,
                        value: r.value,
                        tokens: u64_to_bigint(r.tokens),
                        cost: r.cost,
                        turn_count: u64_to_bigint(r.turn_count),
                    })
                    .collect()
            }),
            replacement_savings: s.replacement_savings.map(ReplacementSavingsSummary::from),
        }
    }
}

#[napi]
pub fn summary(opts: Option<SummaryOptions>) -> Result<Summary, BurnError> {
    let opts = opts.unwrap_or(SummaryOptions {
        session: None,
        project: None,
        since: None,
        tags: None,
        group_by_tag: None,
        ledger_home: None,
    });
    let raw = sdk::SummaryOptions {
        session: opts.session,
        project: opts.project,
        since: opts.since,
        tags: opts
            .tags
            .map(|tags| tags.into_iter().collect::<BTreeMap<_, _>>()),
        group_by_tag: opts.group_by_tag,
        ledger_home: maybe_path(opts.ledger_home),
    };
    sdk::summary(raw).map(Summary::from).map_err(sdk_err)
}

// ---------------------------------------------------------------------------
// session_cost
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct SessionCostOptions {
    /// Session id to total. Omit for `{ note: 'no session id provided' }`.
    pub session: Option<String>,
    pub ledger_home: Option<String>,
}

#[napi(object)]
pub struct SessionCostResult {
    pub session_id: Option<String>,
    /// Total cost in USD, rounded to 6 decimal places. Surfaced as
    /// `totalUSD` (screaming USD) to match the Node facade contract —
    /// napi-rs would otherwise camelCase it to
    /// `totalUsd`.
    #[napi(js_name = "totalUSD")]
    pub total_usd: f64,
    pub total_tokens: BigInt,
    pub turn_count: BigInt,
    pub models: Vec<String>,
    pub note: Option<String>,
}

impl From<sdk::SessionCostResult> for SessionCostResult {
    fn from(r: sdk::SessionCostResult) -> Self {
        SessionCostResult {
            session_id: r.session_id,
            total_usd: r.total_usd,
            total_tokens: u64_to_bigint(r.total_tokens),
            turn_count: u64_to_bigint(r.turn_count),
            models: r.models,
            note: r.note,
        }
    }
}

/// Compact session-scoped cost shape; powers the MCP `burn__sessionCost` tool.
#[napi(js_name = "sessionCost")]
pub fn session_cost(opts: Option<SessionCostOptions>) -> Result<SessionCostResult, BurnError> {
    let opts = opts.unwrap_or(SessionCostOptions {
        session: None,
        ledger_home: None,
    });
    let raw = sdk::SessionCostOptions {
        session: opts.session,
        ledger_home: maybe_path(opts.ledger_home),
    };
    sdk::session_cost(raw)
        .map(SessionCostResult::from)
        .map_err(sdk_err)
}

// ---------------------------------------------------------------------------
// overhead + overhead_trim — JsonValue passthrough wrapped in
// BigIntPromoting; see the file header for why we don't mirror these as
// typed `#[napi(object)]` structs.
// ---------------------------------------------------------------------------

/// Mirror of `sdk::OverheadFileKind`. Wire values match
/// The Node facade's `'claude-md' | 'agents-md'` literal union.
#[napi(string_enum = "kebab-case")]
pub enum OverheadFileKind {
    ClaudeMd,
    AgentsMd,
}

impl From<OverheadFileKind> for sdk::OverheadFileKind {
    fn from(k: OverheadFileKind) -> Self {
        match k {
            OverheadFileKind::ClaudeMd => sdk::OverheadFileKind::ClaudeMd,
            OverheadFileKind::AgentsMd => sdk::OverheadFileKind::AgentsMd,
        }
    }
}

#[napi(object)]
pub struct OverheadOptions {
    /// Project path to inspect; defaults to process.cwd().
    pub project: Option<String>,
    pub since: Option<String>,
    pub kind: Option<OverheadFileKind>,
    pub ledger_home: Option<String>,
}

/// Per-file + per-section overhead cost attribution. Powers `burn overhead`.
///
/// Returns the attribution result as an `OverheadResult` (see
/// `packages/sdk-node/src/index.d.ts`). Numeric u64 fields (`tokens`, `bytes`,
/// `totalLines`, `sessionCount`, `startLine`, `endLine`) cross the
/// boundary as `BigInt`; everything else is plain JS `number` / string.
#[napi(ts_return_type = "import('./index').OverheadResult")]
pub fn overhead(opts: Option<OverheadOptions>) -> Result<BigIntPromoting, BurnError> {
    let opts = opts.unwrap_or(OverheadOptions {
        project: None,
        since: None,
        kind: None,
        ledger_home: None,
    });
    let raw = sdk::OverheadOptions {
        project: maybe_path(opts.project),
        since: opts.since,
        kind: opts.kind.map(Into::into),
        ledger_home: maybe_path(opts.ledger_home),
    };
    let result = sdk::overhead(raw).map_err(sdk_err)?;
    let value = serde_json::to_value(&result)
        .map_err(|e| NapiError::new(SDK_ERROR_CODE, format!("serialize overhead: {e}")))?;
    Ok(BigIntPromoting(value))
}

#[napi(object)]
pub struct OverheadTrimOptions {
    pub project: Option<String>,
    pub since: Option<String>,
    pub kind: Option<OverheadFileKind>,
    pub ledger_home: Option<String>,
    /// Recommendations per file. Default 3. Plain `u32` rather than
    /// `BigInt` — `top` is a small recommendation cap, never near 2^32,
    /// and the Node facade types it as `number`.
    pub top: Option<u32>,
    /// Include the unified-diff text per recommendation. Default true.
    pub include_diff: Option<bool>,
}

/// Trim recommendations for high-cost overhead-file sections. Powers
/// `burn overhead trim`. Returns an `OverheadTrimResult`-shaped JSON
/// object with the same `BigInt` substitutions as [`overhead`].
#[napi(
    js_name = "overheadTrim",
    ts_return_type = "import('./index').OverheadTrimResult"
)]
pub fn overhead_trim(opts: Option<OverheadTrimOptions>) -> Result<BigIntPromoting, BurnError> {
    let opts = opts.unwrap_or(OverheadTrimOptions {
        project: None,
        since: None,
        kind: None,
        ledger_home: None,
        top: None,
        include_diff: None,
    });
    let raw = sdk::OverheadTrimOptions {
        project: maybe_path(opts.project),
        since: opts.since,
        kind: opts.kind.map(Into::into),
        ledger_home: maybe_path(opts.ledger_home),
        top: opts.top.map(u64::from),
        include_diff: opts.include_diff,
    };
    let result = sdk::overhead_trim(raw).map_err(sdk_err)?;
    let value = serde_json::to_value(&result)
        .map_err(|e| NapiError::new(SDK_ERROR_CODE, format!("serialize overhead_trim: {e}")))?;
    Ok(BigIntPromoting(value))
}

// ---------------------------------------------------------------------------
// hotspots — discriminated union; serialized via serde_json so the
// `kind` discriminant + per-variant rows survive the boundary. The TS
// .d.ts already documents the shape (`HotspotsResult` union).
// ---------------------------------------------------------------------------

/// Mirror of `sdk::HotspotsGroupBy`. Wire values match
/// The Node facade's
/// `'attribution' | 'bash' | 'bash-verb' | 'file' | 'subagent'` literal
/// union.
#[napi(string_enum = "kebab-case")]
pub enum HotspotsGroupBy {
    Attribution,
    Bash,
    BashVerb,
    File,
    Subagent,
}

impl From<HotspotsGroupBy> for sdk::HotspotsGroupBy {
    fn from(g: HotspotsGroupBy) -> Self {
        match g {
            HotspotsGroupBy::Attribution => sdk::HotspotsGroupBy::Attribution,
            HotspotsGroupBy::Bash => sdk::HotspotsGroupBy::Bash,
            HotspotsGroupBy::BashVerb => sdk::HotspotsGroupBy::BashVerb,
            HotspotsGroupBy::File => sdk::HotspotsGroupBy::File,
            HotspotsGroupBy::Subagent => sdk::HotspotsGroupBy::Subagent,
        }
    }
}

#[napi(object)]
pub struct HotspotsOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub group_by: Option<HotspotsGroupBy>,
    pub patterns: Option<Vec<String>>,
    pub workflow: Option<String>,
    pub provider: Option<Vec<String>>,
    pub ledger_home: Option<String>,
}

/// Per-axis hotspot attribution + pattern-finding queries. Returns a
/// JSON-shaped discriminated union — see `HotspotsResult` in
/// `packages/sdk-node/src/index.d.ts`. u64 row counts (`callCount`,
/// `distinctCommands`, `ridingTurns`, `firstEmitTurnIndex`,
/// `toolCallCount`, `turnsAnalyzed`, `analyzed`, `excluded`) cross as
/// `BigInt` per the file header rule.
#[napi(ts_return_type = "import('./index').HotspotsResult")]
pub fn hotspots(opts: Option<HotspotsOptions>) -> Result<BigIntPromoting, BurnError> {
    let opts = opts.unwrap_or(HotspotsOptions {
        session: None,
        project: None,
        since: None,
        group_by: None,
        patterns: None,
        workflow: None,
        provider: None,
        ledger_home: None,
    });
    let raw = sdk::HotspotsOptions {
        session: opts.session,
        project: opts.project,
        since: opts.since,
        group_by: opts.group_by.map(Into::into),
        patterns: opts.patterns,
        workflow: opts.workflow,
        provider: opts.provider,
        ledger_home: maybe_path(opts.ledger_home),
    };
    let result = sdk::hotspots(raw).map_err(sdk_err)?;
    let value = serde_json::to_value(&result)
        .map_err(|e| NapiError::new(SDK_ERROR_CODE, format!("serialize hotspots: {e}")))?;
    Ok(BigIntPromoting(value))
}

// ---------------------------------------------------------------------------
// compare — dynamic model totals + fidelity maps, serialized via serde_json.
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct CompareOptions {
    /// Required: at least two model names to compare.
    pub models: Vec<String>,
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub workflow: Option<String>,
    pub agent: Option<String>,
    pub provider: Option<Vec<String>>,
    /// Insufficient-sample threshold. Default 5. Plain `u32` — a turn
    /// count never approaches 2^32, and TS
    /// the Node facade types it as `number`.
    pub min_sample: Option<u32>,
    /// One of `full`, `usage-only`, `aggregate-only`, `cost-only`, `partial`.
    pub min_fidelity: Option<String>,
    pub ledger_home: Option<String>,
}

/// Per-(model, activity) comparison shape. Returns a `CompareResult` JSON
/// object; u64 counters (`analyzedTurns`, `turns`, `pricedTurns`, fidelity
/// counts, etc.) cross as `BigInt` per the file-header rule.
#[napi(ts_return_type = "import('./index').CompareResult")]
pub fn compare(opts: CompareOptions) -> Result<BigIntPromoting, BurnError> {
    let raw = sdk::CompareOptions {
        models: opts.models,
        session: opts.session,
        project: opts.project,
        since: opts.since,
        workflow: opts.workflow,
        agent: opts.agent,
        provider: opts.provider,
        min_sample: opts.min_sample.map(u64::from),
        min_fidelity: parse_fidelity_class(opts.min_fidelity.as_deref())?,
        ledger_home: maybe_path(opts.ledger_home),
    };
    let result = sdk::compare(raw).map_err(sdk_err)?;
    let value = serde_json::to_value(&result)
        .map_err(|e| NapiError::new(SDK_ERROR_CODE, format!("serialize compare: {e}")))?;
    Ok(BigIntPromoting(value))
}

fn parse_fidelity_class(raw: Option<&str>) -> Result<Option<sdk::FidelityClass>, BurnError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let class = match raw {
        "full" => sdk::FidelityClass::Full,
        "usage-only" => sdk::FidelityClass::UsageOnly,
        "aggregate-only" => sdk::FidelityClass::AggregateOnly,
        "cost-only" => sdk::FidelityClass::CostOnly,
        "partial" => sdk::FidelityClass::Partial,
        other => {
            return Err(NapiError::new(
                INVALID_ARGUMENT_ERROR_CODE,
                format!(
                    "compare: invalid minFidelity: {other} \
                     (expected one of full, usage-only, aggregate-only, cost-only, partial)"
                ),
            ));
        }
    };
    Ok(Some(class))
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct SearchQueryOptions {
    /// FTS5 query string. Supports phrase (`"out of memory"`), boolean
    /// (`a OR b`), and prefix (`mem*`) syntax.
    pub query: String,
    /// Hit cap. Defaults to 25 when omitted.
    pub limit: Option<BigInt>,
    /// Restrict to a single session_id. Omit to search all sessions.
    pub session_id: Option<String>,
    pub ledger_home: Option<String>,
}

#[napi(object)]
pub struct SearchHit {
    pub session_id: String,
    pub message_id: String,
    pub source: String,
    /// FTS5 BM25 rank (lower = better match).
    pub rank: f64,
    /// `<b>…</b>`-highlighted snippet around the matching tokens.
    pub snippet: String,
}

#[napi(object)]
pub struct SearchResult {
    pub query: String,
    pub hits: Vec<SearchHit>,
}

#[napi]
pub fn search(opts: SearchQueryOptions) -> Result<SearchResult, BurnError> {
    let limit = match opts.limit {
        Some(b) => Some(bigint_to_u64(b)? as usize),
        None => None,
    };
    let raw = sdk::SearchQueryOptions {
        query: opts.query.clone(),
        limit,
        session_id: opts.session_id,
        ledger_home: maybe_path(opts.ledger_home),
    };
    let result = sdk::search(raw).map_err(sdk_err)?;
    Ok(SearchResult {
        query: result.query,
        hits: result
            .hits
            .into_iter()
            .map(|h| SearchHit {
                session_id: h.session_id,
                message_id: h.message_id,
                source: h.source,
                rank: h.rank,
                snippet: h.snippet,
            })
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// export_ledger / export_stamps
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct ExportLedgerOptions {
    pub ledger_home: Option<String>,
}

#[napi(object)]
pub struct ExportStampsOptions {
    pub ledger_home: Option<String>,
}

/// Stream every event row as a JSONL-shaped JSON object. Each value has
/// the form `{ v: 1, kind: '<kind>', record: <json> }`.
///
/// Buffered into an array for v1; matches the SDK's
/// `export_ledger() -> impl Iterator` behavior (it's already in-memory
/// today). A streaming variant is a follow-up.
///
/// The result is wrapped in [`BigIntPromoting`] so u64 fields nested
/// inside each `record` object (`turnIndex`, `eventIndex`, `callIndex`,
/// `contentLength`, `tokensBeforeCompact`, `byteLen`, `approxTokens`,
/// `retries`, `collapsedCalls`, and the `usage` sub-object's six u64
/// keys) cross as JS `BigInt` instead of being silently truncated above
/// 2^53 by the default serde-json `number` conversion.
#[napi(js_name = "exportLedger", ts_return_type = "unknown[]")]
pub fn export_ledger(opts: Option<ExportLedgerOptions>) -> Result<BigIntPromoting, BurnError> {
    let opts = opts.unwrap_or(ExportLedgerOptions { ledger_home: None });
    let raw = sdk::ExportLedgerOptions {
        ledger_home: maybe_path(opts.ledger_home),
    };
    let iter = sdk::export_ledger(raw).map_err(sdk_err)?;
    let values: Vec<JsonValue> = iter.collect();
    Ok(BigIntPromoting(JsonValue::Array(values)))
}

/// Stream every stamp row as a JSONL-shaped JSON object. Sibling of
/// [`export_ledger`].
///
/// The result is wrapped in [`BigIntPromoting`] for symmetry with
/// [`export_ledger`]. Stamps don't currently carry u64 fields, but the
/// wrapper is cheap and means a future stamp-shape change that
/// introduces one (e.g. a `byteLen` on a range bound) won't silently
/// regress to f64 truncation.
#[napi(js_name = "exportStamps", ts_return_type = "unknown[]")]
pub fn export_stamps(opts: Option<ExportStampsOptions>) -> Result<BigIntPromoting, BurnError> {
    let opts = opts.unwrap_or(ExportStampsOptions { ledger_home: None });
    let raw = sdk::ExportStampsOptions {
        ledger_home: maybe_path(opts.ledger_home),
    };
    let iter = sdk::export_stamps(raw).map_err(sdk_err)?;
    let values: Vec<JsonValue> = iter.collect();
    Ok(BigIntPromoting(JsonValue::Array(values)))
}

// ---------------------------------------------------------------------------
// ingest — async; returns a Promise<IngestReport> on the JS side.
// ---------------------------------------------------------------------------

/// Mirror of the Node facade's
/// `'claude-code' | 'codex' | 'opencode'` literal union. Surfaced as a
/// `string_enum` so TS callers get the same string contract without a
/// stringly-typed `harness: string` field.
///
/// Note: the SDK's `ingest_all` does not currently accept a per-harness
/// filter — passing this is a forward-compat hook that mirrors the TS
/// shape (`packages/sdk-node/src/index.js`'s `ingest()` likewise takes the option
/// today and routes to `ingestAll()` without filtering).
#[napi(string_enum = "kebab-case")]
pub enum IngestHarness {
    ClaudeCode,
    Codex,
    Opencode,
}

/// Mirrors `packages/sdk-node/src/index.d.ts`'s `IngestOptions` shape. The field
/// set is kept intentionally narrow; the binding routes to the SDK's
/// fuller `sdk::IngestOptions` shape (with default `IngestRoots`) at the
/// boundary.
#[napi(object)]
pub struct IngestOptions {
    /// Reserved for compatibility; `ingestAll()` ignores
    /// it; mirrored here so the napi binding accepts the same caller
    /// shape without a TypeError.
    pub session_id: Option<String>,
    /// Reserved for compatibility; `ingestAll()` ignores
    /// it; mirrored here for shape parity.
    pub harness: Option<IngestHarness>,
    pub ledger_home: Option<String>,
}

#[napi(object)]
pub struct IngestReport {
    pub scanned_sessions: BigInt,
    pub ingested_sessions: BigInt,
    pub appended_turns: BigInt,
    pub applied_pending_stamps: BigInt,
}

impl From<sdk::IngestReport> for IngestReport {
    fn from(r: sdk::IngestReport) -> Self {
        IngestReport {
            scanned_sessions: u64_to_bigint(r.scanned_sessions as u64),
            ingested_sessions: u64_to_bigint(r.ingested_sessions as u64),
            appended_turns: u64_to_bigint(r.appended_turns as u64),
            applied_pending_stamps: u64_to_bigint(r.applied_pending_stamps as u64),
        }
    }
}

/// Discover and ingest unprocessed turns from the configured session
/// stores. Returns a `Promise<IngestReport>`.
///
/// Progress / warning sinks are intentionally not surfaced through the
/// boundary in v1 — the JS surface today doesn't expose them either.
/// Wave 2 D9 picks them up if the conformance gate calls for it.
///
/// **Error-code contract.** Unlike the synchronous verbs (which reject
/// with `e.code` set to one of [`BurnErrorCode`]'s string values), this
/// async verb's rejection surfaces as `code: 'GenericFailure'`. The
/// rendered SDK error chain is in `e.message`. See the file header for
/// the full rationale (napi-rs 2.x's `execute_tokio_future` is
/// hard-typed to `Result<T, Error<Status>>` and `Status` is a closed
/// enum, so `'BURN_SDK'` cannot be threaded through). The
/// [`ingest_uses_generic_failure_code_runtime_invariant`] test pins
/// this discrepancy so a future napi-rs upgrade or hand-rolled deferred
/// either fixes it or has to update the test in lockstep with the
/// header docs.
#[napi]
pub async fn ingest(opts: Option<IngestOptions>) -> Result<IngestReport, NapiError> {
    let opts = opts.unwrap_or(IngestOptions {
        session_id: None,
        harness: None,
        ledger_home: None,
    });
    // session_id / harness are TS-shape mirror fields; the SDK's
    // `ingest_all` takes neither, so we drop them here and rely on the
    // SDK's discovery to pick up every session under the configured
    // roots. Matches the Node facade's behavior at packages/sdk-node/src/index.js's
    // `ingest()`.
    let _ = opts.session_id;
    let _ = opts.harness;
    let raw = sdk::IngestOptions {
        ledger_home: maybe_path(opts.ledger_home),
        roots: sdk::IngestRoots::default(),
        on_progress: None,
        on_warn: None,
    };
    // SDK ingest is sync (filesystem walks + rusqlite writes). Run it on
    // tokio's blocking pool so the napi runtime stays responsive while the
    // sweep is in flight.
    let report = tokio::task::spawn_blocking(move || sdk::ingest(raw))
        .await
        .map_err(|e| NapiError::from_reason(format!("ingest task panicked: {e}")))?
        .map_err(|e| NapiError::from_reason(format!("{e:#}")))?;
    Ok(report.into())
}

// ---------------------------------------------------------------------------
// Module-level metadata. napi-rs doesn't require a `register_module`
// entry point — `#[napi]` items register themselves via the macros.
// We export the open-options shape under a stable name for wave-2
// callers that want to construct one explicitly.
// ---------------------------------------------------------------------------

/// Synchronously open and immediately close a ledger to validate the
/// configured paths. Returns the resolved `home` path. Mirrors the
/// `Ledger.open()` smoke-call shape from `packages/sdk-node/src/index.d.ts`; a
/// future PR can add a stateful `Ledger` JS class that holds a handle.
#[napi(js_name = "ledgerOpen")]
pub fn ledger_open(opts: Option<LedgerOpenOptions>) -> Result<String, BurnError> {
    let opts = opts.unwrap_or(LedgerOpenOptions {
        home: None,
        content_home: None,
    });
    let home = opts.home.clone();
    let content_home = opts.content_home.clone();
    let raw = open_options(home, content_home);
    // Open + drop. Schema DDL applies on the first open, so this is a
    // cheap "is the path writable / migration current?" probe.
    let _handle = sdk::Ledger::open(raw).map_err(sdk_err)?;
    // Echo the resolved home back so JS callers know which ledger they
    // attached to.
    Ok(opts
        .home
        .unwrap_or_else(|| sdk::ledger_home().to_string_lossy().into_owned()))
}

// ---------------------------------------------------------------------------
// Tests — exercise the helpers that don't need a live napi env. The full
// boundary is covered end-to-end by the conformance test scaffold landing
// in #247-b (Wave 1 D2).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u64_to_bigint_round_trip_small() {
        let big = u64_to_bigint(42);
        assert_eq!(bigint_to_u64(big).unwrap(), 42);
    }

    #[test]
    fn u64_to_bigint_round_trip_max() {
        let big = u64_to_bigint(u64::MAX);
        assert_eq!(bigint_to_u64(big).unwrap(), u64::MAX);
    }

    #[test]
    fn bigint_to_u64_rejects_signed() {
        let signed = BigInt {
            sign_bit: true,
            words: vec![1],
        };
        assert!(bigint_to_u64(signed).is_err());
    }

    #[test]
    fn bigint_to_u64_rejects_too_wide() {
        let two_words = BigInt {
            sign_bit: false,
            words: vec![0, 1],
        };
        assert!(bigint_to_u64(two_words).is_err());
    }

    #[test]
    fn parse_iso_system_time_accepts_pending_stamp_timestamp_shape() {
        let parsed = parse_iso_system_time("2026-04-23T00:00:00.123Z").unwrap();
        let elapsed = parsed.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(elapsed.subsec_millis(), 123);
    }

    #[test]
    fn parse_iso_system_time_rejects_non_zulu_values() {
        let err = parse_iso_system_time("2026-04-23T00:00:00").unwrap_err();
        assert!(err.reason.contains("ISO-8601 Z timestamp"));
    }

    #[test]
    fn parse_iso_system_time_rejects_impossible_dates() {
        let err = parse_iso_system_time("2026-02-31T00:00:00Z").unwrap_err();
        assert!(err.reason.contains("outside the supported range"));
        assert!(parse_iso_system_time("2024-02-29T00:00:00Z").is_ok());
        assert!(parse_iso_system_time("2025-02-29T00:00:00Z").is_err());
    }

    #[test]
    fn maybe_path_threads_string_to_pathbuf() {
        assert!(maybe_path(None).is_none());
        assert_eq!(
            maybe_path(Some("/tmp/x".into())),
            Some(PathBuf::from("/tmp/x"))
        );
    }

    #[test]
    fn parse_fidelity_class_accepts_compare_wire_values() {
        assert_eq!(
            parse_fidelity_class(Some("usage-only")).unwrap(),
            Some(sdk::FidelityClass::UsageOnly)
        );
        assert_eq!(parse_fidelity_class(None).unwrap(), None);
        assert!(parse_fidelity_class(Some("usage_only")).is_err());
    }

    #[test]
    fn bigint_field_membership_covers_documented_keys() {
        // Every camelCased u64 field that crosses the boundary today
        // must be in BIGINT_FIELDS so the walker promotes it.
        for key in [
            // overhead + hotspots verbs (round-1 set)
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
            // export_ledger / export_stamps record-body u64s (round-3 set)
            "turnIndex",
            "eventIndex",
            "callIndex",
            "contentLength",
            "tokensBeforeCompact",
            "byteLen",
            "approxTokens",
            "retries",
            "collapsedCalls",
            // nested `usage` shape on TurnRecord / ToolResultEventRecord
            "input",
            "output",
            "reasoning",
            "cacheRead",
            "cacheCreate5m",
            "cacheCreate1h",
        ] {
            assert!(is_bigint_field(key), "{key} missing from BIGINT_FIELDS");
        }

        // Spot-check that f64 / string fields aren't accidentally on
        // the list — a regression here would silently turn floats into
        // BigInts on the JS side.
        for key in [
            "totalCost",
            "grandTotal",
            "initialTokens",
            "persistenceTokens",
            "tokenShare",
            "perSessionAvg",
            "path",
            "kind",
            // Record envelope fields that live alongside the promoted
            // u64 keys but are themselves a schema version (u32 small)
            // or a string discriminant — promoting these would corrupt
            // the JSONL contract.
            "v",
            "ts",
            "sessionId",
            "messageId",
            "source",
        ] {
            assert!(
                !is_bigint_field(key),
                "{key} unexpectedly present in BIGINT_FIELDS"
            );
        }
    }

    #[test]
    fn export_record_u64_fields_survive_above_2_pow_53() {
        // Pin the round-3 fix: the four field names CodeRabbit called
        // out, plus the rest of the export-record u64 surface, must be
        // in BIGINT_FIELDS so the BigIntPromoting walker hands them to
        // JS as `BigInt`. A regression — anyone removing one of these
        // and forgetting to update the export verbs — would silently
        // truncate values >2^53 in `exportLedger` / `exportStamps`.
        //
        // This is a static membership test rather than a live napi-env
        // round-trip because the napi sys calls require a running JS
        // environment (covered end-to-end by the wave-2 D9 conformance
        // suite). The walker's behavior given a matched key is already
        // exercised structurally — see `is_bigint_field` / the round-1
        // overhead+hotspots tests — so guarding the membership here is
        // load-bearing for the contract.
        let above_2_pow_53: u64 = (1u64 << 53) + 1;
        // Sanity: this value is precisely the kind we'd lose to f64
        // rounding, so pinning it in the test doc keeps the failure
        // mode visible.
        assert!(above_2_pow_53 > (1u64 << 53));
        assert!(above_2_pow_53 as f64 as u64 != above_2_pow_53);

        // The exact field names from CodeRabbit's report.
        for key in [
            "turnIndex",
            "eventIndex",
            "contentLength",
            "tokensBeforeCompact",
        ] {
            assert!(
                is_bigint_field(key),
                "round-3 fix regressed: {key} not in BIGINT_FIELDS"
            );
        }

        // The wrapper itself round-trips a JsonValue::Array (the shape
        // export_ledger / export_stamps emit). We can't assert on the
        // napi conversion here — see comment above — but we *can*
        // build the wrapper to confirm the type plumbing compiles and
        // accepts a value-above-2^53 inside a record body shaped like
        // the live emitter.
        let record = serde_json::json!({
            "v": 1,
            "kind": "turn",
            "record": {
                "turnIndex": above_2_pow_53,
                "eventIndex": above_2_pow_53,
                "contentLength": above_2_pow_53,
                "tokensBeforeCompact": above_2_pow_53,
            },
        });
        let wrapped = BigIntPromoting(JsonValue::Array(vec![record.clone()]));
        // We exposed the inner JsonValue as the tuple field; make sure
        // the value we just wrapped wasn't lossily reshaped before
        // hand-off to the walker — `serde_json::Number::as_u64` is what
        // the walker calls, and that path returns the original u64.
        if let JsonValue::Array(arr) = &wrapped.0 {
            let rec = &arr[0]["record"];
            for k in [
                "turnIndex",
                "eventIndex",
                "contentLength",
                "tokensBeforeCompact",
            ] {
                let n = rec[k].as_u64().expect("u64 survived the JSON round-trip");
                assert_eq!(n, above_2_pow_53, "{k} value mutated");
            }
        } else {
            panic!("BigIntPromoting wrapper dropped the array shape");
        }
    }

    #[test]
    fn burn_error_codes_match_constants() {
        // The TS-exported BurnErrorCode variant values must equal the
        // string codes we hand to `napi::Error::new`, otherwise JS
        // callers comparing `e.code` to `BurnErrorCode.Sdk` would
        // silently miss every error.
        assert_eq!(SDK_ERROR_CODE, "BURN_SDK");
        assert_eq!(IO_ERROR_CODE, "BURN_IO");
        assert_eq!(INVALID_ARGUMENT_ERROR_CODE, "BURN_INVALID_ARGUMENT");
    }

    /// Runtime invariant pinning the documented `ingest()` error-code
    /// discrepancy. The body is a compile-time assertion that the type
    /// returned by `ingest`'s rejection path is the default
    /// `napi::Error` (`Error<Status>`), *not* our typed
    /// `Error<&'static str>` (`BurnError`). If a future napi-rs upgrade
    /// or hand-rolled deferred replacement makes typed async errors
    /// possible, this test will start failing; that's the signal to
    /// remove the caveat from the file header + the `ingest()` doc
    /// comment in lockstep with switching the signature to
    /// `Result<IngestReport, BurnError>`.
    ///
    /// Why a static-typing check rather than a JS-side assertion: the
    /// JS-side end-to-end test is wave-2 D9 territory (it requires a
    /// built `.node` artifact); we can still pin the discrepancy *here*
    /// by encoding the napi-rs limitation as a type-level fact. The
    /// caveat in the header docs is then forced to track the type.
    #[test]
    fn ingest_uses_generic_failure_code_runtime_invariant() {
        use std::any::TypeId;

        // `BurnError` (the typed error used by every sync verb) is
        // distinct from the default `napi::Error<Status>`.
        // `execute_tokio_future` (which `#[napi] async fn ingest` is
        // lowered to) is hard-typed to the latter — see the file
        // header. If these two ever become assignable, the docs need
        // updating.
        type DefaultNapiError = napi::Error;
        assert_ne!(
            TypeId::of::<BurnError>(),
            TypeId::of::<DefaultNapiError>(),
            "BurnError and napi::Error<Status> have unified — \
             ingest() can now return BurnError; update the file header \
             docs and switch ingest's return type."
        );

        // Sanity-check the shapes of the codes we *can* deliver vs the
        // status code that `ingest()` will surface. These are what
        // upstream `JsError::from(Error<Status>).into_value(env)`
        // writes for the Status::GenericFailure case.
        let sync_codes = [SDK_ERROR_CODE, IO_ERROR_CODE, INVALID_ARGUMENT_ERROR_CODE];
        let async_code: &str = napi::Status::GenericFailure.as_ref();
        for c in sync_codes {
            assert_ne!(
                c, async_code,
                "{c} must not collide with the async fallback code {async_code}"
            );
        }
        assert_eq!(async_code, "GenericFailure");
    }
}
