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
//!   right place to surface that.
//! - **Timestamps → ISO-8601 `String`.** The SDK already speaks ISO
//!   strings (`turn.ts`, `since` parameters); we keep that wire format
//!   rather than dragging `chrono::DateTime` or `Date` through the FFI.
//!   Matches the existing `packages/sdk/index.d.ts` byte-for-byte.
//! - **`async fn` SDK verbs → `Promise<T>` on the JS side.** napi-rs's
//!   `tokio_rt` feature drives this; we mark `ingest` `async fn` and the
//!   sync verbs (`summary`, `sessionCost`, …) as plain `fn` returning
//!   `napi::Result<T>`.
//! - **Errors → typed `BurnError` JS class.** Domain failures from the
//!   SDK (`anyhow::Error`) are mapped into a tagged `BurnError` with a
//!   discriminant + message; no untyped `napi::Error` leaks. JS code can
//!   `instanceof BurnError` and switch on `err.code`.
//!
//! # Surface
//!
//! Every public verb in `relayburn-sdk` (free-function form) is bound
//! here. The `Ledger` / `LedgerHandle` method form is omitted from the JS
//! surface for v1 — the TS sibling `@relayburn/sdk@1.x` only exposes the
//! free-function shape, and a future PR can add a `Ledger` JS class
//! without breaking compatibility. The deferred D9 PR (Wave 2) checks in a
//! `.d.ts` snapshot test against `packages/sdk/index.d.ts`.
//!
//! See `RUST_PORT_WAVE_PLAN.md` section 3 for how this fits the larger
//! port.

#![allow(clippy::needless_pass_by_value)]

use std::path::PathBuf;

use napi::bindgen_prelude::{BigInt, Error as NapiError, Result as NapiResult, Status};
use napi_derive::napi;
use serde_json::Value as JsonValue;

use relayburn_sdk as sdk;

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Tagged error code surfaced on `BurnError.code`. Mirrors the lower-crate
/// failure axes today; future SDK errors should map here rather than
/// leaking as opaque strings.
#[napi(string_enum)]
pub enum BurnErrorCode {
    /// Catch-all for `anyhow::Error` chains the SDK raises. Refine over
    /// time as the SDK's error surface grows typed variants.
    Sdk,
    /// I/O failures from the napi boundary itself (path conversions, etc.).
    Io,
    /// Caller passed an invalid argument shape (e.g. `since` that isn't a
    /// relative range nor an ISO timestamp).
    InvalidArgument,
}

/// Domain error surfaced to JS callers. The `BurnError` JS class is a
/// thin Object with `{ code, message }`; the napi runtime turns it into a
/// real `Error` subclass at module-init time.
///
/// Keep this distinct from the JS `Error` napi-rs would synthesize for an
/// `Err(napi::Error)` — the two-tier design lets TS consumers
/// `try { ... } catch (e) { if (e instanceof BurnError) ... }` and switch
/// on `e.code`.
#[napi(object)]
pub struct BurnError {
    pub code: BurnErrorCode,
    pub message: String,
}

fn sdk_err(e: anyhow::Error) -> NapiError {
    // Render the chain so the message is informative; the discriminant
    // stays "Sdk" until the SDK's typed error story exists.
    NapiError::new(Status::GenericFailure, format!("{e:#}"))
}

fn invalid_arg(msg: impl Into<String>) -> NapiError {
    NapiError::new(Status::InvalidArg, msg.into())
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

fn bigint_to_u64(v: BigInt) -> NapiResult<u64> {
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
// Ledger open options
// ---------------------------------------------------------------------------

/// Where on disk a ledger should land. Mirrors
/// `relayburn_sdk::LedgerOpenOptions`. `home` defaults to `RELAYBURN_HOME`
/// (or `~/.relayburn`); `contentHome` overrides only the `content.sqlite`
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
// summary
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct SummaryOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    /// ISO timestamp (e.g. `2026-04-01T00:00:00Z`) or relative range
    /// (`24h`, `7d`, `4w`, `2m`).
    pub since: Option<String>,
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
pub struct Summary {
    pub total_tokens: BigInt,
    pub total_cost: f64,
    pub turn_count: BigInt,
    pub by_tool: Vec<SummaryToolRow>,
    pub by_model: Vec<SummaryModelRow>,
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
        }
    }
}

