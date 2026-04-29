# Changelog

All notable changes to `@relayburn/analyze`.

## [Unreleased]

### Changed

- `detectToolOutputBloat` now sizes oversized tool output via cl100k token counts from user-turn enrichment, with a bytes/4 fallback for legacy ledgers. Highly compressible payloads (repetitive logs, base64 dumps) score lower in tokens and may slip below the 15k default threshold.

### Fixed

- `detectObservedBloat` no longer double-counts a tool call when its `tool_result` is followed by a `subagent_notification` (or other non-carrier event) sharing the same `toolUseId`. Non-carrier events are now sized by their own `contentLength` instead of inheriting the carrier's enriched token count.

## [0.44.0] - 2026-04-29

### Changed

- Use tool-result events for waste patterns

## [0.43.0] - 2026-04-29

### Added

- Added shared effective-provider helpers and `aggregateByProvider()` for provider-scoped rendering.

### Changed

- `detectPatterns()` now uses persisted tool-result event chronology for retry and failure detection, with legacy `toolCalls[].isError` fallback when graph rows are absent.
- Waste attribution now prefers persisted user-turn block sizes over content sidecar estimates before falling back to even-split attribution.

## [0.42.0] - 2026-04-28

### Added

- Ghost-surface detection now recognizes Claude command markers and Codex slash prompt invocations when content sidecars are available.
- Added `detectToolOutputBloat` for oversized tool output from Claude config and observed tool-result events.
- Added `detectGhostSurface` for unused user-installed agents, skills, commands, prompts, rules, and memories.

## [0.41.0] - 2026-04-28

### Added

- Waste-pattern detectors can now include content-sidecar context such as error signatures, lost work summaries, and edit previews.
- Added the shared `WasteFinding` / `WasteAction` envelope plus adapters for all waste detectors.

## [0.37.0] - 2026-04-28

### Added

- Added an edit-heavy session detector for sessions with at least five edits and more than four edits per read.

## [0.35.0] - 2026-04-28

### Added

- Added OpenCode detectors for repeated skill calls, prune-protected skill results, and system-prompt or skill-catalog tax.

## [0.34.0] - 2026-04-27

### Changed

- `burn compare` now uses analyze fidelity helpers to exclude low-confidence turns by default.

## [0.33.0] - 2026-04-27

### Added

- Added `planUsageFromArchive()` for archive-backed plan spend queries.
- `PlanUsage` now includes cycle fidelity confidence and summary data.

## [0.31.0] - 2026-04-27

### Added

- Added `compareFromArchive()` to build compare tables directly from `archive.sqlite`.

## [0.27.0] - 2026-04-26

### Changed

- Waste attribution now uses persisted user-turn block sizes before falling back to even-split attribution.

## [0.22.0] - 2026-04-26

### Changed

- OpenCode fidelity data is now available to analyze consumers.

## [0.18.0] - 2026-04-26

### Fixed

- Fixed reasoning-token pricing. Codex reasoning is no longer double-billed, and distinct reasoning tariffs from pricing data are honored.
- Waste-attribution totals now use the same reasoning pricing path as per-turn costs.

### Added

- Added `ModelCost.reasoningMode`, optional reasoning tariffs, `CostForUsageOptions`, and exported `flatten()`.

## [0.14.0] - 2026-04-25

### Added

- Added `summarizeFidelity()` and `hasMinimumFidelity()` for coverage-aware analysis.

## [0.13.1] - 2026-04-25

### Added

- Added query-time provider reattribution for Synthetic-routed model IDs.

## [0.11.0] - 2026-04-25

### Added

- Added `computePlanUsage()` for monthly plan spend, projection, budget, and runway calculations.
- Added `cycleBounds()` for reset-day based billing windows.

## [0.9.0] - 2026-04-24

### Added

- Added `buildSubagentTree()` and `aggregateSubagentTypeStats()`.

## [0.8.0] - 2026-04-24

### Added

- Added analyze support needed by Claude hook-based ingest.

## [0.6.0] - 2026-04-24

### Added

- Added quality signals: `computeQuality()`, `inferOutcome()`, and `computeOneShotRate()`.
- Added waste-pattern detectors for retry loops, failure runs, compaction loss, and edit reverts.

## [0.5.0] - 2026-04-24

### Changed

- Moved earlier unreleased notes into release sections. No package behavior changed.

## [0.4.0] - 2026-04-23

### Added

- Added per-tool, per-file, Bash, and subagent cost attribution.

## [0.3.0] - 2026-04-23

### Added

- Added context-file parsing, attribution, and trim recommendations for `CLAUDE.md` and `AGENTS.md`.

### Fixed

- Fixed markdown section parsing edge cases and duplicate context riding-turn counts.

## [0.2.0] - 2026-04-23

### Added

- Added `buildCompareTable()` and `DEFAULT_MIN_SAMPLE`.
- Compare cells now distinguish no pricing from zero cost and no data from low sample size.
- Requested models remain visible even when the filtered slice has no matching turns.

## [0.1.0] - 2026-04-22

### Added

- Initial release with pricing loading, per-turn cost calculation, cost aggregation, and provider-prefix fallback.
