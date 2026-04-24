# Changelog

All notable changes to `@relayburn/analyze` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Subagent tree + per-type statistics.** `buildSubagentTree(turns, { pricing })` returns a per-session `SubagentTreeNode` hierarchy: main thread at the root, subagent invocations nested by `parentAgentId`, with `selfCost` / `selfTurns` per node and `cumulativeCost` / `cumulativeTurns` rolled up from leaves. Sidechain turns that arrived without resolvable tree fields attach under a synthetic `(unresolved)` node so their cost isn't dropped. `aggregateSubagentTypeStats(turns, { pricing })` reports invocations, turns, total / median / p95 / mean cost per `subagentType` across sessions (counted once per unique `sessionId + agentId`, not per turn). New exported types: `SubagentTreeNode`, `SubagentTypeStats`, `BuildSubagentTreeOptions`. Consumes the new `TurnRecord.subagent` fields from `@relayburn/reader`. Closes [#8](https://github.com/AgentWorkforce/burn/issues/8).

## [0.6.0] - 2026-04-24

### Added

- **Quality signals module.** `computeQuality(turns, opts)` returns two orthogonal per-session signals — `SessionOutcome` (outcome inference) and `OneShotMetrics` (one-shot rate) — for answering "was this work good enough that a cheaper model could have done it." Closes [#6](https://github.com/AgentWorkforce/burn/issues/6). Also exported individually as `inferOutcome` and `computeOneShotRate`.
  - **Outcome inference** classifies each session as `completed` / `abandoned` / `errored` / `unknown` with explicit `high` / `medium` / `low` confidence and a reason code (`single-exchange`, `too-short`, `recent`, `user-ended`, `user-ended-long`, `failure-streak`, `give-up`, `assistant-ended`, `unknown-ending`, `empty`). Works from turn metadata alone; an optional `contentBySession` map downgrades `assistant-ended` to `give-up/low` when the last assistant text matches known give-up phrases (e.g. `"i'm unable to"`, `"i cannot access"`, `"doesn't appear to exist"`).
  - **One-shot rate** is `oneShotTurns / editTurns` per session, where a one-shot turn is an edit turn with zero retries. Sidechain (subagent) turns are excluded from the denominator so their retry counts don't poison the parent session's rate. Also returns `totalRetries` as a raw volume signal.
  - Computed lazily at query time, never persisted to the ledger — upgrading the rules later does not require a rebuild. Requires no prompt storage; the give-up downgrade runs opportunistically when content is available.
  - Handles sources that don't record `stopReason` (e.g. Codex): the final-turn ending role is reported as `'unknown'` and the session is classified `completed/low` with reason `unknown-ending` rather than being swept into `abandoned`.
- **Waste-pattern detectors** — retry loops, failure runs, compaction loss, edit-revert. Closes [#11](https://github.com/AgentWorkforce/burn/issues/11).

## [0.5.0] - 2026-04-24

### Changed

- Clean up changelogs: move [Unreleased] content into 0.3.0/0.4.0 sections

## [0.4.0] - 2026-04-23

### Added

- **Per-tool-call & per-file cost attribution module.** `attributeWaste(turns, {pricing, contentBySession})` returns a per-`tool_use_id` ledger of initial cost (the turn after a tool call, where the result enters context as `input`/`cacheCreate`) plus persistence cost (subsequent turns where it rides along in `cacheRead` until evicted). Sized when content sidecar is available (estimates each tool_result's tokens from its text length); falls back to even-split (initial only) when it isn't.
- `aggregateByFile`, `aggregateByBash`, `aggregateBySubagent` — collapse the attribution ledger to ranked top-N tables for `Read`/`Edit`/`Write`/`NotebookEdit` (by target path), `Bash` (by `argsHash` so repeated commands collapse), and `Agent`/`Task` (by `subagent_type`).
- Attribution honors per-paying-turn model rates: initial cost uses turn N+1's rate and (input + cacheCreate) mix; persistence cost uses each ride-along turn's own rate. Sessions that switch models mid-stream are priced correctly.
- Sibling normalization: when multiple tool_results enter on the same turn, their summed `initialTokens` are capped at the turn's actual `newContent` and split proportionally by size. Persistence likewise allocates each turn's `cacheRead` proportionally across all still-cached results so the per-turn sum never exceeds the actual cached tokens.

## [0.3.0] - 2026-04-23

### Added

- **CLAUDE.md cost attribution module.** `parseClaudeMd(path, text)` / `loadClaudeMdFile(path)` / `findClaudeMdFiles(projectPath)` resolve a project's CLAUDE.md set (root and `.claude/CLAUDE.md`) and split it into sections at the H2 level (H1 fallback), skipping headings inside fenced code blocks.
- `attributeClaudeMd({files, turns, pricing})` — computes per-session cost as `claude_md_tokens × cacheReadPrice` for every turn whose `cacheRead` is large enough to hold the file. Returns per-section costs proportional to section byte share (strictly additive, so `Σ sectionCost ≤ totalCost`). Includes zero-cost sessions in `sessionCount` / `perSessionAvg` / `perSessionP95` so stats cover the whole query window rather than only cache-hit sessions.
- `buildAdviseRecommendations(attribution, topN)` + `renderUnifiedDiffForRecommendation(path, text, rec, baseDir?)` — emit read-only unified-diff TRIM hunks for the most expensive non-preamble sections. Paths render POSIX-relative when a `baseDir` is given so the diff applies with standard patch tooling.
- **Multi-harness context-file attribution.** New `findContextFiles(projectPath)` discovers `CLAUDE.md`, `.claude/CLAUDE.md`, and `AGENTS.md`, each tagged with the `SourceKind[]` it applies to. `attributeContext({files, turns, pricing})` routes turns to files by `source` so Claude Code sessions pay for `CLAUDE.md`, Codex and OpenCode sessions pay for `AGENTS.md`, and neither cross-attributes. Per-file attribution returns one `ClaudeMdAttributionResult` each, plus a grand total across all files.

### Fixed

- `parseClaudeMd` normalizes CRLF → LF and strips a single trailing newline so `totalLines` and section `endLine` match what an editor shows. Empty input returns zero sections.
- Strict CommonMark fence-close matching: a line must contain only fence characters (length ≥ opening run) plus optional whitespace to close. A `` ````python `` line nested inside a 3-backtick block no longer closes the fence and corrupts section boundaries.
- `attributeContext` deduplicates per-session `totalRidingTurns` using max-per-session rather than summing across files, so a session that reads multiple context files isn't double-counted.

## [0.2.0] - 2026-04-23

### Added

- **`buildCompareTable(turns, opts)`** — bucket turns by `(model, activity)` and emit a `CompareTable` with per-cell metrics: `turns`, `editTurns`, `oneShotTurns`, `pricedTurns`, `totalCost`, `costPerTurn`, `oneShotRate`, `cacheHitRate`, `medianRetries`, plus `noData` / `insufficientSample` flags. Sorts models by total cost descending, categories by total turns descending. Filters: `models[]`, `minSample`.
- **`DEFAULT_MIN_SAMPLE`** export (defaults to 5).
- `CompareCell.pricedTurns` distinguishes "cost is zero because the model is free" from "cost is unknown because we have no pricing for this model" — `costPerTurn` is `null` (renders as `—`) when no priced turns, never silently `$0.00`.
- `CompareCell.noData` is mutually exclusive with `insufficientSample` so consumers can tell "we never saw this combination" apart from "we have data but the sample is small."
- `--models` filter pre-seeds requested models so a model the user explicitly asked about stays visible (as an all-empty column with coverage notes) even when zero turns matched.

## [0.1.0] - 2026-04-22

### Added

- **Initial release.** Pricing loader and per-record cost derivation.
- `loadBuiltinPricing()` / `loadPricing()` — vendored models.dev snapshot with optional user override at `$RELAYBURN_HOME/models.dev.json`.
- `costForTurn(turn, pricing)` / `costForUsage(usage, model, pricing)` — per-turn cost breakdown (`input`, `output`, `reasoning` at output rate, `cacheRead`, `cacheCreate`).
- `sumCosts(costs[])` aggregator.
- Provider-prefix fallback in lookup so `anthropic/claude-sonnet-4-6` resolves to the `claude-sonnet-4-6` rate.
