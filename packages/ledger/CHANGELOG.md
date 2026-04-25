# Changelog

All notable changes to `@relayburn/ledger` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **Lock recovery gap closed.** `withLock` previously gave up after 50×20ms = 1 second of retries while declaring a lockfile stale only after 30 seconds, leaving a 29-second window where any orphan lock from a crashed `burn` process would hard-fail every acquirer with `could not acquire lock after 50 attempts`. The acquire loop is now a two-phase fast/slow schedule (1s of 20ms retries for normal contention, then 10s of 250ms retries to outlast the stale window), and the stale threshold drops from 30s to 5s. A single CLI invocation now self-heals through an orphan in well under a second instead of failing for half a minute. Timeout error messages also distinguish "held by live process" from "lock appears stale but unlink kept failing" so the failure mode is obvious. Closes [#62](https://github.com/AgentWorkforce/burn/issues/62).

## [0.11.0] - 2026-04-25

### Added

- **Plans config primitives** (#39). New `Plan` / `PlansFile` / `PlanProvider` types, `loadPlans()` / `savePlans()` against `~/.relayburn/plans.json`, `BUILTIN_PRESETS` covering claude/pro ($20), claude/max ($200), cursor/pro ($20), and `findPreset(provider, name)`. `normalizePlan` validates each row (positive `budgetUsd`, integer `resetDay` 1-31, known `provider`) so a malformed file throws once at load rather than producing garbage downstream. New `plansPath()` exported alongside the existing path helpers.

## [0.8.0] - 2026-04-24

### Added

- **`buildClaudeHookSettings({ burnBin? })`** — spawner-side primitive returning `{ sessionId, settings }`: a fresh UUID plus a JSON string wiring every Claude Code hook event (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Notification`, `Stop`, `SubagentStop`, `SessionEnd`) to `burn ingest --runtime claude --quiet`. Sits alongside `stamp` so orchestrators get both integration primitives from one package; spawners can inject hooks per-invocation via Claude's `--settings` flag rather than mutating `~/.claude/settings.json` globally. Closes [#7](https://github.com/AgentWorkforce/burn/issues/7).

## [0.7.0] - 2026-04-24

### Added

- **Content capture for Codex and OpenCode sessions** lands in the ledger via the reader's expanded emitters; no schema change in this package. Supporting helper `listContentSessionIds()` enumerates sessionIds with a non-empty sidecar on disk, used by `burn rebuild --content` to skip already-populated sessions.

## [0.6.0] - 2026-04-24

### Added

- **Waste-pattern detector ledger queries.** Analyze consumers read `toolCalls[].isError` and `retries` directly off stored `TurnRecord`s — no schema change, just the query shape the detectors (retry loops, failure runs, compaction loss, edit-revert) operate on. Closes [#11](https://github.com/AgentWorkforce/burn/issues/11).

## [0.5.0] - 2026-04-24

### Changed

- Clean up changelogs: move [Unreleased] content into 0.3.0/0.4.0 sections

## [0.4.0] - 2026-04-23

Synchronized version bump alongside `@relayburn/cli@0.4.0` / `@relayburn/analyze@0.4.0` / `@relayburn/reader@0.4.0`. No functional changes in this package.

## [0.3.0] - 2026-04-23

Synchronized version bump alongside `@relayburn/cli@0.3.0` / `@relayburn/analyze@0.3.0` / `@relayburn/reader@0.3.0`. No functional changes in this package.

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
