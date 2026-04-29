# Changelog

All notable changes to `@relayburn/mcp`.

## [Unreleased]

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
