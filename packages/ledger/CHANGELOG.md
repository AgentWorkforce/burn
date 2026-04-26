# Changelog

All notable changes to `@relayburn/ledger` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`queryAllFromArchive(query)` + `archiveAvailable()`** ([#82](https://github.com/AgentWorkforce/burn/issues/82)). New read-side entry point in `@relayburn/ledger` that issues SQL against `archive.sqlite` and returns the same `EnrichedTurn[]` shape as `queryAll`, so consumers (starting with `burn summary`) can swap implementations without touching their aggregation code. Filters land as `WHERE` clauses against indexed columns (`ts`, `model`, `project_key`, `session_id`, `source`, materialized enrichment columns); arbitrary stamp keys not promoted to columns fall back to a `json_extract` over `enrichment_json` to match `queryAll` semantics. Tool calls are bulk-hydrated keyed on `(source, session_id, message_id)` so callers that read `turn.toolCalls` keep working without an extra round-trip. Fidelity is reconstructed from the persisted `attribution_fidelity` / `tokens_present` / `cost_present` columns plus class-implied coverage defaults — class equality (the load-bearing parity contract for `summarizeFidelity`) is preserved; the synthesized coverage shape may differ from the on-ledger blob for classes that don't pin every flag.
- **`CodexCursor` carries execution-graph commit state** ([#87](https://github.com/AgentWorkforce/burn/issues/87)). Three optional fields — `rootSessionEmitted`, `nextEventIndex`, `toolResultCounters` — let `burn ingest` resume Codex sessions without re-emitting the root `SessionRelationshipRecord` or restarting `ToolResultEventRecord.eventIndex` at zero across `burn` invocations. Older cursor files are backward-compatible: missing fields default to "fresh" (root not emitted, indices start at zero), and the next ingest pass pre-loads them onto the writer's dedup index.

## [0.25.0] - 2026-04-26

### Added

- **`tool_result_events` archive table is now populated** (#101). `buildArchive()` materializes `ToolResultEventLine` ledger lines into the previously-empty `tool_result_events` table during incremental builds, keyed on (`source`, `session_id`, `message_id`, `tool_use_id`, `event_index`). Columns mirror the canonical `ToolResultEventRecord`: `status`, `content_length`, `content_hash`, `subagent_session_id`, `agent_id`, `event_source`, `ts`, `call_index`. Cursor uses the same `archive_state.ledger_offset_bytes` as turns / stamps / compactions, so no parallel cursor and no extra disk read. `rebuildArchive()` replays tool-result events alongside the other line kinds and yields the same row count deterministically. New `idx_tool_result_events_use_id` / `_session` / `_subagent` indexes for the obvious join paths. `BuildResult.toolResultEventsApplied` and `ArchiveStatus.rowCounts.toolResultEvents` expose the new counts. Closes #101, refs #40, #42, #77.

### Changed

- **Bumped `ARCHIVE_VERSION` to 2** to pick up the new `tool_result_events` indexes on existing archives. The next `buildArchive()` call detects the version mismatch and rebuilds from scratch — safe because the archive is derived state.

## [0.24.0] - 2026-04-26

### Added

- **Fidelity / coverage columns on the analytics archive** (#110, follow-up to #40 / #41 / #78). `turns` carries `attribution_fidelity` (the `FidelityClass` string from `TurnRecord.fidelity.class` — `full`, `usage-only`, `partial`, `aggregate-only`, `cost-only`), `tokens_present` (1 if the source surfaced any per-turn input/output/reasoning count), and `cost_present` (1 iff cost-only). `sessions` carries `min_fidelity` (the worst class observed across the session's known-fidelity turns) and `has_full_attribution` (1 iff every fidelity-tagged turn is `full`). Older lines that pre-date the upstream parser fidelity work (Codex/OpenCode pre-#84/#89) persist `NULL` rather than guessing — downstream queries should read `NULL` as "unknown". Migration is additive: `openArchive()` runs idempotent `ALTER TABLE … ADD COLUMN` guarded by `PRAGMA table_info`, so existing archives forward-migrate without a rebuild and `ARCHIVE_VERSION` stays at 1. `getArchiveStatus()` now returns a `fidelityHistogram` (counts per `attribution_fidelity` value, with `NULL` bucketed as `unknown`).

## [0.21.0] - 2026-04-26

### Added

- **User-turn ledger line: `UserTurnLine`** (#94, follows #74). New `LedgerLine` kind (`user_turn`) with matching `appendUserTurns` writer, `queryUserTurns` reader, and `isUserTurnLine` guard. Append-only and dedup'd through the same `~/.relayburn/ledger-index` namespace as turns / compactions / relationships / tool-result events via `userTurnIdHash` keyed on `(source, sessionId, userUuid)`. `rebuildIndex` re-indexes the new kind. Old readers that don't recognize `kind: 'user_turn'` simply skip the line — the existing per-kind guards already filter to known kinds. Persists the per-user-turn block sizes the reader started emitting in #74 so consumers can read them back without re-parsing source session files; backfilling user turns into existing ledgers is not done automatically — see follow-up.

## [0.20.0] - 2026-04-26

### Added

- **Derived analytics archive** (#40, foundation PR). New `archive.sqlite` materialized read model alongside the canonical `ledger.jsonl`, exposed via `buildArchive()`, `rebuildArchive()`, `getArchiveStatus()`, and `openArchive()`. Schema covers `sessions`, `turns`, `tool_calls`, `compactions`, and a reserved `tool_result_events` table for the future content-sidecar bridge (#33). Stamps are folded into materialized columns (`workflow_id`, `agent_id`, `persona`, `tier`) plus a JSON blob, so consumers no longer have to fold the full stamp set on every query. Build is incremental keyed off `archive_state.ledger_offset_bytes`, and rebuild-from-zero is deterministic — deleting `archive.sqlite` and rebuilding always reproduces the same row counts. Backed by `node:sqlite` so no native build step. New `archivePath()` exported alongside the existing path helpers.

## [0.19.0] - 2026-04-26

### Added

- **Execution-graph ledger lines: `SessionRelationshipLine` and `ToolResultEventLine`** (#42, first PR). Two new `LedgerLine` kinds (`relationship`, `tool_result_event`) with matching `appendRelationships` / `appendToolResultEvents` writers, `queryRelationships` / `queryToolResultEvents` readers, and `isSessionRelationshipLine` / `isToolResultEventLine` guards. Both append-only; both dedup through the same `~/.relayburn/ledger-index` namespace as turns and compactions via `relationshipIdHash` (keyed on type + agentId + parentToolUseId) and `toolResultEventIdHash` (keyed on sessionId + toolUseId + eventIndex). `rebuildIndex` re-indexes both kinds. Old readers that don't recognize the new kinds simply skip them — the existing `isTurnLine` / `isStampLine` / `isCompactionLine` guards already filter to known kinds.
- `burn ingest` (both runtime-driven and hook paths) and `burn claude` now persist relationships + tool-result events when the Claude reader emits them, so the execution-graph substrate lands automatically alongside turns.

## [0.15.0] - 2026-04-25

### Changed

- **`pruneContent` accepts an optional `isRecoverable(sessionId)` callback** and skips sidecars whose source session file still exists. The ledger package stays decoupled from adapter-specific paths — the predicate is supplied by the caller. `PruneResult` now carries a `skippedRecoverable` count alongside `filesDeleted` / `bytesFreed`. Without `isRecoverable` (or with a throwing predicate), retention is applied unchanged. (#61)

## [0.14.1] - 2026-04-25

### Fixed

- **Lock recovery gap closed.** `withLock` previously gave up after 50×20ms = 1 second of retries while declaring a lockfile stale only after 30 seconds, leaving a 29-second window where any orphan lock from a crashed `burn` process would hard-fail every acquirer with `could not acquire lock after 50 attempts`. The acquire loop is now a two-phase fast/slow schedule (1s of 20ms retries for normal contention, then 10s of 250ms retries to outlast the stale window), and the stale threshold drops from 30s to 5s. A single CLI invocation now self-heals through an orphan in well under a second instead of failing for half a minute. Timeout error messages also distinguish "held by live process" from "lock appears stale but unlink kept failing" so the failure mode is obvious. Closes [#62](https://github.com/AgentWorkforce/burn/issues/62).

## [0.13.1] - 2026-04-25

### Changed

- Bump packages to v0.13.0

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
