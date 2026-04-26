# Changelog

All notable changes to `@relayburn/analyze` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.18.0] - 2026-04-26

### Fixed

- **Reasoning-token pricing semantics** (#32). Two correctness bugs that distorted reported spend whenever reasoning tokens were involved:
  - Codex `usage.reasoning` was double-billed at the output rate even though Codex's `output_tokens` already includes reasoning. `burn` now treats Codex turns as `included_in_output` and bills `output` only. On a 10-turn Codex sample (660k input / 53k output / 29k reasoning / 5.6M cacheRead), this drops the reported cost from $4.282607 to $3.846557 — about 11% off the Codex slice.
  - `cost.reasoning` from the `models.dev` snapshot was discarded during `flatten()`, so any model with a distinct reasoning tariff (e.g. Alibaba Qwen reasoning models) couldn't be priced correctly. The flattener now preserves `reasoning` and tags the entry `reasoningMode: 'separate'`; `costForUsage` honors the distinct tariff.
- **Waste-attribution session totals now honor the same reasoning-mode semantics** as `costForTurn`. `attributeWaste` previously had a private `costForTurnLocal` that unconditionally billed reasoning at the output rate, which double-billed Codex turns and ignored separate reasoning tariffs in `sessionGrand` / `grandCost` / `unattributedCost`. It now delegates to `costForTurn`, so waste totals match `cost.ts` for any session involving reasoning tokens (Devin review on #73).

### Added

- `ModelCost.reasoningMode: 'included_in_output' | 'separate' | 'same_as_output'` and optional `reasoning` per-million tariff. `ReasoningMode` and `CostForUsageOptions` are exported.
- `costForUsage(usage, model, pricing, { reasoningMode })` accepts an explicit override. `costForTurn` infers `included_in_output` for `source: 'codex'` automatically.
- `flatten` is now exported so callers can build `PricingTable`s from in-memory `models.dev` payloads.

## [0.14.0] - 2026-04-25

### Added

- **`summarizeFidelity(turns)` and `hasMinimumFidelity(fidelity, minimum)`** ([#41](https://github.com/AgentWorkforce/burn/issues/41) — first cut). `summarizeFidelity` walks a slice of turns and returns a `FidelitySummary` with totals broken down by `class`, by `granularity`, and per-field `missingCoverage` counts plus an `unknown` bucket for records emitted before `TurnRecord.fidelity` existed. `hasMinimumFidelity` is the predicate behind a future "default exclude aggregate-only / cost-only" filter for `burn compare` and friends; treats `undefined` fidelity as passing for backward compat. Pure functions — no I/O, safe to call repeatedly.

## [0.13.1] - 2026-04-25

### Added

- **Synthetic provider reattribution layer (#31).** `resolveProvider(model, rules?)` returns a `{ provider, normalizedModel, matchedRule }` for Synthetic-routed model IDs — the cross-collector reattribution pattern used when a Claude Code or OpenCode session uses a model dispatched through Synthetic.new. First pass covers three prefix shapes (`hf:*`, `accounts/fireworks/models/*`, `synthetic/*`) and exposes `DEFAULT_RULES` plus a `ProviderRule` type so future aggregators (OpenRouter, etc.) plug in via the same scaffolding. Pricing lookup in `costForTurn`, `attributeWaste`, and `attributeClaudeMd`/`attributeContext` all consult the reattribution layer before falling back to the existing `provider/model` strip, so a turn logged with `hf:deepseek-ai/deepseek-r1` resolves to the `deepseek-r1` rate instead of returning `null` across summary, waste, and context views. Reattribution stays query-time only — raw model strings are never mutated in the ledger. Octofriend SQLite fallback and other aggregator prefixes are deferred to follow-up issues.

## [0.11.0] - 2026-04-25

### Added

- **`computePlanUsage(plan, turns, { pricing, now })`** (#39) — aggregates spend over a plan's current cycle and returns a `PlanUsage`: `spentUsd`, `daysElapsed`, `daysInCycle`, `projectedEndOfCycleUsd` (linear extrapolation from observed rate), `overBudget`, `runwayDays` (days of budget left at the current daily rate, only populated when the projection exceeds budget), `resetAt`, and `limitedData` (true when fewer than 7 days have elapsed in the cycle, so renderers can mark projections as low-confidence per the issue's acceptance). Provider-aware filtering: `claude` plans count `claude-code` + `anthropic-api` turns, `cursor` plans count `cursor` turns (no reader emits these yet — see SourceKind), `custom` plans count every turn.
- **`cycleBounds(resetDay, now)`** — exposed for callers that need the cycle window without a full `PlanUsage`. UTC-anchored, clamps `resetDay > 28` to the actual last-day-of-month so February with `resetDay: 31` resolves to Feb 28/29, handles year-boundary crossings, and never returns a zero-length cycle.

## [0.9.0] - 2026-04-24

### Added

- **Subagent tree + per-type statistics.** `buildSubagentTree(turns, { pricing })` returns a per-session `SubagentTreeNode` hierarchy: main thread at the root, subagent invocations nested by `parentAgentId`, with `selfCost` / `selfTurns` per node and `cumulativeCost` / `cumulativeTurns` rolled up from leaves. Sidechain turns that arrived without resolvable tree fields attach under a synthetic `(unresolved)` node so their cost isn't dropped. `aggregateSubagentTypeStats(turns, { pricing })` reports invocations, turns, total / median / p95 / mean cost per `subagentType` across sessions (counted once per unique `sessionId + agentId`, not per turn). New exported types: `SubagentTreeNode`, `SubagentTypeStats`, `BuildSubagentTreeOptions`. Consumes the new `TurnRecord.subagent` fields from `@relayburn/reader`. Closes [#8](https://github.com/AgentWorkforce/burn/issues/8).

## [0.8.0] - 2026-04-24

### Added

- **Add Claude hook-based ingest and settings**

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
