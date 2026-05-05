# Changelog

Cross-package release notes for relayburn. Package changelogs contain package-level detail.

## [Unreleased]

- `relayburn-ingest` (Rust): port the standalone primitives — `pending_stamps` (binary-compatible with the TS `@relayburn/ingest` wire format), `walk` (`walk_jsonl` / `walk_opencode_sessions`), `watch_loop` (`tokio::time::interval`-driven `WatchController` with graceful stop), and the typed `cursors` module layered on the SQLite ledger's cursor blob. Public verb surface (`ingest_all`, per-harness verbs, `reingest_missing_content`) is wired; per-harness orchestration follow-ups deferred to dedicated sub-issues. (#245)
- `relayburn-analyze` (Rust): port the `compare` aggregator — `build_compare_table` for the in-memory `(model, activity)` rollup with per-cell turn / edit / one-shot / priced / cost / cache-hit / median-retries metrics, plus `compare_from_archive` sourced from the SQLite ledger via `Ledger::query_turns`. Public surface: `CompareCell`, `CompareTable`, `CompareTotals`, `CompareOptions`, `CompareCategory`, `DEFAULT_MIN_SAMPLE`, `compare_from_archive`, `CompareFromArchiveResult`. (#269)

## [1.9.0] - 2026-05-03

### Changed

- **Architecture: `@relayburn/sdk` is now the canonical in-process query surface.** Dependency order moves from `… → mcp → cli → sdk → relayburn` to `… → sdk → mcp → cli → relayburn`; `@relayburn/mcp` now depends on `@relayburn/sdk` and rewrites `burn__sessionCost` as a thin wrapper over the SDK's new `sessionCost()` function. New read verbs should land in the SDK first; MCP and CLI become presenters (tool definitions / table rendering) over the same SDK calls so query logic stops drifting between them.
- `burn compare` joins `summary` / `sessionCost` / `overhead` / `overheadTrim` as a thin presenter over `@relayburn/sdk`'s new `compare()` function. The archive-vs-ledger branching and fidelity-gate logic move into the SDK so a future `burn__compare` MCP tool (and embedders) can wrap the same call without re-implementing them.

### Breaking Changes

- `@relayburn/sdk` `hotspots()` now returns a discriminated union (`{ kind: 'attribution' | 'bash' | 'bash-verb' | 'file' | 'subagent' | 'findings' }`) instead of either a raw attribution blob or a flat findings array. CLI / MCP / embedded callers must branch on `kind`. Mirrors the shape `burn hotspots --json` emits and adds four narrow `groupBy` views for single-axis consumers.

## [1.8.0] - 2026-05-02

### Added

- Recognise `_meta.replaces` / `_meta.collapsedCalls` annotations on Claude `tool_result` blocks across reader → analyze → CLI, so replacement tools (e.g. relaywash) get attributed estimated tokens saved in `burn summary` and `burn summary --by-tool`.

## [1.7.0] - 2026-05-02

### Added

- `burn hotspots --patterns=tool-call-pattern` flags vanilla call patterns with consolidatable overhead (Glob → Grep → Read sequences, single-file edit clusters, `git status` / `pnpm test` / `gh pr` Bash calls), with per-occurrence counts and token-overhead estimates. Vendor-neutral — downstream tools map patterns to specific consolidations.
- `@relayburn/sdk` `hotspots()` now also surfaces `tool-output-bloat`, `ghost-surface`, and `tool-call-pattern` findings (previously only the core `detectPatterns` set).
- New `@relayburn/ingest` package owns session-store discovery, parse-and-append orchestration, pending-stamp resolution, and watch-loop primitives extracted from `@relayburn/cli`. CLI commands and harness adapters now consume ingest from `@relayburn/ingest`; `@relayburn/sdk` drops its `@relayburn/cli` dependency and imports `ingest()` from the new package and `buildGhostSurfaceInputs` from `@relayburn/analyze`.

## [1.5.0] - 2026-05-02

### Added

- `RELAYBURN_STORAGE=sqlite` selects a new single-file SQLite backend (default path `~/.relayburn/burn.sqlite`, override via `RELAYBURN_SQLITE_PATH`). Replaces JSONL ledger + sidecars + `.idx` files with one DB; ingest paths use native `INSERT OR IGNORE` on content-addressed dedup hashes so multi-writer setups converge without external indexes.

### Removed

- Removed Burn's budget and quota tracking surfaces: `burn budget`, monthly plan config APIs, and the MCP `burn__currentBlock` tool are no longer shipped.

## [1.2.2] - 2026-04-30

### Changed

- Publish workflow now always targets all lockstep packages and no longer exposes per-package release choices.

## [1.2.1] - 2026-04-30

### Added

- `burn overhead trim --json` now emits structured trim recommendations for programmatic review while preserving the existing unified diff text output.

## [1.1.0] - 2026-04-29

### Changed

- Publish workflow now creates one GitHub Release per lockstep publish, anchored to `relayburn`, with all package versions listed in the release body.

## [1.0.0] - 2026-04-29

### Added

- `burn ingest --watch --opencode-stream` now ingests stream-owned OpenCode sessions directly at completed tool-call grain while keeping file ingest as the fallback.

### Changed

- Renamed the attribution surface: `burn hotspots` replaces `burn waste` and `burn diagnose`, while `burn overhead` replaces `burn context`.
- `burn summary --subagent-tree` now renders persisted session relationship graphs while preserving legacy subagent-tree output for older data.

### Fixed

- OpenCode stream cursor progress now survives concurrent file-ingest fallback saves.

## [0.45.0] - 2026-04-29

### Added

- Defaulted user-turn block sizing to cl100k, with a measurement script that reports token-count and tool-attribution drift against the bytes/4 heuristic fallback.

### Fixed

- `burn rebuild --content` now fast-skips renamed Codex rollout files using their embedded session metadata.

## [0.43.0] - 2026-04-29

### Changed

- `burn waste --patterns` now bases retry and failure findings on persisted tool-result chronology when available, while preserving legacy fallback behavior.
- Spawn-env and native sidechain attribution now share session relationship records, and `burn diagnose --explain-drift` surfaces sessions where they disagree.
- Provider-aware CLI rendering now uses shared analyze helpers for effective-provider resolution and aggregation.
- Per-tool cost attribution now uses persisted user-turn block sizes in summary and hotspots reports, with rebuild backfill for historical sessions.

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
