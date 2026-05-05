//! `relayburn-sdk` — the embedding API for relayburn. This is one of two
//! crates published to crates.io (the other is `relayburn-cli`); everything
//! a Rust consumer needs to read or compute against a relayburn ledger
//! lives behind this surface.
//!
//! The crate is mostly a thin re-export over `relayburn-reader`,
//! `relayburn-ledger`, `relayburn-analyze`, and `relayburn-ingest`. The
//! public API mirrors `@relayburn/sdk` (the TS package) so cross-language
//! consumers ask the same questions, but uses Rust types directly rather
//! than going through a JS bridge.
//!
//! # Surface at a glance
//!
//! Nine verbs, each callable two ways: as a free function or as a method
//! on [`LedgerHandle`]. The verbs themselves land in follow-up PRs;
//! this crate currently exposes the scaffold ([`Ledger::open`],
//! [`LedgerOpenOptions`], [`LedgerHandle`]) plus the re-exports below.
//!
//! `ingest` is async (tokio); the eight query/compute verbs are sync
//! (CPU-bound). Callers running them from an async context — the typical
//! pattern in the MCP server — should wrap them in `tokio::task::spawn_blocking`.
//!
//! # Opening a ledger
//!
//! ```no_run
//! use relayburn_sdk::{Ledger, LedgerOpenOptions};
//!
//! let handle = Ledger::open(LedgerOpenOptions::default())?;
//! # Ok::<_, anyhow::Error>(())
//! ```
//!
//! [`LedgerOpenOptions`] lets callers point at a non-default
//! `RELAYBURN_HOME` (and a separate `content.sqlite` location) without
//! mutating process env — useful for tests and embeddings that run
//! against multiple ledgers in the same process.

use std::path::PathBuf;

// --- Re-exports ------------------------------------------------------------
//
// We expose every type a caller might need to construct an option struct or
// destructure a result, without forcing them to add the lower crates to
// their own `Cargo.toml`. The grouping mirrors the four wave-1 crates.

pub use relayburn_reader::{
    parse_bash_command, resolve_project, ActivityCategory, BashParse, ClassificationInput,
    ClassificationResult, CompactionEvent, ContentKind, ContentRecord, ContentRole,
    ContentStoreMode, ContentToolResult, ContentToolUse, Coverage, Fidelity, FidelityClass,
    Harness, ProjectResolver, RelationshipSourceKind, RelationshipType, ResolvedProject,
    SessionRelationshipRecord, SourceKind, Subagent, ToolCall, ToolResultEventRecord,
    ToolResultEventSource, ToolResultStatus, TurnRecord, Usage, UsageAttribution,
    UsageGranularity, UserTurnBlock, UserTurnBlockKind, UserTurnRecord,
};

pub use relayburn_ledger::{
    burn_sqlite_path, config_path, content_sqlite_path, is_valid_session_id, ledger_home,
    load_config, BurnConfig, ContentConfig, EnrichedTurn, Enrichment, Ledger as RawLedger,
    LedgerError, MessageRange, PruneStats, Query, RebuildSummary, Retention, SearchHit,
    SearchOptions, Stamp, StampError, StampSelector, DEFAULT_RETENTION_DAYS,
};

pub use relayburn_analyze::{
    aggregate_by_bash, aggregate_by_bash_verb, aggregate_by_file, aggregate_by_subagent,
    attribute_hotspots, attribute_overhead, build_trim_recommendations, cost_for_turn,
    cost_for_usage, detect_patterns, detect_tool_call_patterns, detect_tool_output_bloat,
    find_overhead_files, findings_from_patterns, has_minimum_fidelity, load_overhead_file,
    load_pricing, render_unified_diff_for_recommendation, summarize_fidelity, sum_costs,
    AttributeOverheadInput, AttributionMethod, BashAggregation, BashVerbAggregation,
    CostBreakdown, FidelitySummary, FileAggregation, HotspotsOptions as AnalyzeHotspotsOptions,
    HotspotsResult as AnalyzeHotspotsResult, ModelCost, OverheadAttribution, OverheadFile,
    OverheadFileAttribution, OverheadFileKind, ParsedOverheadFile, PricingTable, ReasoningMode,
    SessionTotals, SubagentAggregation, ToolAttribution, TrimRecommendation, WasteFinding,
    WasteSeverity,
};

