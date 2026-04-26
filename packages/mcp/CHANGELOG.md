# Changelog

All notable changes to `@relayburn/mcp` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **`burn__sessionCost` and `burn__currentBlock` query the analytics archive on the hot path** ([#97](https://github.com/AgentWorkforce/burn/issues/97)). Both tool handlers now default to `queryTurnsFromArchive` (a single SQL query against `archive.sqlite`) instead of folding the entire JSONL ledger on every MCP call. Tool responses are equivalent to the pre-migration implementation within float-rounding tolerance for cost. The CLI's `burn mcp-server` invokes an incremental `buildArchive()` once at startup so the first tool call hits SQL, not a ledger walk. If the archive cannot be opened or the query throws, both handlers transparently fall back to `queryAll` and emit a one-line note via the new `onLog` dependency hook (the CLI server wires this to stderr) so persistent breakage is visible without ever refusing to serve.

## [0.18.0] - 2026-04-26

### Fixed

- Fix reasoning-token pricing semantics, preserve models.dev reasoning tariffs (#32)

## [0.13.1] - 2026-04-25

### Added

- Initial release: MCP (Model Context Protocol) stdio server exposing read-only burn ledger queries for in-session self-query by the running agent (#26).
- `startStdioServer({sessionId?, tools?, ...})` — minimal JSON-RPC 2.0 MCP server over stdio. No external SDK dependency.
- `buildMcpConfig({sessionId, burnBin?})` — returns a JSON string for Claude Code's `--mcp-config` flag. Registers `burn mcp-server --session-id <id>` as the `burn` server.
- Built-in tools:
  - `burn__sessionCost({ sessionId? })` — returns `{totalUSD, totalTokens, turnCount, models}` for the session. Session id defaults to the `--session-id` baked into the server at spawn time.
  - `burn__currentBlock({ sessionId? })` — returns the Claude OAuth-reported 5-hour `percentUsed` plus a locally-forecast `burnRateTokensPerMin`, `projectedBlockTotal`, and `minutesToReset` with a coarse `advice` label.
- Read-only by construction: no MCP tool writes to the ledger.