#[napi]
pub fn summary(opts: Option<SummaryOptions>) -> NapiResult<Summary> {
    let opts = opts.unwrap_or(SummaryOptions {
        session: None,
        project: None,
        since: None,
        ledger_home: None,
    });
    let raw = sdk::SummaryOptions {
        session: opts.session,
        project: opts.project,
        since: opts.since,
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
    /// Total cost in USD, rounded to 6 decimal places.
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
pub fn session_cost(opts: Option<SessionCostOptions>) -> NapiResult<SessionCostResult> {
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
// overhead + overhead_trim — passed through serde_json. The shapes are
// large and recursive (sections, attribution detail, applies-to harness
// arrays); rebuilding each as a `#[napi(object)]` struct here would be
// hundreds of lines of mechanical translation that D9's snapshot test
// will catch drift on regardless. The serde wire format already matches
// `packages/sdk/index.d.ts` modulo the `bigint` substitutions; D9 owns
// the `BigInt` upgrades for `tokens` / `bytes` / `totalLines` fields if
// the conformance gate flags them.
// ---------------------------------------------------------------------------

#[napi(string_enum)]
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
/// Returns the attribution result as a JSON-shaped object — the schema is
/// `OverheadResult` in `packages/sdk/index.d.ts`. D9's snapshot test
/// upgrades the `tokens` / `bytes` / `totalLines` fields to `BigInt` if
/// the conformance gate calls for it.
#[napi]
pub fn overhead(opts: Option<OverheadOptions>) -> NapiResult<JsonValue> {
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
        .map_err(|e| NapiError::new(Status::GenericFailure, format!("serialize overhead: {e}")))?;
    Ok(value)
}

#[napi(object)]
pub struct OverheadTrimOptions {
    pub project: Option<String>,
    pub since: Option<String>,
    pub kind: Option<OverheadFileKind>,
    pub ledger_home: Option<String>,
    /// Recommendations per file. Default 3.
    pub top: Option<BigInt>,
    /// Include the unified-diff text per recommendation. Default true.
    pub include_diff: Option<bool>,
}

/// Trim recommendations for high-cost overhead-file sections. Powers
/// `burn overhead trim`. Returns an `OverheadTrimResult`-shaped JSON
/// object; see the comment on [`overhead`] for the BigInt-upgrade plan.
#[napi(js_name = "overheadTrim")]
pub fn overhead_trim(opts: Option<OverheadTrimOptions>) -> NapiResult<JsonValue> {
    let opts = opts.unwrap_or(OverheadTrimOptions {
        project: None,
        since: None,
        kind: None,
        ledger_home: None,
        top: None,
        include_diff: None,
    });
    let top = match opts.top {
        Some(b) => Some(bigint_to_u64(b)?),
        None => None,
    };
    let raw = sdk::OverheadTrimOptions {
        project: maybe_path(opts.project),
        since: opts.since,
        kind: opts.kind.map(Into::into),
        ledger_home: maybe_path(opts.ledger_home),
        top,
        include_diff: opts.include_diff,
    };
    let result = sdk::overhead_trim(raw).map_err(sdk_err)?;
    let value = serde_json::to_value(&result).map_err(|e| {
        NapiError::new(Status::GenericFailure, format!("serialize overhead_trim: {e}"))
    })?;
    Ok(value)
}

// ---------------------------------------------------------------------------
// hotspots — discriminated union; serialized via serde_json so the
// `kind` discriminant + per-variant rows survive the boundary. The TS
// .d.ts already documents the shape (`HotspotsResult` union).
// ---------------------------------------------------------------------------

#[napi(string_enum)]
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
    pub ledger_home: Option<String>,
}

/// Per-axis hotspot attribution + pattern-finding queries. Returns a
/// JSON-shaped discriminated union — see `HotspotsResult` in
/// `packages/sdk/index.d.ts`.
#[napi]
pub fn hotspots(opts: Option<HotspotsOptions>) -> NapiResult<JsonValue> {
    let opts = opts.unwrap_or(HotspotsOptions {
        session: None,
        project: None,
        since: None,
        group_by: None,
        patterns: None,
        ledger_home: None,
    });
    let raw = sdk::HotspotsOptions {
        session: opts.session,
        project: opts.project,
        since: opts.since,
        group_by: opts.group_by.map(Into::into),
        patterns: opts.patterns,
        ledger_home: maybe_path(opts.ledger_home),
    };
    let result = sdk::hotspots(raw).map_err(sdk_err)?;
    let value = serde_json::to_value(&result)
        .map_err(|e| NapiError::new(Status::GenericFailure, format!("serialize hotspots: {e}")))?;
    Ok(value)
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
pub fn search(opts: SearchQueryOptions) -> NapiResult<SearchResult> {
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
#[napi(js_name = "exportLedger")]
pub fn export_ledger(opts: Option<ExportLedgerOptions>) -> NapiResult<Vec<JsonValue>> {
    let opts = opts.unwrap_or(ExportLedgerOptions { ledger_home: None });
    let raw = sdk::ExportLedgerOptions {
        ledger_home: maybe_path(opts.ledger_home),
    };
    let iter = sdk::export_ledger(raw).map_err(sdk_err)?;
    Ok(iter.collect())
}

/// Stream every stamp row as a JSONL-shaped JSON object. Sibling of
/// [`export_ledger`].
#[napi(js_name = "exportStamps")]
pub fn export_stamps(opts: Option<ExportStampsOptions>) -> NapiResult<Vec<JsonValue>> {
    let opts = opts.unwrap_or(ExportStampsOptions { ledger_home: None });
    let raw = sdk::ExportStampsOptions {
        ledger_home: maybe_path(opts.ledger_home),
    };
    let iter = sdk::export_stamps(raw).map_err(sdk_err)?;
    Ok(iter.collect())
}

// ---------------------------------------------------------------------------
// ingest — async; returns a Promise<IngestReport> on the JS side.
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct IngestRoots {
    /// `~/.claude/projects` override.
    pub claude_projects_dir: Option<String>,
    /// `~/.codex/sessions` override.
    pub codex_sessions_dir: Option<String>,
    /// `~/.local/share/opencode/storage` override.
    pub opencode_storage_dir: Option<String>,
}

#[napi(object)]
pub struct IngestOptions {
    pub ledger_home: Option<String>,
    pub roots: Option<IngestRoots>,
}

#[napi(object)]
pub struct IngestReport {
    pub scanned_sessions: BigInt,
    pub ingested_sessions: BigInt,
    pub appended_turns: BigInt,
}

impl From<sdk::IngestReport> for IngestReport {
    fn from(r: sdk::IngestReport) -> Self {
        IngestReport {
            scanned_sessions: u64_to_bigint(r.scanned_sessions as u64),
            ingested_sessions: u64_to_bigint(r.ingested_sessions as u64),
            appended_turns: u64_to_bigint(r.appended_turns as u64),
        }
    }
}

/// Discover and ingest unprocessed turns from the configured session
/// stores. Returns a `Promise<IngestReport>`.
///
/// Progress / warning sinks are intentionally not surfaced through the
/// boundary in v1 — the JS surface today doesn't expose them either.
/// Wave 2 D9 picks them up if the conformance gate calls for it.
#[napi]
pub async fn ingest(opts: Option<IngestOptions>) -> NapiResult<IngestReport> {
    let opts = opts.unwrap_or(IngestOptions {
        ledger_home: None,
        roots: None,
    });
    let roots = opts.roots.unwrap_or(IngestRoots {
        claude_projects_dir: None,
        codex_sessions_dir: None,
        opencode_storage_dir: None,
    });
    let raw = sdk::IngestOptions {
        ledger_home: maybe_path(opts.ledger_home),
        roots: sdk::IngestRoots {
            claude_projects_dir: maybe_path(roots.claude_projects_dir),
            codex_sessions_dir: maybe_path(roots.codex_sessions_dir),
            opencode_storage_dir: maybe_path(roots.opencode_storage_dir),
        },
        on_progress: None,
        on_warn: None,
    };
    let report = sdk::ingest(raw).await.map_err(sdk_err)?;
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
/// `Ledger.open()` smoke-call shape from `packages/sdk/index.d.ts`; a
/// future PR can add a stateful `Ledger` JS class that holds a handle.
#[napi(js_name = "ledgerOpen")]
pub fn ledger_open(opts: Option<LedgerOpenOptions>) -> NapiResult<String> {
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
    fn maybe_path_threads_string_to_pathbuf() {
        assert!(maybe_path(None).is_none());
        assert_eq!(
            maybe_path(Some("/tmp/x".into())),
            Some(PathBuf::from("/tmp/x"))
        );
    }
}
