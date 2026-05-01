//! `burn-sdk` — public programmatic surface for relayburn.
//!
//! This is the supported embedding surface. `burn-cli` consumes it; wash
//! consumes it directly via `Cargo.toml`; `burn-sdk-node` wraps it via
//! napi-rs and re-exports it as `@relayburn/sdk` on npm.
//!
//! Planned exports (filed as sub-issues under #222):
//!
//! ```ignore
//! pub use burn_reader::{TurnRecord, ContentRecord, ActivityCategory, Harness};
//! pub use burn_ledger::{Ledger, LedgerHandle};
//! pub use burn_analyze::{Summary, HotspotFinding, Pattern};
//!
//! pub struct LedgerOptions { pub home: Option<PathBuf> }
//! impl Ledger { pub fn open(opts: LedgerOptions) -> Result<LedgerHandle> }
//!
//! pub async fn ingest(opts: IngestOptions) -> Result<IngestReport>;
//! pub async fn summary(q: SummaryQuery) -> Result<Summary>;
//! ```
//!
//! Async boundary: `ingest` and the watch loop are `async` (tokio).
//! `summary` and `hotspots` are sync — CPU-bound queries against an open
//! handle. Wash's MCP handlers wrap them in `tokio::task::spawn_blocking`.

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_compiles() {}
}