pub use relayburn_ingest::{
    cleanup_stale_pending_stamps, ingest_all, IngestOptions as RawIngestOptions, IngestReport,
    IngestRoots,
};

// --- LedgerOpenOptions -----------------------------------------------------

/// Where on disk a [`Ledger`] should land. Both fields default to the
/// `RELAYBURN_HOME` / `RELAYBURN_CONTENT_PATH` env-var resolved paths;
/// override them for tests or for callers that keep multiple ledgers per
/// process.
#[derive(Debug, Clone, Default)]
pub struct LedgerOpenOptions {
    /// Override for `$RELAYBURN_HOME` (the directory containing
    /// `burn.sqlite`). When `None`, the env var is consulted, then
    /// `~/.relayburn`.
    pub home: Option<PathBuf>,
    /// Override for the `content.sqlite` location specifically. When
    /// `None`, follows `home` (or its env-var fallback). Provided as a
    /// separate knob because content can grow large and is often parked on
    /// cheaper / bigger storage than the events DB.
    pub content_home: Option<PathBuf>,
}

impl LedgerOpenOptions {
    /// Build with an explicit home directory; both `burn.sqlite` and
    /// `content.sqlite` will live inside it.
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            home: Some(home.into()),
            content_home: None,
        }
    }

    fn resolve_burn_path(&self) -> PathBuf {
        match &self.home {
            Some(h) => h.join("burn.sqlite"),
            None => burn_sqlite_path(),
        }
    }

    fn resolve_content_path(&self) -> PathBuf {
        if let Some(c) = &self.content_home {
            return c.join("content.sqlite");
        }
        match &self.home {
            Some(h) => h.join("content.sqlite"),
            None => content_sqlite_path(),
        }
    }
}

// --- Ledger / LedgerHandle -------------------------------------------------

/// Handle on an open relayburn ledger. Wraps [`relayburn_ledger::Ledger`]
/// (re-exported as [`RawLedger`]) and exposes the SDK verb surface.
///
/// Not `Sync`; wrap in a `Mutex` if you need to share it across threads.
/// The underlying SQLite WAL allows concurrent reads via separate handles
/// pointing at the same files, which is the recommended pattern for
/// long-lived embeddings — open one handle per worker thread instead of
/// sharing one through a lock.
pub struct LedgerHandle {
    pub(crate) inner: RawLedger,
}

impl LedgerHandle {
    /// Borrow the underlying [`RawLedger`] for direct access. Useful when
    /// a caller needs a lower-level method that the SDK has not yet wrapped.
    pub fn raw(&self) -> &RawLedger {
        &self.inner
    }

    /// Mutable variant of [`Self::raw`].
    pub fn raw_mut(&mut self) -> &mut RawLedger {
        &mut self.inner
    }
}

/// Namespace type for the open verb. Matches the TS surface
/// (`Ledger.open(...)`); you usually just call [`Ledger::open`] and use the
/// returned [`LedgerHandle`].
pub struct Ledger;

impl Ledger {
    /// Open the ledger described by `opts`, applying schema DDL if needed,
    /// and return a [`LedgerHandle`] for the verbs in this crate.
    pub fn open(opts: LedgerOpenOptions) -> anyhow::Result<LedgerHandle> {
        let burn = opts.resolve_burn_path();
        let content = opts.resolve_content_path();
        let inner = RawLedger::open(&burn, &content)?;
        Ok(LedgerHandle { inner })
    }
}
