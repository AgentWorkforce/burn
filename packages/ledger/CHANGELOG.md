# Changelog

All notable changes to `@relayburn/ledger` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1] - 2026-04-23

### Changed

- Backfill 0.1.0 and 0.2.0 changelog entries for all four packages

## [0.2.0] - 2026-04-23

### Added

- **`reclassifyLedger({ force? })`** — rewrites the ledger atomically and re-runs `classifyActivity` on every turn. Default mode skips turns that already have an `activity` set (safe to re-run, won't downgrade Claude turns whose content sidecar has since been pruned). `--force` reclassifies everything; useful after a classifier rule change.
- Recovers user prompt text + assistant text + errored tool IDs from the content sidecar when present, falling back to tool-only signal otherwise.
- Returns a `ReclassifyReport` with separate `scanned` / `processed` / `changed` / `skipped` counters plus per-category change breakdown.
- Preserves stamp lines and blank lines verbatim; only rewrites turn lines whose classification actually changed.

### Fixed

- **Race between `appendTurns` and `reclassifyLedger`.** `appendLines` in the writer now acquires the same `withLock('ledger', ...)` that `reclassifyLedger` holds for its read-modify-write pass. Without this, an `appendFile` that landed between reclassify's `readFile` and `rename` would write to the soon-orphaned old inode and silently disappear when the rename swapped the new file in. Reported by Devin's PR review on #48.

## [0.1.0] - 2026-04-22

### Added

- **Initial release.** Append-only JSONL ledger at `~/.relayburn/ledger.jsonl` with override via `RELAYBURN_HOME`.
- `appendTurns(turns)` with content-fingerprint dedup against historical turns.
- `stamp(selector, enrichment)` — attach metadata to a session by `{sessionId}`, `{messageId}`, or `{sessionId, range: { fromTs, toTs }}`. Last-write-wins per key. Stamps fold at query time.
- `query(q)` / `queryAll(q)` async iterables, returning `EnrichedTurn` (the turn record plus folded enrichment). Filters: `since`, `until`, `project`, `sessionId`, `source`, `enrichment`.
- **Content sidecar** at `~/.relayburn/content/<sessionId>.jsonl`. `appendContent`, `readContent`, `pruneContent` with `full` / `hash-only` / `off` modes via `RELAYBURN_CONTENT_STORE` or config file. Default 90-day retention, configurable via `RELAYBURN_CONTENT_TTL_DAYS`.
- **Index sidecar** for fast dedup. `rebuildIndex()` rebuilds the on-disk hash sets from the ledger; `turnIdHash`, `turnContentFingerprint` exported.
- Per-process file lock (`withLock`) used by content writes and index updates; stale-lock recovery after 30s.
- Cursors (`loadCursors`, `saveCursors`) for incremental ingest of Claude / Codex / OpenCode session files. Carries `lastUserText` for Claude so classification keyword refinement survives EOF resumes.
- Schema types: `LedgerLine`, `TurnLine`, `StampLine`, `Enrichment`, `MessageIdRange`, `StampSelector` plus `isTurnLine` / `isStampLine` / `stampMatches` guards.
