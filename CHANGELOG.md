# Changelog

Cross-package narrative for the relayburn monorepo. The per-package CHANGELOGs at `packages/*/CHANGELOG.md` are authoritative for exactly what shipped in each package; this file is the unified view.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) and the workspace adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Packages are released in lockstep, so each version below applies to all five (`reader`, `ledger`, `analyze`, `mcp`, `cli`).

## [Unreleased]

## [0.18.0] - 2026-04-26

### Fixed

- **Reasoning-token pricing semantics** (#32). User-visible cost numbers will change downward for any session with non-zero reasoning tokens — most notably Codex sessions, where reasoning was being billed twice (once inside `output_tokens` and again on top via `usage.reasoning`). On the documented 10-turn Codex sample the reported cost drops from $4.282607 to $3.846557 (~11.3%). Models with a distinct reasoning tariff in `models.dev` (e.g. Alibaba Qwen reasoning models) are now priced correctly instead of falling through at the output rate. The reader-level `usage.reasoning` field is unchanged — the bug was in pricing, not data capture. See `packages/analyze/CHANGELOG.md` for the full breakdown.

## [0.13.1] - 2026-04-25

### Added

- **`@relayburn/mcp` package + `burn mcp-server`** (#26). Closes the loop between observation and decision: a running agent can self-query its own cost and quota state mid-session via MCP and adjust behavior (downgrade model, defer expensive subagent, abort) before hitting the 5-hour wall. None of the surveyed competitors do this — ccusage's MCP is for user-query, not agent-self-query.
  - `@relayburn/mcp` (new) — minimal JSON-RPC 2.0 stdio MCP server with no external SDK runtime dep. Exports `startStdioServer`, `buildMcpConfig({sessionId})`, `createSessionCostTool`, `createCurrentBlockTool`. Registers `burn__sessionCost` and `burn__currentBlock`. Read-only by construction.
  - `@relayburn/cli` — `burn mcp-server [--session-id <uuid>]` subcommand. Spawners use `buildMcpConfig` to inject the server into `claude --mcp-config <…>` so the registered tools default to the running session.

## [0.11.0] - 2026-04-25

### Added

- **Plan-based monthly quota tracking** (#39). Complement to `burn limits`'s 5-hour OAuth window: track spend against monthly plan budgets (Claude Pro/Max, Cursor Pro, or any custom plan) and surface projected end-of-cycle spend, runway-days-at-current-rate, and over/under-budget delta. Configure with `burn plans add --provider claude --preset max`; the status flows automatically into `burn limits` output as a `Monthly plan` block alongside the 5-hour view. Projections with fewer than 7 days of cycle data are marked `(limited data)` so users don't anchor on first-week noise. Plans persist to `~/.relayburn/plans.json` and respect a custom `resetDay` 1-31 (with end-of-month clamping for short months).
  - `@relayburn/ledger` — `Plan` / `PlansFile` / `PlanProvider` types, `loadPlans` / `savePlans`, `BUILTIN_PRESETS`, `findPreset`, `normalizePlan`, `plansPath`.
  - `@relayburn/analyze` — `computePlanUsage(plan, turns, { pricing, now })` returning a fully-derived `PlanUsage` (spend, projection, runway, limited-data flag); `cycleBounds(resetDay, now)` exposed for callers that just need the window.
  - `@relayburn/cli` — `burn plans` subcommand (list / add / remove / set-reset-day), plan-status block wired into `burn limits` TTY + `--json` output.

## [0.9.0] - 2026-04-24

### Added

- **Subagent tree as a first-class primitive** (#8). Replaces the flat `subagent.isSidechain` boolean with a parent→child tree reconstructed from Claude JSONL. `TurnRecord.subagent` gains `agentId`, `parentAgentId`, `parentToolUseId`, `subagentType`, `description`. New `buildSubagentTree(turns, { pricing })` and `aggregateSubagentTypeStats` in analyze. New `burn summary --subagent-tree <session-id>` and `burn summary --by-subagent-type` (both `--json`-able).

## [0.8.0] - 2026-04-24

### Added

- **Hook-based Claude Code ingest** (#7). Spawners install burn's ingest hooks per-invocation via Claude Code's `--settings` flag — no global `~/.claude/settings.json` mutation. New `buildClaudeHookSettings({ burnBin? })` in ledger. New `burn ingest --runtime claude [--quiet]` reads hook payloads on stdin; safe to fire on every event, hook failures never propagate non-zero back to Claude Code.

## [0.7.0] - 2026-04-24

### Added

- **Content capture for Codex and OpenCode parsers** (#33 follow-up). Both parsers now emit `ContentRecord` entries when `contentMode === 'full'`, matching the Claude shape — `text`, `thinking` (Codex reasoning), `tool_use`, and `tool_result` keyed by the same `call_id` / `callID` the tool call carries. Codex commits content only at `task_complete`; uncommitted content re-emits when the turn commits.
- **`burn rebuild --content`** — re-parses source session files to populate missing content sidecars. Skips sessions with content on disk; leaves cursors and ledger rows untouched.
- **`listContentSessionIds()`** — ledger helper enumerating sessionIds with non-empty content sidecars on disk.

## [0.6.0] - 2026-04-24

### Added

- **Quality signals: outcome inference + one-shot rate** (#6). New `computeQuality(turns, opts)` returns `SessionOutcome[]` (`completed` / `abandoned` / `errored` / `unknown` with confidence + reason code) and `OneShotMetrics[]` (edit turns / one-shot edit turns / retry volume, sidechain-excluded). `burn summary --quality` appends outcome counts and weighted one-shot rate. Lazy at query time. Sources without `stopReason` (Codex) classify `completed/low` rather than `abandoned`.
- **Waste-pattern detectors** (#11) — retry loops, failure runs, compaction loss, edit-revert.

## [0.5.0] - 2026-04-24

### Changed

- Housekeeping bump only — folds prior `[Unreleased]` content into the correct historical release sections. No functional changes in any package.

## [0.4.0] - 2026-04-23

### Added

- **`burn waste`** (#3) — per-tool-call and per-file cost attribution. Ranks tool calls / files / Bash commands / subagents by attributed cost: **initial** (turn after the call, where the result enters context as fresh `input` / `cacheCreate`) plus **persistence** (every subsequent turn it rides along in `cacheRead` until evicted). Sized attribution from the content sidecar when available; even-split fallback (initial only) when not, with a printed note. Per-paying-turn model rates so cross-model sessions price correctly. Sibling normalization across simultaneously-entering tool_results so attributed cost never exceeds what was actually paid.

## [0.3.0] - 2026-04-23

### Added

- **`burn context`** (#10) — cost attribution for agent context files (`CLAUDE.md`, `.claude/CLAUDE.md`, `AGENTS.md`). Per-file ranked section tables plus a grand total across all discovered files. `--kind claude-md|agents-md` narrows by file kind. Each file is attributed only against turns whose harness reads it (Claude Code for CLAUDE.md; Codex / OpenCode for AGENTS.md). Section parser groups at H2 (H1 fallback), treats top-level content as preamble, skips headings inside fenced code blocks.
- **`burn context advise`** — emits read-only unified-diff TRIM hunks for the most expensive sections across every discovered file. POSIX-relative paths apply with `git apply` / `patch`. No `--apply` flag — burn never mutates your context files.

### Fixed

- `attributeContext` deduplicates `totalRidingTurns` per session via max-per-session rather than summing across files, so a session that reads multiple context files isn't double-counted.

## [0.2.0] - 2026-04-23

### Added

- **`burn compare`** (#38) — observed-data comparison of cost-per-turn, one-shot rate, and turn count by `(model, activity)`. The headline meta-goal query: "which model handled each activity category best in the work I actually did?" Flags: `--models a,b`, `--since 7d`, `--project`, `--session`, `--workflow`, `--agent`, `--min-sample <n>`, `--json`, `--csv`. Grouped TTY table; missing-data cells render `—` (never `$0.00` or `0%`). JSON / CSV expose `noData` / `insufficientSample` / `pricedTurns`.
- **`burn rebuild --reclassify [--force]`** — backfills `activity` labels on existing ledger turns by re-running `classifyActivity`. Default skips already-classified; `--force` reclassifies everything. `--index` rebuilds the sidecar; both can run together. `burn rebuild-index` retained as alias.
- **Activity classifier runs for Codex and OpenCode turns** — previously Claude-only, which left every other harness in `unclassified` and made cross-harness comparison impossible.
- **`TOOL_ALIASES` + `normalizeToolName(name)`** — collapses harness-specific tool names (`apply_patch`, `exec_command`, `shell`, codex agent tools, lowercased OpenCode names) onto canonical Claude names so the rule tables stay single-source.
- **Six new activity categories** (taxonomy 12 → 18): `reasoning`, `docs`, `deps`, `format`, `review`, `verification`.
- **`buildCompareTable(turns, opts)`** — pure aggregator in analyze so scripts can consume the same shape the CLI renders.

### Fixed

- **Race between `appendTurns` and `reclassifyLedger`** (#48). `appendLines` now acquires the same `withLock('ledger', …)` that reclassify holds; previously an `appendFile` landing between reclassify's `readFile` and `rename` would write to a soon-orphaned inode and silently disappear when rename swapped the new file in.

### Changed

- `BUILD_DEPLOY_PATTERNS` tightened — replaces catch-all `/\bdeploy\b/` with explicit verbs per-tool (`vercel/netlify/flyctl/railway/sst deploy|up`, `kubectl apply/rollout/set`, `helm install/upgrade`, `terraform apply/plan/destroy`, `make build|release|dist|package|deploy`).
- `burn compare` rejects `--json` + `--csv` together with exit 2 instead of silently picking JSON.
- `--models` filter pre-seeds requested models so a model explicitly asked about stays visible (all-empty column with coverage note) when zero turns matched.

## [0.1.0] - 2026-04-22

### Added

- **Initial release.** Four packages.
  - **`@relayburn/reader`** — pure parsers turning agent session logs into `TurnRecord[]` and `ContentRecord[]`. Claude Code, Codex (with cumulative-token-delta accounting), OpenCode (per-session/message/part storage layout). All three have `*SessionIncremental` variants for cursor-based ingest. Activity classifier with 12 categories (Claude Code only at this release). Git-canonical project resolution (`resolveProject`) so `projectKey` survives across worktrees.
  - **`@relayburn/ledger`** — append-only JSONL ledger at `~/.relayburn/ledger.jsonl` (override via `RELAYBURN_HOME`). `appendTurns`, `stamp`, `query`, `queryAll`. Stamping with `{sessionId}` / `{messageId}` / `{sessionId, range}` selectors; last-write-wins per key, folded at query time. Content sidecar at `~/.relayburn/content/<sessionId>.jsonl` with `full` / `hash-only` / `off` modes (default 90-day retention). Index sidecar with `rebuildIndex()`. Per-process file lock with stale-lock recovery. Per-source cursors for incremental ingest.
  - **`@relayburn/analyze`** — pricing loader (vendored models.dev snapshot, override at `$RELAYBURN_HOME/models.dev.json`) and per-record cost derivation (`costForTurn`, `costForUsage`, `sumCosts`). Reasoning tokens billed at the output rate and reported separately. Provider-prefix fallback so `anthropic/claude-sonnet-4-6` resolves to the `claude-sonnet-4-6` rate.
  - **`@relayburn/cli`** — `burn` binary with `summary`, `by-tool`, `claude` / `codex` / `opencode` spawn-wrappers (pre-assigned session UUIDs, stamp before spawn, ingest on exit), `content prune`, `rebuild-index`. Opportunistic content-sidecar prune on every non-`content` invocation.
