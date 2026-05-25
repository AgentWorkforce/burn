# Changelog

All notable changes to `@relayburn/mcp`.

## [Unreleased]

### Added

- `createFingerprintTool()` factory + `burn__fingerprint` tool wrapping the
  SDK's new fingerprint primitive (`{count}:{maxMtimeUnix}:{totalBytes}`).
  Accepts optional `sessionId` / `project` to scope, mutually exclusive. (#440)

## [2.4.0] - 2026-05-08

### Changed

- Remove legacy TypeScript 1.x packages (#383)

## [2.0.0] - 2026-05-07

### Changed

- Drop `@relayburn/{reader, ledger, analyze}` from MCP entirely; `@relayburn/sdk` is now the only `@relayburn/*` package edge.

## [1.9.0] - 2026-05-03

### Changed

- `burn__sessionCost` is now a thin wrapper over `@relayburn/sdk`'s new `sessionCost()` function. The wire shape is unchanged (`sessionId`, `totalUSD`, `totalTokens`, `turnCount`, `models`, `note?`); the cost computation, archive-with-fallback strategy, and pricing snapshot all live in the SDK now, eliminating the duplicate query path that previously lived inside the MCP tool.
- `@relayburn/mcp` now depends on `@relayburn/sdk`. The package's role going forward is "MCP-shaped wrapper over the SDK's query surface" — new tools should call SDK functions rather than re-implementing computation against `@relayburn/analyze` / `@relayburn/ledger`.

## [1.6.2] - 2026-05-02

### Changed

- Bump package versions to 1.6.1

## [1.5.0] - 2026-05-02

### Removed

- Removed the `burn__currentBlock` quota forecast tool; MCP now exposes session cost lookup only.

## [1.2.1] - 2026-04-30

### Changed

- Remove issue refs and tidy comments

## [1.0.0] - 2026-04-29

### Fixed

- Fix budget command references

## [0.43.0] - 2026-04-29

### Changed

- Tighten changelog entries

## [0.33.0] - 2026-04-27

### Dependencies

- Version sync only. No package behavior changed.

## [0.30.0] - 2026-04-27

### Changed

- `burn__sessionCost` and `burn__currentBlock` now read from `archive.sqlite` by default, build the archive before each query, and fall back to the JSONL ledger on archive failure.

## [0.18.0] - 2026-04-26

### Fixed

- Fixed reasoning-token pricing in MCP tool responses.

## [0.13.1] - 2026-04-25

### Added

- Initial release with a read-only stdio MCP server.
- Added `startStdioServer()` and `buildMcpConfig()`.
- Added `burn__sessionCost` and `burn__currentBlock` tools.
