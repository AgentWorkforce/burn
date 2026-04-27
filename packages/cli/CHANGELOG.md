# Changelog

All notable changes to `@relayburn/cli` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **`burn mcp-server` runs an incremental `buildArchive()` at startup** ([#97](https://github.com/AgentWorkforce/burn/issues/97)) so the first `burn__sessionCost` / `burn__currentBlock` tool call hits the SQL archive instead of re-walking the JSONL ledger. The build is idempotent — a no-op when nothing has changed since the last build — and a build failure logs to stderr but never refuses to serve. Tool fallbacks to `queryAll` are wired through the new `onLog` hook so any persistent archive breakage is visible in the MCP host's stderr stream.

## [0.29.0] - 2026-04-26

### Added

- **Complete Codex/OpenCode parity for spawn attribution and live ingest** (#63). `burn codex` and `burn opencode` now write v1 pending-stamp manifests under `$RELAYBURN_HOME/pending-stamps/` before spawning, resolve them before the first matching turn is appended, and run a foreground watch loop for the child lifetime so sessions ingest incrementally while live. Adds `burn watch [--interval <ms>] [--once]` for passive foreground ingest of Claude, Codex, and OpenCode stores; `--daemon` is explicitly unsupported for now.

### Changed

- Codex session-id discovery falls back to the first JSONL `session_meta.payload.id` when the rollout filename does not end in a UUID.

## [0.28.0] - 2026-04-26

### Added

- **Synthetic provider filters and grouping** (#31). `burn summary`, `burn by-tool`, and `burn waste` accept `--provider <name>`; `burn summary --by-provider` groups query-time reattributed turns under provider labels such as `synthetic` without rewriting raw ledger model strings. Synthetic routing recognizes `hf:*`, `accounts/fireworks/models/*`, and `synthetic/*`.

## [0.27.0] - 2026-04-26

### Changed

- **Persist user-turn block-size records during ingest** (#2). `burn ingest`, passive ingest, and the Claude/Codex/OpenCode wrappers now append parser-emitted `UserTurnRecord`s for all three harnesses. Codex passive cursors also carry the in-flight user-turn slot so resumed ingest can complete a bridge record across file-growth boundaries. `burn waste` and `burn diagnose` load these records and use them as the sized fallback when content sidecars are missing.

## [0.26.0] - 2026-04-26

### Added

- **Execution-graph passthrough for Codex ingest** ([#87](https://github.com/AgentWorkforce/burn/issues/87)). `burn ingest` (and `burn codex`) now persist Codex `SessionRelationshipRecord`s and `ToolResultEventRecord`s the reader emits, alongside the existing turns / content lines, mirroring the Claude path landed in the previous release. The Codex cursor (`~/.relayburn/cursors.json`) gains `rootSessionEmitted`, `nextEventIndex`, and `toolResultCounters` so dedup of execution-graph rows survives across `burn` invocations even when the writer-side index isn't warm.

## [0.25.0] - 2026-04-26

### Added

- `burn archive status` (text + `--json`) now reports `tool_result_events` row counts alongside sessions / turns / tool_calls / compactions, and `burn archive build` / `rebuild` summary lines include the count of tool-result events materialized this run (#101).

## [0.24.0] - 2026-04-26

### Added

- **`burn archive status --json` surfaces a fidelity histogram on `turns`** (#110). The JSON payload gains a `fidelityHistogram` field — counts per `attribution_fidelity` value across all materialized turns (`full`, `usage-only`, `partial`, `aggregate-only`, `cost-only`, plus `unknown` for rows with no fidelity metadata yet). Lets ops scripts spot upstream parser gaps without opening the SQLite directly. Text-mode output is unchanged for now.

## [0.23.0] - 2026-04-26

### Added

- **Execution-graph passthrough for OpenCode ingest** ([#93](https://github.com/AgentWorkforce/burn/issues/93)). `burn ingest` (OpenCode path) now persists the `SessionRelationshipRecord`s and `ToolResultEventRecord`s the OpenCode reader produces — root / subagent edges from `session.parentID`, plus terminal-status tool-result events with `contentLength` / `contentHash` — via the existing `appendRelationships` / `appendToolResultEvents` writers. No new flags or output; closes the OpenCode arm of the gap that #77 closed for Claude.

## [0.22.0] - 2026-04-26

### Changed

- **`burn` ingest runs cross-file Claude relationship reconciliation at end of pass** ([#112](https://github.com/AgentWorkforce/burn/issues/112)). After parsing every Claude session file in an ingest pass, the CLI now feeds the per-file evidence through `reconcileClaudeSessionRelationships` and appends any resulting `fork` / `continuation` rows. Idempotent — the writer's `relationshipIdHash` dedup folds repeats on subsequent runs.

## [0.21.0] - 2026-04-26

### Added

- **User-turn passthrough in ingest** (#94). `burn ingest`, `burn ingest --runtime claude` (hook path), and `burn claude` (subcommand wrapper) now persist `UserTurnRecord`s the Claude / Codex / OpenCode parsers produce, alongside the existing turns / content / compaction / relationship / tool-result-event lines. No new flags or output yet — this lays the substrate so per-tool-call cost attribution (#2) can read user-turn block sizes back out of the ledger instead of re-parsing source session files at query time.

## [0.20.0] - 2026-04-26

### Added

- **Add derived analytics archive foundation: archive.sqlite + `burn archive`** (#40)

## [0.19.0] - 2026-04-26

### Added

- **Execution-graph passthrough in ingest** (#42, first PR). `burn ingest` and `burn claude` (subcommand wrapper) now persist `SessionRelationshipRecord`s and `ToolResultEventRecord`s the Claude reader produces, alongside the existing turns / content / compaction lines. No new flags or output yet — this PR just lays the substrate so #8 (subagent tree), #11 (waste patterns), and future archive work can consume the graph instead of reconstructing it from `isSidechain` / `parentUuid`.

## [0.17.0] - 2026-04-25

### Changed

- Surface silent parser content-capture gaps at ingest time (#59)

## [0.15.1] - 2026-04-25

### Added

- **Wire spawner-owned RELAYBURN_* env-var contract into burn claude/codex/opencode** (#63)

## [0.15.0] - 2026-04-25

### Changed

- Protect recoverable sidecars from retention prune (#61)

## [0.14.2] - 2026-04-25

### Changed

- Promote even-split note to a banner when it dominates (#60)

## [0.14.0] - 2026-04-25

### Added

- **Add coverage and fidelity metadata to TurnRecord** (#41)

## [0.13.1] - 2026-04-25

### Added

- **`burn archive build | rebuild | status`** — manage the new derived analytics archive (`~/.relayburn/archive.sqlite`). `build` applies any ledger tail not yet materialized, `rebuild` recreates from scratch, `status` reports schema version, row counts, and sync state. Both `build` and `status` accept `--json`. The archive is rebuildable from the canonical `ledger.jsonl` at any time, so `rm ~/.relayburn/archive.sqlite && burn archive rebuild` always reproduces the same state. Foundation for #40; rewiring `burn summary` / `compare` / `plans` to read from the archive lands in follow-up PRs.
- **`burn mcp-server`** — stdio MCP (Model Context Protocol) server that lets a running agent self-query its own cost and quota state mid-session. Registers `burn__sessionCost` and `burn__currentBlock`. Read-only. Pair with `buildMcpConfig({sessionId})` from `@relayburn/mcp` to inject the server into a spawned `claude --mcp-config <…>` session. (#26)

## [0.11.0] - 2026-04-25

### Added

- **Add plan-based monthly quota tracking** (#39)

### Changed

- Wrap loadPlanStatuses in try/catch (Devin review on #66) (#66)
- Reference #22 (cursor wontfix) on the cursor plan branch (#22)

## [0.10.0] - 2026-04-24

### Added

- **`burn limits`** — Claude quota-window tracker. Pairs the OAuth `usage` endpoint snapshot (`five_hour`, `seven_day`, `seven_day_opus`, `extra_usage`) with a local-ledger forecast (burn rate + linearly-extrapolated projected % at reset). Supports `--watch [5s]`, `--json`, `--no-api` (offline), `--no-forecast`. Reads the OAuth token from `CLAUDE_CODE_OAUTH_TOKEN`, `~/.claude/.credentials.json`, or macOS Keychain without persisting it; responses cached ≤30s in-process. Token-missing exits 2 with a one-line message. (#5)
- **`burn plans`** — monthly plan budget tracking. Subcommands: `add --provider <p> --preset <name>` (built-in presets: claude/pro $20, claude/max $200, cursor/pro $20), `add --provider custom --id <id> --name <"…"> --budget <usd> [--reset-day <1-31>]`, `remove <id>`, `set-reset-day <id> <day>`. Bare `burn plans` lists configured plans with current cycle spend, projected end-of-cycle, and elapsed days; `--json` emits the raw `PlanUsage` shape. Plans persist to `~/.relayburn/plans.json`. (#39)
- **`burn limits` integrates plan status** when plans are configured: a `Monthly plan (<name>):` block per plan reports cycle spend / budget / elapsed / projected end-of-cycle (`$X over` or `Y% under`) plus `Runway: N more days at current rate` when over-pace. Projections with fewer than 7 days of cycle data render with a `(limited data)` marker so users don't anchor on noise from the first week. The `--json` payload gains a `plans[]` array with the full numeric breakdown. (#39)

## [0.9.0] - 2026-04-24

### Added

- **Add [Unreleased] changelog entries for subagent tree** (#8)
- **Add subagent tree primitive + summary queries** (#8)

## [0.8.0] - 2026-04-24

### Added

- **`burn ingest --runtime claude [--quiet]`** — hook-driven ingest. Reads a Claude Code hook payload JSON on stdin, extracts `session_id` + `transcript_path`, and incrementally parses the transcript via the existing cursor + dedup machinery. Safe to fire on every hook event (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Notification`, `Stop`, `SubagentStop`, `SessionEnd`); hook failures never propagate a non-zero exit back to Claude Code. Paired with `buildClaudeHookSettings` in `@relayburn/ledger` for spawner-integrated hook installation. Tool-call failures ride in the normal `PostToolUse` payload (surfaced as `ToolCall.isError` on the `TurnRecord`); no phantom `PostToolUseFailure` event is registered. Closes [#7](https://github.com/AgentWorkforce/burn/issues/7).
- **`burn summary --subagent-tree <session-id>`** — renders the session's subagent tree with cumulative cost and turn counts rolled up from leaves. Main thread at the root, first-level subagents beneath it (labelled by `subagent_type`), nested subagents under their spawners. `--json` emits the raw `SubagentTreeNode` structure. Answers "which subagent invocation cost the most in this workflow?" from historical Claude JSONL alone. Closes [#8](https://github.com/AgentWorkforce/burn/issues/8).
- **`burn summary --by-subagent-type`** — aggregates subagent invocations across sessions (respecting `--since` / `--project` / `--workflow` / `--agent`) and reports per-`subagent_type` invocation count, turn count, total / median / p95 / mean cost. Answers "what did the Explore subagent cost us cumulatively across the week?" without needing hook-based enrichment. Closes [#8](https://github.com/AgentWorkforce/burn/issues/8).

## [0.7.0] - 2026-04-24

### Added

- **Content capture for Codex and OpenCode sessions** (#33 follow-up). Codex and OpenCode sessions now write `ContentRecord`s to the sidecar when `content.store = full`, matching the Claude Code surface. This unlocks sized per-tool-call attribution (`burn waste`) and outcome-signal analysis on non-Claude harnesses.
- **`burn rebuild --content`** — re-parses source session files to populate missing content sidecars. Skips sessions that already have content on disk, leaves cursors and ledger rows untouched. Primary use: backfill content for historical Codex and OpenCode sessions ingested before content capture landed for those adapters, or to restore a sidecar that was pruned.

## [0.6.0] - 2026-04-24

### Added

- **`burn summary --quality`** — appends a quality rollup to the summary output: outcome counts (completed / abandoned / errored / unknown) plus the weighted one-shot edit rate across the matched sessions. Closes [#6](https://github.com/AgentWorkforce/burn/issues/6).
  - Opportunistically loads per-session content sidecars (when available) so give-up phrase detection can downgrade assistant-ended confidence. Sidecar reads run with a concurrency cap of 8 so large ledgers don't serialize I/O.
- **Waste-pattern detectors** surfaced via the analyze module (retry loops, failure runs, compaction loss, edit-revert). Closes [#11](https://github.com/AgentWorkforce/burn/issues/11).

## [0.5.0] - 2026-04-24

### Changed

- Clean up changelogs: move [Unreleased] content into 0.3.0/0.4.0 sections

## [0.4.0] - 2026-04-23

### Added

- **`burn waste`** — ranks tool calls, files, Bash commands, and subagent calls by their attributed cost. Splits each `tool_use`'s cost into **initial** (the turn after the tool call, where the result enters context) and **persistence** (every subsequent turn where it rides along in `cacheRead` until evicted). Sized attribution when the content sidecar is enabled; even-split fallback (initial only) with a printed note when it isn't. Closes [#3](https://github.com/AgentWorkforce/burn/issues/3).
  - Flags: `--since 7d`, `--project <path>`, `--session <id>`, `--workflow <id>`, `--all` (full lists, not just top-N), `--json` (raw aggregations for downstream tooling).
  - Per-paying-turn model pricing: cross-model sessions (e.g. Sonnet → Haiku) attribute each turn's costs at that turn's rate.
  - Sibling normalization: multiple tool_results entering on the same turn share the turn's `newContent` proportionally; cached tool_results share each turn's `cacheRead` proportionally — so attributed cost never exceeds what was actually paid.

## [0.3.0] - 2026-04-23

### Added

- **`burn context`** — cost attribution for agent context files across every agent `burn` ingests. Discovers `CLAUDE.md`, `.claude/CLAUDE.md`, and `AGENTS.md`; attributes each against only the turns whose harness actually reads it (Claude Code for CLAUDE.md; Codex and OpenCode for AGENTS.md). Per-file ranked section tables plus a grand total across all context files. Closes [#10](https://github.com/AgentWorkforce/burn/issues/10).
  - Flags: `--project <path>`, `--since 7d`, `--kind <claude-md|agents-md>`, `--json`.
  - Uses the git-canonical `projectKey` (via `resolveProject`) for the ledger query when available, so multiple worktrees of the same repo roll up together; falls back to the filesystem path when no git remote is set.
- **`burn context advise`** — emits unified-diff TRIM hunks for the most expensive sections across all discovered context files. Paths render POSIX-relative to the project root so they apply with `git apply` / `patch`. No `--apply` flag: burn never mutates your context files.
  - Flags: `--top <n>` (default 3, per file), `--kind <k>`, `--project <path>`, `--since 7d`.

## [0.2.0] - 2026-04-23

### Added

- **`burn compare`** — observed-data comparison of cost-per-turn, one-shot rate, and turn count by `(model, activity)`. The headline query for the meta-goal: "looking at the work I actually did, which model handled each activity category best?" Closes #38.
  - Flags: `--models a,b`, `--since 7d`, `--project <path>`, `--session <id>`, `--workflow <id>`, `--agent <id>`, `--min-sample <n>`, `--json`, `--csv`, `--help` / `-h` / `help`.
  - TTY output renders a grouped table (model name spans three sub-columns) with coverage notes for missing or low-sample cells. Notes capped at 8 with `… and N more` overflow.
  - `--json` and `--csv` are mutually exclusive — passing both exits 2 instead of silently picking JSON.
  - Missing-data cells render `—`, never `$0.00` or `0%`. JSON/CSV expose the underlying `noData` and `insufficientSample` flags so script consumers can distinguish them.
- **`burn rebuild`** — backfill derived ledger artifacts.
  - `--reclassify [--force]` re-runs `classifyActivity` across the whole ledger so old turns benefit from new classifier rules. Default mode skips turns that already have an `activity` (safe to re-run); `--force` reclassifies every turn.
  - `--index` rebuilds the sidecar index (equivalent to `burn rebuild-index`). Both flags can run together.
  - Reports `processed` / `changed` / `unchanged` / `skipped` counts plus per-category change breakdown.
- **`burn rebuild-index`** retained as a backward-compat alias for `burn rebuild --index`.

### Changed

- Top-level `burn` help and the README now list every `burn compare` filter (`--session`, `--agent`) instead of just the most common ones.

## [0.1.0] - 2026-04-22

### Added

- **Initial release.** `burn` CLI binary.
- `burn summary [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>]` — per-model token + cost breakdown. Triggers ingest from Claude Code, Codex, and OpenCode session logs before reporting.
- `burn by-tool [--since 7d] [--project <path>] [--session <id>]` — per-tool attribution. Splits each turn's input cost across the prior turn's tool calls.
- `burn claude [--tag k=v ...] [-- <claude args>]` — spawn-wrapper that pre-assigns a session UUID, applies stamps before spawn, and ingests the session on exit.
- `burn codex [--tag k=v ...] [-- <codex args>]` — same wrapper pattern for Codex.
- `burn opencode [--tag k=v ...] [-- <opencode args>]` — same wrapper pattern for OpenCode.
- `burn content prune [--days <n>]` — apply retention to the content sidecar.
- `burn rebuild-index` — rebuild the sidecar hash index from the ledger.
- Opportunistic content-sidecar prune on every non-`content` invocation (best-effort, never fails the CLI).
