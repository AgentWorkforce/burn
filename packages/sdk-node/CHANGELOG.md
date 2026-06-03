# @relayburn/sdk (2.x)

## [Unreleased]

### Changed

- `ingest()` returns in roughly source-walk time (`{ ingested: 0 }`) when nothing upstream has changed, instead of paying the full ~0.7s cursor-load + per-file scan every call. Gated on a stat-only source fingerprint persisted in the ledger. (#468)

## [3.0.0] - 2026-05-26

### Added

- `fingerprint({ session?, project?, ledgerHome? })` returns a
  `{count}:{maxMtimeUnix}:{totalBytes}` polling primitive over `turns`.
  Sub-millisecond per session-scope call; suitable for "did anything
  change" gates without re-querying or re-ingesting. (#440)

## [2.10.0] - 2026-05-24

### Added

- `hotspots()` attribution result carries a new `mcpServers` field —
  per-server rollup of `mcp__<server>__<tool>` tool calls with summed
  cost/tokens/ride-turns and top representative tool basenames. (#424)

## [2.9.0] - 2026-05-21

### Added

- `writeStamp({ sessionId | messageId, enrichment, ts?, ledgerHome? })` writes
  a stamp by exact selector — companion to `writePendingStamp` for launchers
  that preallocate the session id (Claude `--session-id`).

## [2.5.0] - 2026-05-08

### Added

- `hotspots()` options now accept `workflow` (folded `workflowId` enrichment
  filter) and `provider` (case-insensitive provider allow-list) — same shape
  the `compare()` options expose.

## [2.4.0] - 2026-05-08

### Breaking Changes

- Removed the `onLog` option from `summary`, `sessionCost`, `overhead`, `overheadTrim`, `hotspots`, and `compare` option types. The 2.x stack is SQLite-native and has no archive-fallback path to surface, so the callback was already a no-op at the napi boundary. (#374)

### Added

- Exported `writePendingStamp()` so Node launchers can write generic
  enrichment tags before spawning Claude, Codex, or OpenCode directly.
- `summary()` options now accept `tags` and `groupByTag` for generic
  enrichment filtering and cost/token grouping.
- Exported `computeCompareExcluded()` from the Node facade for callers that
  need the same fidelity-exclusion breakdown used by `compare()`.

### Changed

- Replaced the TypeScript 1.x deep-conformance test with native 2.x smoke
  coverage against the committed fixture ledger.

### Fixed

- `search()` now accepts a numeric `limit` in the Node facade and normalizes it
  before calling the napi-rs binding.

## [2.1.0] - 2026-05-07

### Added

- `summary()` result now includes `replacementSavings` — a rollup of per-tool collapsed-call counts and tokens-saved estimates derived from `_meta`-annotated tool results. Omitted (field absent) when no replacement-tool calls exist in the queried window.

## [2.0.0] - 2026-05-07

- Initial scaffolding: umbrella package layout (`@relayburn/sdk`) +
  per-platform packages (`@relayburn/sdk-{darwin-arm64,darwin-x64,linux-arm64-gnu,linux-x64-gnu}`)
  resolved via `optionalDependencies`, TS facade re-exporting the napi-rs
  binding, conformance scaffold against the TS 1.x SDK, esbuild bundle
  smoke test. (#247 part b)
- Shape conformance with TS `@relayburn/sdk@1.x`: `Ledger.open()` returns
  a `Promise<Ledger>` instance, `sessionCost()` emits `totalUSD`
  (screaming USD), every read verb is `async` (`Promise<T>`),
  `IngestOptions` is `{ sessionId, harness, ledgerHome }`, `top` and
  `minSample` accept plain `number`, and `onLog` callbacks are accepted
  on every read verb's options (silently dropped at the napi boundary
  until the SDK wires fallback logging). Adds `search`, `exportLedger`,
  `exportStamps`, `BurnErrorCode`, `OverheadFileKind`, and
  `HotspotsGroupBy` as 2.x extensions over the 1.x surface. (#247 part c)
- Umbrella facade now coerces napi-rs `BigInt` return values to `Number`
  for safe-range integers (`[Number.MIN_SAFE_INTEGER,
Number.MAX_SAFE_INTEGER]`), matching the TS 1.x runtime shape; values
  outside that range stay `BigInt` to avoid silent precision loss.
- Conformance suite is now wired into CI: `napi build` writes its outputs
  (`.node`, `binding.cjs`, `binding.d.ts`) into `src/` so the generated
  loader's local-file branch resolves; the suite was originally wired as a
  deep-equality gate against TS `@relayburn/sdk@1.x`. (#247 part d)
