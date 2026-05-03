# Changelog

All notable changes to `@relayburn/sdk`.

## [Unreleased]

## [1.9.0] - 2026-05-03

### Added

- `compare({ models, … })` returns the per-(model, activity) `CompareResult` shape (`analyzedTurns`, `models`, `categories`, `totals`, flat `cells[]`, `fidelity { minimum, excluded, summary }`) — the JSON object `burn compare --json` now emits. Mirrors the CLI's archive-vs-ledger branching: archive when `minFidelity === 'partial'` and no provider filter, ledger walk otherwise. Falls back transparently to the ledger walk when the archive read fails.
- `sessionCost({ session })` returns the compact per-session cost shape (`totalUSD`, `totalTokens`, `turnCount`, `models`) the MCP `burn__sessionCost` tool now wraps directly.
- `summary()` result now includes `turnCount`.
- `summary()` and `sessionCost()` read through the SQLite archive by default with transparent fallback to the JSONL ledger walk on archive failure. Pass `onLog` to capture the fallback reason.
- `overhead({ project, since?, kind? })` returns per-file + per-section overhead cost attribution (the JSON shape `burn overhead --json` now consumes).
- `overheadTrim({ project, since?, kind?, top?, includeDiff? })` returns trim recommendations with projected savings and (by default) embedded unified diffs (the JSON shape `burn overhead trim --json` now consumes). Pass `includeDiff: false` to skip the per-file disk reads.
- `summary({ since })` and `overhead({ since })` / `overheadTrim({ since })` now accept either an ISO timestamp or a relative range (`24h`, `7d`, `4w`, `2m`); the SDK normalizes both forms before querying the ledger so direct SDK callers get the same forgiving input shape CLI users have. Previously a raw relative string would silently filter out every turn.

## [1.7.0] - 2026-05-02

### Added

- `hotspots({ patterns })` now also surfaces `tool-output-bloat`, `ghost-surface`, and `tool-call-pattern` findings (previously only the core `detectPatterns` set). Each side-channel detector loads its own inputs (Claude settings, tool-result events, on-disk surface) lazily based on the requested patterns.

### Changed

- SDK no longer depends on `@relayburn/cli`. `ingest()` now imports from the new `@relayburn/ingest` package, and `buildGhostSurfaceInputs` lives in `@relayburn/analyze`. The SDK's public surface is unchanged.

## [1.5.0] - 2026-05-01

### Added

- Initial release with embedded `Ledger.open()`, `ingest()`, `summary()`, and `hotspots()` helpers.
