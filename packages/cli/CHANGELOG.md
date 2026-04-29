# Changelog

All notable changes to `@relayburn/cli`.

## [Unreleased]

### Added

- `burn ingest --runtime claude` now records hook-path tool-result events for Claude `PreToolUse`, `PostToolUse`, `SubagentStop`, and tool-tied `Notification` payloads.
- `burn watch --opencode-stream` can subscribe to OpenCode's local SSE endpoint and wake ingest immediately on session/message events while polling remains the fallback.

### Changed

- Provider filters and `burn summary --by-provider` now use the shared analyze provider resolver.
- `burn summary --by-tool` now uses persisted user-turn block sizes for proportional attribution and reports each JSON row's attribution method.
- `burn rebuild --content` now backfills missing user-turn rows for historical sessions, even when content sidecars already exist.

## [0.42.0] - 2026-04-28

### Added

- `burn waste --patterns ghost-surface` now recognizes Claude command markers and Codex slash prompts when content sidecars are available.
- Added `burn waste --patterns tool-output-bloat` for oversized tool output from Claude settings and observed tool-result events.
- Added `burn waste --patterns ghost-surface` for unused user-installed agents, skills, commands, prompts, rules, and memories.
- OpenCode ingest now persists compaction events.

## [0.41.0] - 2026-04-28

### Fixed

- Pending stamps are now claimed FIFO, preventing concurrent same-directory Codex or OpenCode runs from taking each other's enrichment.

### Added

- `burn diagnose` and `burn waste --patterns` now show content-sidecar details such as error signatures, lost work, and edit previews when available.
- `burn diagnose` without a session now reports content-capture gaps by adapter.
- Added `burn waste --patterns --findings` for one severity-ranked table across all detector types.

## [0.40.0] - 2026-04-28

### Added

- Added `burn archive vacuum` to reclaim unused space in `archive.sqlite`.

## [0.39.0] - 2026-04-28

### Changed

- `burn compare` now requires `burn compare <model_a,model_b[,...]>`. The old `--models` flag exits with guidance.

## [0.38.0] - 2026-04-28

### Changed

- `burn summary --by-tool` replaces `burn by-tool` and inherits the normal summary filters.

## [0.37.0] - 2026-04-28

### Added

- Added `burn waste --patterns edit-heavy` and per-session `burn diagnose` output for edit-heavy sessions.

## [0.36.0] - 2026-04-28

### Added

- Added `burn compare --provider <name>` for provider-filtered comparison tables.

## [0.35.0] - 2026-04-28

### Added

- `burn summary` now marks partial per-cell token coverage in text and JSON output.
- Codex ingest now persists compaction events.
- Added OpenCode skill-recall, skill-pruning, and system-prompt tax detectors to `burn waste --patterns` and `burn diagnose`.

### Changed

- `burn run <claude|codex|opencode>` replaces the separate harness wrappers and uses a shared adapter registry.

### Removed

- Removed `burn claude`, `burn codex`, and `burn opencode`. Use `burn run <name>`.
- Removed `burn rebuild-index`. Use `burn rebuild --index`.

## [0.34.0] - 2026-04-27

### Added

- `burn limits` now reports forecast fidelity when quota projections rely on partial token data.

### Changed

- `burn compare` now excludes low-fidelity turns by default. Use `--fidelity` or `--include-partial` to override.

## [0.33.0] - 2026-04-27

### Added

- `burn plans` now reports low-confidence cycles when token coverage is partial.
- `burn waste` now reports fidelity coverage and refuses analysis when required data is entirely missing.

### Changed

- `burn plans` reads spend from `archive.sqlite` by default, with `--no-archive` and `RELAYBURN_ARCHIVE=0` fallbacks.

## [0.31.0] - 2026-04-27

### Changed

- `burn summary` now reads from `archive.sqlite` by default and falls back to the JSONL ledger if the archive fails.

## [0.30.0] - 2026-04-27

### Changed

- `burn mcp-server` builds the archive at startup and before each tool query so MCP responses include fresh hook-ingested turns.

## [0.29.0] - 2026-04-26

