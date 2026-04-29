# Changelog

All notable changes to `@relayburn/ledger`.

## [Unreleased]

### Changed

- Archive builds now materialize stamps in SQLite so incremental turn folding no longer scans the full ledger history.

## [0.43.0] - 2026-04-29

### Added

- Session stamps with `parentAgentId` now append a deduped `spawn-env` relationship record for source-agnostic spawn-tree queries.

## [0.40.0] - 2026-04-28

### Added

- Added `vacuumArchive()` to reclaim unused `archive.sqlite` space under the archive lock.

## [0.35.0] - 2026-04-28

### Changed

- Codex cursors now retain the last committed turn so compaction events can anchor correctly during incremental ingest.

## [0.33.0] - 2026-04-27

### Dependencies

- Version sync only. No package behavior changed.

## [0.31.0] - 2026-04-27

### Added

- Added `queryAllFromArchive()` and `archiveAvailable()` for SQL-backed `EnrichedTurn` queries.

## [0.30.0] - 2026-04-27

### Changed

- Archive query helpers now support MCP hot-path reads.

## [0.27.0] - 2026-04-26

### Added

- Added append-only `user_turn` ledger records with `appendUserTurns()`, `queryUserTurns()`, and index rebuild support.

## [0.26.0] - 2026-04-26

### Added

- Added `queryTurnsFromArchive()` for archive-backed turn queries.
- Codex cursors now track execution-graph state across incremental ingest.

## [0.25.0] - 2026-04-26

### Added

- The archive now materializes `tool_result_event` ledger lines and reports their row counts.

### Changed

- Bumped `ARCHIVE_VERSION` to 2 for the new tool-result-event indexes.

## [0.24.0] - 2026-04-26

### Added

- Added fidelity and coverage columns to archive `turns` and `sessions`.
- `getArchiveStatus()` now returns a fidelity histogram.

## [0.21.0] - 2026-04-26

### Added

- Added the `UserTurnLine` ledger kind for persisted user-turn block sizes.

## [0.20.0] - 2026-04-26

### Added

- Added the rebuildable `archive.sqlite` read model with `buildArchive()`, `rebuildArchive()`, `getArchiveStatus()`, `openArchive()`, and `archivePath()`.

## [0.19.0] - 2026-04-26

### Added

- Added `relationship` and `tool_result_event` ledger kinds with append, query, guard, dedup, and index rebuild support.

## [0.15.0] - 2026-04-25

### Changed

- `pruneContent()` can now skip recoverable sidecars through an optional callback.

## [0.14.1] - 2026-04-25

### Fixed

- Lock recovery now outlasts stale lock windows and reports clearer timeout causes.

## [0.13.1] - 2026-04-25

### Changed

- Version sync only. No package behavior changed.

## [0.11.0] - 2026-04-25

### Added

- Added plan config primitives, built-in presets, validation, and `plansPath()`.

## [0.8.0] - 2026-04-24

### Added

- Added `buildClaudeHookSettings()` for per-invocation Claude hook installation.

## [0.7.0] - 2026-04-24

### Added

- Added `listContentSessionIds()` for content sidecar backfills.

## [0.6.0] - 2026-04-24

### Added

- Stored turn data now exposes the error and retry signals used by waste-pattern detectors.

## [0.5.0] - 2026-04-24

### Changed

- Moved earlier unreleased notes into release sections. No package behavior changed.

## [0.4.0] - 2026-04-23

### Changed

- Version sync only. No package behavior changed.

## [0.3.0] - 2026-04-23

### Changed

- Version sync only. No package behavior changed.

## [0.2.0] - 2026-04-23

### Added

- Added `reclassifyLedger()` for atomic activity backfills.

### Fixed

- `appendTurns()` now shares the ledger lock with reclassification rewrites, preventing lost rows.

## [0.1.0] - 2026-04-22

### Added

- Initial release with the append-only ledger, stamps, queries, content sidecar, dedup index, locks, cursors, and schema types.
