//! `burn-ledger` — append-only JSONL ledger with content sidecar and SQLite archive.
//!
//! Mirrors `packages/ledger/src/` from the TypeScript workspace. Planned
//! modules (filed as sub-issues under #222):
//!
//! - `schema`         — `LedgerLine`, `TurnLine`, `StampLine` (was `schema.ts`)
//! - `writer`         — append + stamp (was `writer.ts`)
//! - `reader`         — streaming line reader (was `reader.ts`)
//! - `lock`           — process-wide `withLock` primitive (was `lock.ts`)
//! - `archive`        — SQLite archive build / refold / vacuum (was `archive.ts`)
//! - `archive_query`  — read-side archive queries (was `archive-query.ts`)
//! - `content`        — content sidecar (was `content.ts`)
//! - `cursors`        — incremental session cursors (was `cursors.ts`)
//! - `index_sidecar`  — id + content-fingerprint indexes (was `index-sidecar.ts`)
//! - `hwm`            — high-water-mark helpers (was `hwm.ts`)
//! - `plans`          — budget plans CRUD (was `plans.ts`)
//! - `reclassify`     — activity reclassification (was `reclassify.ts`)
//! - `paths`          — `~/.relayburn` path resolution (was `paths.ts`)
//! - `config`         — adapter selection (was `config.ts`)
//! - `hook_settings`  — Claude hook settings injection (was `hook-settings.ts`)
//! - `adapters::file` — JSONL on-disk adapter (was `adapters/file-adapter.ts`)
//! - `adapters::factory` — adapter selection (was `adapters/factory.ts`)
//!
//! Concurrency invariant: any read-modify-write on the ledger must hold the
//! `'ledger'` lock. In Rust this becomes a typed `LedgerLock` guard so the
//! borrow checker enforces what the TS version enforced by convention.

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_compiles() {}
}
