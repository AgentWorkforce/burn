# Changelog

All notable changes to `@relayburn/reader`.

## [Unreleased]

### Added

- Claude reader now back-populates `_meta.replaces` / `_meta.collapsedCalls` annotations from `tool_result` blocks onto the matching `ToolCall.replacedTools` / `ToolCall.collapsedCalls` and the corresponding `ToolResultEventRecord`, so replacement tools (e.g. relaywash) carry counterfactual attribution through to analyze.

## [1.4.0] - 2026-05-01

### Added

- Added `parseBashCommand()` for normalized Bash verb extraction from shell command strings.

## [1.2.1] - 2026-04-30

### Changed

- Remove issue refs and tidy comments

## [1.0.0] - 2026-04-29

### Added

- OpenCode stream events can now be normalized into turn, content, user-turn, relationship, and tool-result event records for stream-owned sessions.

### Changed

- OpenCode tool-result events now include a conservative per-tool share of the assistant turn's usage when turn-level usage is available.

### Fixed

- OpenCode stream ingestion now keeps tool-result event indexes stable across repeated flushes and clears buffered parts for deleted sessions.

## [0.45.0] - 2026-04-29

### Added

- Added `readCodexSessionIdHint()` for cheap Codex first-line `session_meta` session-id discovery.
- Claude, Codex, and OpenCode parsers now default user-turn token estimates to lazily loaded `@dqbd/tiktoken` cl100k, with `tokenizer: 'heuristic'` available as a cheap fallback.

## [0.43.0] - 2026-04-29

### Changed

- Claude and OpenCode subagent relationship rows now identify native parent-signal sources separately from parser root/session records.
- Codex and Claude parsers now capture additional native fork, continuation, and subagent notification execution-graph signals.

## [0.42.0] - 2026-04-28

### Added

- OpenCode parsers now emit `CompactionEvent`s for compaction parts and dedupe them across incremental parses.

## [0.35.0] - 2026-04-28

### Added

- Codex parsers now emit `CompactionEvent`s for `compacted` records.
- OpenCode skill tool calls now populate `ToolCall.skillName`.

## [0.33.0] - 2026-04-27

### Dependencies

- Version sync only. No package behavior changed.

## [0.26.0] - 2026-04-26

### Added

- Codex parsers now emit session relationships and tool-result events.

## [0.23.0] - 2026-04-26

### Added

- OpenCode parsers now emit session relationships and tool-result events.

## [0.22.0] - 2026-04-26

### Added

- Claude parsers now emit fork and continuation relationship evidence.
- Codex parsers now populate `TurnRecord.fidelity`.
- OpenCode parsers now populate `TurnRecord.fidelity`.

## [0.19.0] - 2026-04-26

### Added

- Added `SessionRelationshipRecord` and `ToolResultEventRecord`.
- Claude parsers now emit execution-graph records.

## [0.16.0] - 2026-04-25

### Added

- Claude parsers now emit `UserTurnRecord`s with block sizes, tool-use ids, and adjacent message ids.

## [0.14.0] - 2026-04-25

### Added

- Added optional `TurnRecord.fidelity` metadata plus `EMPTY_COVERAGE`, `classifyFidelity()`, and `makeFidelity()`.
- Claude parsers now populate fidelity metadata.

## [0.13.1] - 2026-04-25

### Changed

- Version sync only. No package behavior changed.

## [0.9.0] - 2026-04-24

### Added

- Claude parsers now reconstruct subagent trees from `parentUuid` chains and populate expanded `TurnRecord.subagent` fields.

## [0.8.0] - 2026-04-24

### Changed

- Version sync for Claude hook-based ingest. No reader behavior changed.

## [0.7.0] - 2026-04-24

### Added

- Codex and OpenCode parsers now emit `ContentRecord`s when full content capture is enabled.

## [0.6.0] - 2026-04-24

### Added

- Reader output now includes the error and retry signals used by waste-pattern detectors.

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

- Activity classification now covers Codex and OpenCode turns.
- Added tool-name normalization via `TOOL_ALIASES` and `normalizeToolName()`.
- Added activity categories for reasoning, docs, dependencies, formatting, review, and verification.
- Added retry-based debugging classification and broader test-command detection.

### Changed

- Tightened build and deploy classifier patterns.

## [0.1.0] - 2026-04-22

### Added

- Initial release with Claude, Codex, and OpenCode parsers; activity classification; project resolution; and tool-call fingerprinting.
