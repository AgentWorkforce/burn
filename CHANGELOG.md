# Changelog

All notable changes across the relayburn monorepo, rolled up across the four published packages. The per-package CHANGELOGs at `packages/*/CHANGELOG.md` remain the authoritative source for what shipped where; this file is a unified view organized by release date.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and the workspace adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Each package versions independently — the version line under each release lists every package that bumped on that date.

## [Unreleased]

### Added

- **Hook-based Claude Code ingest.** Spawners can now install burn's ingest hooks per-invocation via Claude Code's `--settings` flag, removing any need to mutate `~/.claude/settings.json` globally. Closes [#7](https://github.com/AgentWorkforce/burn/issues/7). [cli, ledger]
  - `@relayburn/ledger` — new `buildClaudeHookSettings({ burnBin? })` returns `{ sessionId, settings }`: a fresh UUID plus a JSON string wiring every Claude Code hook event (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Notification`, `Stop`, `SubagentStop`, `SessionEnd`) to `burn ingest --runtime claude --quiet`. Sits alongside `stamp` so spawners get both primitives from one package.
  - `burn ingest --runtime claude [--quiet]` — new CLI command that reads a hook payload JSON on stdin, extracts `session_id` + `transcript_path`, and incrementally parses the transcript via the existing cursor + dedup machinery. Safe to fire on every hook event; hook failures never propagate a non-zero exit back to Claude Code.
  - Tool-call failures ride in the regular `PostToolUse` payload (surfaced as `ToolCall.isError` on the resulting `TurnRecord`); no phantom `PostToolUseFailure` event is registered.

## 2026-04-24 — Content capture everywhere + `burn rebuild --content`

**Versions:** `@relayburn/reader@0.7.0`, `@relayburn/ledger@0.7.0`, `@relayburn/analyze@0.7.0`, `@relayburn/cli@0.7.0`

### Added

- **Content capture for Codex and OpenCode parsers** (#33 follow-up). Both parsers now emit `ContentRecord` entries when `contentMode === 'full'`, matching the shape the Claude parser already produced. Covers `text` (user/assistant), `thinking` (Codex reasoning), `tool_use`, and — most importantly for `burn waste` attribution — `tool_result` keyed by the same `call_id` / `callID` the tool call carries. In Codex, content only emits for turns that commit at `task_complete`; uncommitted content is dropped and will be re-emitted once the turn commits. [reader]
- **`burn rebuild --content`** — re-parses source session files to populate missing content sidecars. Skips sessions that already have content on disk, leaves cursors and ledger rows untouched. Primary use: backfill content for historical Codex and OpenCode sessions ingested before content capture was implemented for those adapters, or to restore a sidecar that was pruned. [cli]
- **`listContentSessionIds()`** — ledger helper enumerating sessionIds with a non-empty content sidecar on disk. Drives `burn rebuild --content`'s skip path. [ledger]

## 2026-04-24 — Quality signals + waste-pattern detectors

**Versions:** `@relayburn/reader@0.6.0`, `@relayburn/ledger@0.6.0`, `@relayburn/analyze@0.6.0`, `@relayburn/cli@0.6.0`

### Added

- **Quality signals: outcome inference + one-shot rate.** Two orthogonal per-session signals for the "was this work good enough that a cheaper model could have done it" question. Closes [#6](https://github.com/AgentWorkforce/burn/issues/6). [cli, analyze]
  - `@relayburn/analyze` — new `computeQuality(turns, opts)` returning `SessionOutcome[]` (classifies sessions as `completed` / `abandoned` / `errored` / `unknown` with explicit confidence and a reason code) and `OneShotMetrics[]` (edit turns / one-shot edit turns / retry volume, excluding sidechain subagent turns). Give-up phrase detection on the last assistant text downgrades confidence when the content sidecar is available, but is never required.
  - `burn summary --quality` — appends a quality rollup (outcome counts + weighted one-shot rate) to summary output. Content sidecar reads run with a concurrency cap of 8 so large ledgers don't serialize I/O.
  - Both signals are computed lazily at query time (never persisted) so future rule changes don't require a rebuild.
  - Sources that don't record `stopReason` (e.g. Codex) are classified `completed/low` with reason `unknown-ending` rather than being swept into `abandoned`.
- **Waste-pattern detectors** — retry loops, failure runs, compaction loss, edit-revert. Closes [#11](https://github.com/AgentWorkforce/burn/issues/11). [analyze, reader]

### PRs in this release

- [#53](https://github.com/AgentWorkforce/burn/pull/53) — Add quality signals (outcome inference + one-shot rate)

## 2026-04-24 — Changelog hygiene

**Versions:** `@relayburn/reader@0.5.0`, `@relayburn/ledger@0.5.0`, `@relayburn/analyze@0.5.0`, `@relayburn/cli@0.5.0`

Housekeeping bump only — folds prior `[Unreleased]` content into the correct historical release sections. No functional changes in any package.

## 2026-04-23 — `burn waste`: per-tool-call cost attribution

**Versions:** `@relayburn/reader@0.4.0`, `@relayburn/ledger@0.4.0`, `@relayburn/analyze@0.4.0`, `@relayburn/cli@0.4.0`

> `reader` and `ledger` had no functional changes — bumped for version parity across the workspace.

### Added

- **`burn waste`** — per-tool-call and per-file cost attribution. Ranks the most expensive tool calls, files, Bash commands, and subagent calls by attributed cost: **initial** (turn after the tool call, where the result enters context as fresh `input`/`cacheCreate`) plus **persistence** (every subsequent turn it rides along in `cacheRead` until evicted). Closes [#3](https://github.com/AgentWorkforce/burn/issues/3). [cli, analyze]
  - `burn waste [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--all] [--json]` — top-N rankings by file, Bash command (collapsed by `argsHash`), and subagent (`subagent_type`). `--all` shows full lists; `--json` emits raw aggregations.
  - Sized attribution from the content sidecar when available (text length / 4 as a token estimate); even-split fallback (initial only) when it isn't, with a printed note.
  - Per-paying-turn model rates so cross-model sessions are priced correctly. Sibling normalization across simultaneously-entering tool_results so attributed cost never exceeds what was actually paid.

### PRs in this release

- [#50](https://github.com/AgentWorkforce/burn/pull/50) — Add `burn waste` plus the per-paying-turn pricing / sibling-normalization refactor

## 2026-04-23 — `burn context`: agent context-file cost attribution

**Versions:** `@relayburn/reader@0.3.0`, `@relayburn/ledger@0.3.0`, `@relayburn/analyze@0.3.0`, `@relayburn/cli@0.3.0`

> `reader` and `ledger` had no functional changes — bumped for version parity across the workspace.

### Added

- **`burn context`** — cost attribution for agent context files across every agent `burn` ingests: `CLAUDE.md`, `.claude/CLAUDE.md`, and `AGENTS.md`. Answers "how much are my rules files costing me per session, and which sections are the most expensive?" Closes [#10](https://github.com/AgentWorkforce/burn/issues/10). [cli, analyze]
  - `burn context [--project <path>] [--since 7d] [--kind <k>] [--json]` — reports file size, per-session avg / p95 cost, window total across N sessions, and sections ranked by cost, per file. `--kind claude-md` / `--kind agents-md` narrows to one file kind.
  - `burn context advise [--top <n>]` — emits read-only unified-diff TRIM hunks for the most expensive sections across every discovered file. POSIX-relative paths so hunks apply with `git apply` / `patch`. No `--apply` flag: burn never mutates your context files.
  - Each file is attributed against only the turns whose harness actually reads it (Claude Code for CLAUDE.md; Codex and OpenCode for AGENTS.md) — a Codex session never pays for CLAUDE.md, a Claude Code session never pays for AGENTS.md.
  - Attribution math is direct: `file_tokens × cacheReadPrice` per turn whose `cacheRead` is large enough to hold the file (conservative eviction signal that skips the first turn, where the file lives in `cacheCreate`, and any turn where it's been compacted away).
  - Uses the git-canonical `projectKey` for ledger queries when available, so multiple worktrees of the same repo roll up together.
  - Section parser groups at H2 (with H1 fallback), treats top-level content as preamble, and skips headings inside fenced code blocks with strict CommonMark close matching. CRLF → LF normalization and trailing-newline handling so line numbers match what an editor shows.

### Fixed

- `attributeContext` deduplicates per-session `totalRidingTurns` using max-per-session rather than summing across files, so a session that reads multiple context files isn't double-counted.

## 2026-04-23 — `burn compare` and cross-harness classifier

**Versions:** `@relayburn/reader@0.2.0`, `@relayburn/ledger@0.2.0`, `@relayburn/analyze@0.2.0`, `@relayburn/cli@0.2.0`

### Added

- **`burn compare`** — observed-data comparison of cost-per-turn, one-shot rate, and turn count by `(model, activity)`. The headline query for the meta-goal: "looking at the work I actually did, which model handled each activity category best?" Closes [#38](https://github.com/AgentWorkforce/burn/issues/38). [cli, analyze]
  - Flags: `--models a,b`, `--since 7d`, `--project <path>`, `--session <id>`, `--workflow <id>`, `--agent <id>`, `--min-sample <n>`, `--json`, `--csv`, `--help`.
  - Grouped TTY table with coverage notes; missing-data cells render `—` (never `$0.00` or `0%`).
  - JSON and CSV output expose `noData` / `insufficientSample` / `pricedTurns` so consumers can distinguish "no data" from "low sample" and "free model" from "unknown pricing."
- **`burn rebuild --reclassify [--force]`** — backfills activity labels on existing ledger turns by re-running `classifyActivity`. Default skips already-classified turns; `--force` reclassifies everything. Reports per-category change counts. `burn rebuild --index` rebuilds the sidecar; both can run together. `burn rebuild-index` retained as alias. [cli, ledger]
- **Activity classifier now runs for Codex and OpenCode turns** — previously only Claude Code turns received an `activity` label, which left every Codex/OpenCode turn in `unclassified` and made cross-harness comparison impossible. [reader]
- **`TOOL_ALIASES` map + `normalizeToolName(name)`** in the classifier normalizes harness-specific tool names (`apply_patch`, `exec_command`, `shell`, lowercase OpenCode names, codex agent tools) onto canonical Claude names so the rule tables stay single-source. [reader]
- **Six new activity categories** (taxonomy expanded from 12 → 18): `reasoning`, `docs`, `deps`, `format`, `review`, `verification`. [reader]
- **`buildCompareTable(turns, opts)`** — pure aggregator in `@relayburn/analyze` so scripts can consume the same shape the CLI renders. [analyze]

### Fixed

- **Race between `appendTurns` and `reclassifyLedger`.** `appendLines` in the writer now acquires the same `withLock('ledger', …)` that `reclassifyLedger` holds for its read-modify-write pass. Previously an `appendFile` landing between reclassify's `readFile` and `rename` would write to the soon-orphaned old inode and silently disappear when rename swapped the new file in. Reported by Devin's review on [#48](https://github.com/AgentWorkforce/burn/pull/48). [ledger]

### Changed

- `BUILD_DEPLOY_PATTERNS` tightened — the catch-all `/\bdeploy\b/` is replaced with explicit verbs per-tool (`vercel/netlify/flyctl/railway/sst deploy|up`, `kubectl apply/rollout/set`, `helm install/upgrade`, `terraform apply/plan/destroy`, `make build|release|dist|package|deploy`). [reader]
- Top-level `burn` help and the README list every `burn compare` filter (`--session`, `--agent`) instead of just the most common ones. [cli]
- `burn compare` now rejects `--json` + `--csv` together with exit 2 instead of silently picking JSON. [cli]
- `--models` filter pre-seeds requested models so a model the user explicitly asked about stays visible (as an all-empty column with a coverage note) even when zero turns matched. [analyze]

### PRs in this release

- [#48](https://github.com/AgentWorkforce/burn/pull/48) — Add burn compare, classifier wiring, reclassify, race fix, taxonomy expansion

## 2026-04-22 — Initial release

**Versions:** `@relayburn/reader@0.1.0`, `@relayburn/ledger@0.1.0`, `@relayburn/analyze@0.1.0`, `@relayburn/cli@0.1.0`

### Added

- **`@relayburn/reader`** — pure parsers turning agent session logs into `TurnRecord[]` and `ContentRecord[]`. Claude Code (`parseClaudeSession`), Codex (`parseCodexSession` with cumulative-token-delta accounting), OpenCode (`parseOpencodeSession` reading the per-session/message/part storage layout). All three have `*SessionIncremental` variants for cursor-based ingest. Deterministic activity classifier with 12 categories (Claude Code only at this release). Git-canonical project resolution (`resolveProject`) so `projectKey` survives across worktrees.
- **`@relayburn/ledger`** — append-only JSONL ledger at `~/.relayburn/ledger.jsonl` (override via `RELAYBURN_HOME`). `appendTurns`, `stamp`, `query`, `queryAll`. Stamping for spawn-time enrichment with `{sessionId}` / `{messageId}` / `{sessionId, range}` selectors; last-write-wins per key, folded at query time. **Content sidecar** at `~/.relayburn/content/<sessionId>.jsonl` with `full` / `hash-only` / `off` modes via `RELAYBURN_CONTENT_STORE`; default 90-day retention. **Index sidecar** for fast dedup with `rebuildIndex()`. Per-process file lock (`withLock`) with stale-lock recovery. Per-source cursors for incremental ingest.
- **`@relayburn/analyze`** — pricing loader (vendored models.dev snapshot with optional override at `$RELAYBURN_HOME/models.dev.json`) and per-record cost derivation (`costForTurn`, `costForUsage`, `sumCosts`). Reasoning tokens billed at the output rate and reported separately. Provider-prefix fallback in lookup so `anthropic/claude-sonnet-4-6` resolves to the `claude-sonnet-4-6` rate.
- **`@relayburn/cli`** — `burn` binary with `summary`, `by-tool`, `claude` / `codex` / `opencode` spawn-wrappers (pre-assigned session UUIDs, stamps before spawn, ingest on exit), `content prune`, `rebuild-index`. Opportunistic content-sidecar prune on every non-`content` invocation.
