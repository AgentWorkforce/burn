# Changelog

All notable changes to `@relayburn/mcp` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.13.1] - 2026-04-25

### Added

- Initial release: MCP (Model Context Protocol) stdio server exposing read-only burn ledger queries for in-session self-query by the running agent (#26).
- `startStdioServer({sessionId?, tools?, ...})` — minimal JSON-RPC 2.0 MCP server over stdio. No external SDK dependency.
- `buildMcpConfig({sessionId, burnBin?})` — returns a JSON string for Claude Code's `--mcp-config` flag. Registers `burn mcp-server --session-id <id>` as the `burn` server.
- Built-in tools:
  - `burn__sessionCost({ sessionId? })` — returns `{totalUSD, totalTokens, turnCount, models}` for the session. Session id defaults to the `--session-id` baked into the server at spawn time.
  - `burn__currentBlock({ sessionId? })` — returns the Claude OAuth-reported 5-hour `percentUsed` plus a locally-forecast `burnRateTokensPerMin`, `projectedBlockTotal`, and `minutesToReset` with a coarse `advice` label.
- Read-only by construction: no MCP tool writes to the ledger.

