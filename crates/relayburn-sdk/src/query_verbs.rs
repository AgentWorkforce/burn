//! Query verbs — `summary`, `session_cost`, `overhead`, `overhead_trim`,
//! `hotspots`. Filled in by the follow-up to #246 PR1.
//!
//! Each verb appears as an `impl LedgerHandle` method (sync, returns
//! `anyhow::Result`) plus a free-function form that opens its own ledger
//! handle from `LedgerOpenOptions`.

// TODO(#246): port the query verbs from `packages/sdk/index.js`.
