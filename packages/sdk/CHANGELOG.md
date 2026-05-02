# Changelog

All notable changes to `@relayburn/sdk`.

## [Unreleased]

### Added

- `sessionCost({ session })` returns the compact per-session cost shape (`totalUSD`, `totalTokens`, `turnCount`, `models`) the MCP `burn__sessionCost` tool now wraps directly.
- `summary()` result now includes `turnCount`.
- `summary()` and `sessionCost()` read through the SQLite archive by default with transparent fallback to the JSONL ledger walk on archive failure. Pass `onLog` to capture the fallback reason.

## [1.7.0] - 2026-05-02

### Added

- `hotspots({ patterns })` now also surfaces `tool-output-bloat`, `ghost-surface`, and `tool-call-pattern` findings (previously only the core `detectPatterns` set). Each side-channel detector loads its own inputs (Claude settings, tool-result events, on-disk surface) lazily based on the requested patterns.

### Changed

- SDK no longer depends on `@relayburn/cli`. `ingest()` now imports from the new `@relayburn/ingest` package, and `buildGhostSurfaceInputs` lives in `@relayburn/analyze`. The SDK's public surface is unchanged.

## [1.5.0] - 2026-05-01

### Added

- Initial release with embedded `Ledger.open()`, `ingest()`, `summary()`, and `hotspots()` helpers.
