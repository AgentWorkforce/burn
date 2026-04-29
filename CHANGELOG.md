# Changelog

Cross-package release notes for relayburn. Package changelogs contain package-level detail.

## [Unreleased]

### Added

- Defaulted user-turn block sizing to cl100k, with a measurement script that reports token-count and tool-attribution drift against the bytes/4 heuristic fallback.

### Fixed

- `burn rebuild --content` now fast-skips renamed Codex rollout files using their embedded session metadata.

## [0.43.0] - 2026-04-29

### Changed

- `burn waste --patterns` now bases retry and failure findings on persisted tool-result chronology when available, while preserving legacy fallback behavior.
- Spawn-env and native sidechain attribution now share session relationship records, and `burn diagnose --explain-drift` surfaces sessions where they disagree.
- Provider-aware CLI rendering now uses shared analyze helpers for effective-provider resolution and aggregation.
- Per-tool cost attribution now uses persisted user-turn block sizes in summary and waste reports, with rebuild backfill for historical sessions.

## [0.42.0] - 2026-04-28

### Added

- OpenCode passive ingest now records compaction events, so compaction waste analysis covers Claude, Codex, and OpenCode.

## [0.41.0] - 2026-04-28

### Added

- Waste detectors now share a structured `WasteFinding` output. `burn waste --patterns --findings` renders all findings in one severity-ranked table and includes the same list in JSON.

## [0.39.0] - 2026-04-28

### Changed

- `burn compare` now requires a comma-separated model list as its first argument. The old `--models` flag was removed; filters and output schemas are unchanged.

## [0.37.0] - 2026-04-28

### Added

- `burn waste --patterns edit-heavy` flags sessions with many edits and too few reads across Claude, Codex, and OpenCode.

## [0.35.0] - 2026-04-28

### Added

- Codex passive ingest now records compaction events.
- `burn waste --patterns` adds OpenCode skill-recall, skill-pruning, and system-prompt tax detectors.

### Changed

- `burn run <claude|codex|opencode>` replaces the separate harness wrappers with one adapter-backed command.

### Removed

- Removed `burn claude`, `burn codex`, and `burn opencode`. Use `burn run <name>`.
- Removed `burn rebuild-index`. Use `burn rebuild --index`.

## [0.34.0] - 2026-04-27

### Changed

- `burn compare` now excludes low-fidelity turns by default, with `--fidelity` and `--include-partial` for overrides.

## [0.33.0] - 2026-04-27

### Added

- `burn plans` now reports per-cycle fidelity so partial data is marked as a lower-confidence estimate.

## [0.27.0] - 2026-04-26

### Added

- User-turn block sizes are now persisted for Claude, Codex, and OpenCode, giving `burn waste` a better fallback when full content is unavailable.

## [0.19.0] - 2026-04-26

### Added

- Added execution-graph records for session relationships and tool-result events.
- Claude ingest now writes the new graph records alongside turns and content.

## [0.18.0] - 2026-04-26

### Fixed

- Fixed reasoning-token pricing. Codex reasoning is no longer double-billed, and distinct reasoning tariffs from pricing data are honored.

## [0.13.1] - 2026-04-25

### Added

- Added the rebuildable `archive.sqlite` analytics read model and `burn archive build | rebuild | status`.
- Added `@relayburn/mcp` and `burn mcp-server` so agents can query their own session cost and quota state.

## [0.11.0] - 2026-04-25

### Added

- Added monthly plan tracking with built-in Claude and Cursor presets, projections, runway, reset-day support, and `burn limits` integration.

## [0.9.0] - 2026-04-24

### Added

- Added Claude subagent tree reconstruction plus `burn summary --subagent-tree` and `--by-subagent-type`.

## [0.8.0] - 2026-04-24

### Added

- Added Claude Code hook-based ingest without mutating global Claude settings.

## [0.7.0] - 2026-04-24

### Added

- Codex and OpenCode parsers now capture content sidecars.
- Added `burn rebuild --content` and `listContentSessionIds()` for sidecar backfills.

## [0.6.0] - 2026-04-24

### Added

- Added quality signals: inferred session outcome, one-shot edit rate, and retry volume.
- Added waste-pattern detectors for retry loops, failure runs, compaction loss, and edit reverts.

## [0.5.0] - 2026-04-24

### Changed

- Moved earlier unreleased changelog notes into their release sections. No package behavior changed.

## [0.4.0] - 2026-04-23

### Added

- Added `burn waste` for per-tool, per-file, Bash, and subagent cost attribution.

## [0.3.0] - 2026-04-23

### Added

- Added `burn context` for context-file cost attribution.
- Added `burn context advise` for read-only trim recommendations.

## [0.2.0] - 2026-04-23

### Added

- Added `burn compare` for model-by-activity cost and quality comparison.
- Added `burn rebuild --reclassify` and `--index`.
- Activity classification now covers Claude, Codex, and OpenCode, with normalized tool names and six new categories.

### Fixed

- Ledger appends now lock against reclassification rewrites, preventing lost rows.

### Changed

- Tightened deploy/build classifier patterns.
- `burn compare` now rejects `--json` and `--csv` together.

## [0.1.0] - 2026-04-22

### Added

- Initial release of `@relayburn/reader`, `@relayburn/ledger`, `@relayburn/analyze`, and `@relayburn/cli`.
