//! Ingest verb — async wrapper over `relayburn_ingest::ingest_all`.
//! Filled in by the follow-up to #246 PR1.
//!
//! Both an `impl LedgerHandle` method (`async fn ingest`) and a free
//! function form (`pub async fn ingest`) live here.

// TODO(#246): port the ingest verb from `packages/sdk/index.js`.