### Added

- Codex and OpenCode wrappers now use pending stamps and live ingest while the child process runs.
- Added `burn watch [--interval <ms>] [--once]` for foreground passive ingest.

### Changed

- Codex session discovery now falls back to `session_meta.payload.id` when the filename has no UUID.

## [0.28.0] - 2026-04-26

### Added

- Added synthetic provider filters to `burn summary`, `burn by-tool`, and `burn waste`.
- Added `burn summary --by-provider`.

## [0.27.0] - 2026-04-26

### Changed

- Ingest now persists parser-emitted user-turn records for Claude, Codex, and OpenCode.

## [0.26.0] - 2026-04-26

### Added

- Codex ingest now persists session relationships and tool-result events.

### Changed

- `burn compare` reads from `archive.sqlite` by default, with `--no-archive` and `RELAYBURN_ARCHIVE=0` fallbacks.

## [0.25.0] - 2026-04-26

### Added

- `burn archive status` and archive build summaries now include tool-result event counts.

## [0.24.0] - 2026-04-26

### Added

- `burn archive status --json` now includes a turn fidelity histogram.

## [0.23.0] - 2026-04-26

### Added

- OpenCode ingest now persists session relationships and tool-result events.

## [0.22.0] - 2026-04-26

### Changed

- Claude ingest now reconciles cross-file fork and continuation relationships after each pass.

## [0.21.0] - 2026-04-26

### Added

- Ingest now persists parser-emitted user-turn records.

## [0.20.0] - 2026-04-26

### Added

- Added `burn archive build | rebuild | status` for the derived analytics archive.

## [0.19.0] - 2026-04-26

### Added

- Claude ingest now persists session relationships and tool-result events.

## [0.17.0] - 2026-04-25

### Changed

- Ingest now surfaces parser content-capture gaps.

## [0.15.1] - 2026-04-25

### Added

- Harness wrappers now pass the spawner-owned `RELAYBURN_*` environment contract.

## [0.15.0] - 2026-04-25

### Changed

- Content pruning now preserves recoverable sidecars.

## [0.14.2] - 2026-04-25

### Changed

- Dominant even-split waste attribution now renders as a banner instead of a quiet note.

## [0.14.0] - 2026-04-25

### Added

- Added CLI support for turn coverage and fidelity metadata.

## [0.13.1] - 2026-04-25

### Added

- Added `burn archive build | rebuild | status`.
- Added `burn mcp-server` for read-only in-session cost and quota queries.

## [0.11.0] - 2026-04-25

### Added

- Added `burn plans` for monthly plan budgets and projections.

### Changed

- Plan status loading now fails softly instead of breaking the CLI.

## [0.10.0] - 2026-04-24

### Added

- Added `burn limits` for Claude quota-window tracking and local usage forecasts.
- Added `burn plans` commands for monthly plan budgets.
- `burn limits` now includes configured monthly plan status.

## [0.9.0] - 2026-04-24

### Added

- Added subagent tree summary commands.

## [0.8.0] - 2026-04-24

### Added

- Added `burn ingest --runtime claude [--quiet]` for Claude hook-driven ingest.
- Added `burn summary --subagent-tree <session-id>`.
- Added `burn summary --by-subagent-type`.

## [0.7.0] - 2026-04-24

### Added

- Codex and OpenCode sessions now write content sidecars when full content capture is enabled.
- Added `burn rebuild --content` for content sidecar backfills.

## [0.6.0] - 2026-04-24

### Added

- Added `burn summary --quality`.
- Added waste-pattern detector output for retry loops, failure runs, compaction loss, and edit reverts.

## [0.5.0] - 2026-04-24

### Changed

- Moved earlier unreleased notes into release sections. No CLI behavior changed.

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
- Retained `burn rebuild-index` as an alias for `burn rebuild --index`.

### Changed

- CLI help now lists all `burn compare` filters.

## [0.1.0] - 2026-04-22

### Added

- Initial `burn` CLI with `summary`, `by-tool`, harness wrappers, `content prune`, and `rebuild-index`.
