# Changelog

All notable changes to `@relayburn/cli` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`burn waste --patterns ghost-surface`** ([#166](https://github.com/AgentWorkforce/burn/issues/166)). New detector kind that flags user-installed surface files (Claude `~/.claude/{agents,skills,commands}/`, Codex `~/.codex/{prompts,skills,rules,memories}/`, OpenCode `opencode.json` + project skills folder) whose basenames never appear in the observed-names set mined from the turn stream. Output is a labeled `Ghost user-installed surface` table with columns `source | kind | path | tokens | sessions | cost | note`. JSON output (`--patterns --json`) gains a `ghostSurface` array; the unified `findings` payload (`--patterns --findings`) folds ghost findings in alongside retry-loop / failure-run / etc. and severity-ranks them by `usdPerSession`. Each finding's suggested fix is a `command`-style `WasteAction`: `mkdir -p <archive-dir> && mv <path> <archive-dir>/`. OpenCode declared-catalog skills are emitted with `cost: $0` and a `catalog (#54)` note in the table to avoid double-counting against the `opencode-system-prompt` catalog-bloat detector. Slash-command-style invocations are not yet detected from tool calls — issue [#172](https://github.com/AgentWorkforce/burn/issues/172) tracks the follow-up, and the adapter inputs reserve a `userTurnTextBySession` field so it can land without a breaking change. Bare `--patterns` (no value) now selects all 9 detectors; the previous count was 8.

## [0.41.0] - 2026-04-28

### Fixed

- **Pending-stamp resolution no longer cross-contaminates concurrent same-cwd same-harness runs** ([#162](https://github.com/AgentWorkforce/burn/issues/162)). When two `burn run codex` (or two `burn run opencode`) processes are running in the same directory, the `{harness, cwd, mtime ≥ spawnStart, sessionDirHint}` filter cannot tell their stamps apart, so the resolver was applying every matching stamp's enrichment to whichever session ingested first — leaving the other session unstamped. Each session now claims at most one stamp (FIFO by `spawnStartTs`), so the older stamp goes to the first session that ingests and the newer stamp stays pending until its own session shows up. Different cwds, different harnesses, and Claude (which uses pre-allocated session IDs and never writes pending stamps) were unaffected and remain unchanged.

### Added

- **Content-sidecar enrichments surface in `burn diagnose` and `burn waste --patterns`** ([#57](https://github.com/AgentWorkforce/burn/issues/57)). When a session was captured with `content.store=full`, the four waste-pattern detectors now emit additional fields that `burn diagnose <session>` and `burn waste --patterns` render through the existing tables and `WasteFinding` text: retry-loop titles include the shared error-line signature (e.g. `"Bash failed 4× in a row: 'npm ERR! code ENOENT'"`), failure-run details list per-tool first-line errors, compaction-loss details summarize the work in the compacted window (`N edit(s), M bash, K read(s) on src/foo.ts, src/bar.ts`), and edit-revert details show truncated `old_string`/`new_string` previews for both anchor edits. `--json` payloads carry the new fields verbatim (`errorSignature`, `errorSignatures`, `lostWork`, `samplePreview`). Sessions in `content.store=hash-only` or with pruned content render exactly as before. `burn waste` lazily loads content sidecars only for the four enrichable detectors (`retries`/`failures`/`compaction`/`reverts`) and only when at least one is selected, so unrelated runs pay no I/O cost.
- **Aggregate parser content-capture report in `burn diagnose`** ([#79](https://github.com/AgentWorkforce/burn/issues/79)). `burn diagnose` (no positional argument) now walks the ledger and emits a per-adapter content-capture gap table — total sessions, sessions with ≥1 tool call, gapped sessions (≥1 tool call but zero `tool_result` ContentRecords), orphan tool-call count, and `degradedPct`. Honors `--json` (`{ adapters: [{ adapter, sessions, sessionsWithToolCalls, gappedSessions, orphanToolCalls, degradedPct }, ...], contentMode }`). The existing per-session `burn diagnose <session-id>` behavior is unchanged. Permanent, queryable surface for the gap that the per-invocation ingest warning ([#75](https://github.com/AgentWorkforce/burn/issues/75)) only flags once per `burn` run; rows omit the gap signal with an explanatory note when `RELAYBURN_CONTENT_STORE` is `hash-only` or `off`. Adapters with no sessions in the ledger are omitted entirely.
- **`burn waste --patterns --findings`** ([#56](https://github.com/AgentWorkforce/burn/issues/56)). Renders every detector's output through one severity-ranked `WasteFinding` table — retry loops / failure runs / compaction losses / edit reverts / edit-heavy / OpenCode skill-* / system-prompt-tax sorted together by severity (high → warn → info) then `usdPerSession`. The existing per-detector tables remain the default render path; `--findings` is opt-in. JSON output (`--patterns --json`) gains a `findings` array alongside the existing per-detector arrays for downstream consumers; the JSON refusal payload also carries `findings: []` for schema parity. `burn waste --findings` (without `--patterns`) implies `--patterns` so the flag is never silently ignored.

## [0.40.0] - 2026-04-28

### Added

- **`burn archive vacuum`** ([#104](https://github.com/AgentWorkforce/burn/issues/104)). New subcommand that runs SQLite `VACUUM` against `archive.sqlite` to reclaim free pages from `INSERT OR REPLACE` churn (stamp re-folds rewrite turn rows; rebuild drops + recreates rows). Acquires the same `'archive'` lock used by `build` / `rebuild`, so a vacuum and a build can be issued concurrently and will serialize without corruption. Text output is a one-liner — `archive: vacuumed 12.3 MB -> 4.1 MB (reclaimed 8.2 MB)` — and `--json` returns `{ archivePath, existed, beforeBytes, afterBytes, reclaimedBytes }`. No-op with a hint if the archive doesn't exist; vacuum never creates an archive as a side effect.

## [0.39.0] - 2026-04-28

### Changed

- **`burn compare` takes models as a required positional, not a flag** ([#159](https://github.com/AgentWorkforce/burn/issues/159)). The verb "compare" implies selection — without an explicit list the old `burn compare` produced a wide N×M survey of every model in the ledger, which is what `burn summary` (with `--by-provider` / `--by-tool`) already covers. The new shape is `burn compare <model_a,model_b[,...]> [flags]` with a minimum of 2 models. Trim/dedupe rules match the old `--models` flag. The `--models` flag is removed; passing it now exits 2 with a pointer to the positional form. Missing or single-model positional likewise exits 2 with `burn compare: needs at least 2 models. Run \`burn summary --by-provider\` (or \`burn summary --by-tool\`) to see which models have data.` Help block, top-level help, and every example in this repo flip to the positional form. No behavioral change to filters, the cell schema, the JSON contract, or aggregation logic.

## [0.38.0] - 2026-04-28

### Changed

- **`burn by-tool` folded into `burn summary --by-tool`** ([#156](https://github.com/AgentWorkforce/burn/issues/156)). The standalone `burn by-tool` verb has been removed in favor of a `summary` mode flag that sits next to `--by-provider`, `--by-subagent-type`, and `--subagent-tree`. Output columns (`tool | calls | attributedCost`) and the attribution-method footer are unchanged. Folding into `summary` closes the previous filter-parity gap — `--by-tool` now inherits `--workflow`/`--agent` from `summary`'s filter list. JSON shape is `{ ingest, turns, byTool: [{ tool, calls, attributedCost }], unattributed, fidelity }`. Mode flags are mutually exclusive: combining `--by-tool` with `--by-provider`/`--by-subagent-type`/`--subagent-tree` exits non-zero with a clear error. No deprecation alias — callers must migrate to `burn summary --by-tool`.

## [0.37.0] - 2026-04-28

### Added

- **`burn waste --patterns edit-heavy`** ([#167](https://github.com/AgentWorkforce/burn/issues/167)). New cross-harness detector flagging sessions whose edit-tool count exceeds 4× read-tool count (≥ 5 edits). Reaches parity across Claude / Codex / OpenCode through the existing `normalizeToolName` table — Claude `Read`/`Edit`/`Write`/..., OpenCode `read`/`edit`/`write`, Codex `read_file`/`apply_patch` all flow through one detector. Renders a table with source, reads, edits, ratio, intra-turn retry count, and cost; appears in `--json` output as `editHeavySessions`. Coverage prereq is `hasToolCalls` only — tool_result is not consulted, so the detector runs on Codex/OpenCode slices that aggregate-only ledgers can't drive the retry / failure detectors against. `burn diagnose` also renders the new section per-session.

## [0.36.0] - 2026-04-28

### Added

- **`burn compare --provider <name>`** ([#138](https://github.com/AgentWorkforce/burn/issues/138)). Filters the per-(model, activity) comparison table to turns whose effective provider matches, using the same classifier `summary` / `by-tool` / `waste` already expose. Single value or comma-separated (e.g. `--provider synthetic`, `--provider anthropic,synthetic`); resolution goes through the pricing-layer rules in `resolveProvider`, so `hf:deepseek-ai/...` aggregates under `synthetic`. When the filter is set, `compare` always uses the in-memory path (the SQLite archive's grouped SQL doesn't see the per-turn classifier).

## [0.35.0] - 2026-04-28

### Added

- **`burn summary` surfaces per-cell fidelity** ([#136](https://github.com/AgentWorkforce/burn/issues/136)). The (model | provider) table now distinguishes a literal `0` from "no source data" inside individual cells: a token-field cell whose every contributing turn omitted the field renders as `—`, and a cell whose contributing turns are mixed (some reported, some omitted) renders the value with a trailing `*` plus a single footer note (`* partial coverage: N of M turns omitted per-turn token data`). Full-fidelity slices print no marker and no footer, so the common all-Claude case looks identical to before. Records emitted before `TurnRecord.fidelity` existed (pre-#41 ledgers) are treated as best-effort full and never trigger the marker. `--json` output replaces the bare `fidelity: FidelitySummary` block with `fidelity: { summary, perCell }`, where `perCell.cells[]` carries per-(model|provider) per-field `{ known, missing }` counters and a `partial` flag — the same shape pattern the sibling fidelity PRs (#135 / #133 / #134 / #132) already emit.
- **Codex ingest persists compaction events.** The Codex passive ingest path now appends parser-emitted compactions through the existing ledger compaction writer, so `burn waste --kind compaction` can see Codex context compactions with the same event shape Claude uses.
- **`burn waste --patterns opencode-skill-recall` and `opencode-skill-pruning`** ([#54](https://github.com/AgentWorkforce/burn/issues/54)). Two new pattern kinds gated to OpenCode sessions: `opencode-skill-recall` detects repeated skill invocations with the same name (content is not deduplicated), and `opencode-skill-pruning` tracks skill results that ride in the cache indefinitely (prune-protected by OpenCode's compaction). Both render as tables in text mode and appear in `--json` output. `burn diagnose` also renders the new pattern types for per-session diagnosis.
- **`burn waste --patterns opencode-system-prompt`** ([#54](https://github.com/AgentWorkforce/burn/issues/54)). Estimates the fixed prefix tax (system prompt + skill catalog) on the first turn of an OpenCode session by subtracting the first user message size from `cacheCreate`. Renders a table showing prefix tokens, user message tokens, estimated system prompt tokens, riding turns, and total cost.

### Changed

- **`burn run <harness>` consolidates the spawn-wrappers** ([#154](https://github.com/AgentWorkforce/burn/issues/154)). The three top-level verbs `burn claude`, `burn codex`, and `burn opencode` are folded into a single `burn run <claude|codex|opencode>` subcommand, dispatched through a `HarnessAdapter` registry at `packages/cli/src/harnesses/`. Adding a new harness is a one-file addition + one-line registration — no driver changes, no help-block edits. The unified driver also emits a uniform `[burn] <name> ingest: N sessions (+M turns)` report line across all three harnesses (previously claude printed a per-file `[burn] ingested ... turns from <file>` line).

### Removed

- **Legacy `burn claude` / `burn codex` / `burn opencode` verbs** ([#154](https://github.com/AgentWorkforce/burn/issues/154)). The standalone harness verbs are removed in favor of `burn run <name>`. Callers must migrate to the new dispatcher.
- **`burn rebuild-index`** ([#151](https://github.com/AgentWorkforce/burn/issues/151)). The standalone subcommand has been dropped — it was a thin alias for `burn rebuild --index` with identical behavior. Run `burn rebuild --index` instead.

## [0.34.0] - 2026-04-27

### Added

- **`burn limits` honors fidelity on its 5-hour forecast** ([#105](https://github.com/AgentWorkforce/burn/issues/105)). The forecast still consumes every windowed turn — partial / aggregate-only / cost-only data still contributes to the running token total — but `burn limits` now classifies the contributing slice via `summarizeFidelity` and surfaces a binary `high` / `low` confidence flag. Text mode appends a `forecast: low-confidence (N of M contributing turns lack per-turn token data)` notice when at least one contributing turn is missing per-turn token coverage; full-fidelity windows print no notice. `--json` output gains a `forecast.fidelity` block carrying the `confidence` flag and the underlying `FidelitySummary`. `--watch` re-evaluates confidence on each tick so the flag flips as fresher full-fidelity turns land.

### Changed

- **`burn compare` honors fidelity** ([#95](https://github.com/AgentWorkforce/burn/issues/95)). The aggregate now defaults to the `usage-only` floor: turns whose fidelity is `aggregate-only`, `cost-only`, or `partial` are excluded so a session with mixed fidelity can't silently bias the cost/turn or one-shot rate of full-fidelity peers from the same model. Records emitted before `TurnRecord.fidelity` existed (pre-#41 ledgers) still pass for backward compatibility. New flags: `--fidelity <class>` (any of `full | usage-only | aggregate-only | cost-only | partial`) overrides the floor; `--include-partial` is shorthand for `--fidelity partial` and includes every turn — both invalid combinations exit 2 with a clear message. Coverage notes gain an `excluded N turns below <class> fidelity (… aggregate-only, … cost-only, … partial)` line whenever the gate dropped anything, the JSON output gains a top-level `fidelity` block (`{ minimum, excluded, summary }`) computed against the unfiltered slice, and per-model totals render `—` instead of `$0.00` when a model survived the filter with zero turns. When fidelity filtering is active (the default) `burn compare` falls back to the in-memory `queryAll` path so the gate is correctly applied; `--include-partial` (or `--fidelity partial`) reuses the archive's grouped SQL path from #88.

## [0.33.0] - 2026-04-27

### Added

- **`burn plans` honors per-cycle fidelity** ([#108](https://github.com/AgentWorkforce/burn/issues/108)). The list view continues to render every plan even when the cycle slice contains `partial` / `aggregate-only` / `cost-only` turns (no fidelity-based filter — `plans`, like `limits`, is permissive), but now flags low-confidence cycles so a "looks under budget" plan isn't read as authoritative. The text table grows a `confidence` column when at least one plan has any contributing turn missing per-turn input/output token data, marked `low (partial token data)`, and a footer note names the affected plan + lower-bound caveat (e.g. `note: claude-pro: 3 of 412 turns this cycle lack per-turn token data — totals are a lower bound.`). Full-fidelity cycles render exactly as before — no extra column, no footer. `--json` gains a per-plan `usage.fidelity: { confidence, summary }` block carrying the same `FidelitySummary` shape the analyze package emits elsewhere, so machine consumers can render exact counts without re-walking the ledger. `cost-only` source contributions count toward `spentUsd` and mark the cycle low-confidence on the token-coverage axis.
- **`burn waste` honors fidelity** ([#100](https://github.com/AgentWorkforce/burn/issues/100)). The attribution path (and the `--patterns retries|failures|reverts` detectors) now hard-filters the input slice against the coverage flags each detector requires — `attributeWaste` / `aggregateBy*` need `hasToolCalls` + `hasToolResultEvents`; `reverts` additionally needs `hasRawContent` (for `editPreHash` / `editPostHash`); `compaction` is unchanged because its sidecar is independent of `TurnRecord.fidelity`. When *all* turns fall below the prereq, `burn waste` exits non-zero with a message naming the missing prerequisite and the source kinds responsible (`burn waste: 142/142 turns lack tool-call/tool-result coverage required for waste attribution. Sources: codex (per-session-aggregate, missing tool-call records, tool-result events). No waste analysis was performed.`). When *some* turns survive, the text and JSON output gain an "analyzed N of M" coverage notice that names the gap per source. `--json` now carries a `fidelity` block (`{ analyzed, excluded, summary, refused }`) mirroring `summary --json`; `--patterns` JSON additionally exposes a `perDetector` array with each detector's `required` flags and `excludedBySource` breakdown. When `compaction` is in the selection it always runs — its sidecar has no per-turn fidelity requirement — so `--patterns retries,compaction` against an aggregate-only slice produces partial output rather than refusing.

### Changed

- **`burn plans` (list view) reads spend from the archive** ([#91](https://github.com/AgentWorkforce/burn/issues/91)). The list path now issues one `SUM(...) GROUP BY (source, model)` aggregate per plan against `archive.sqlite` instead of walking the full ledger once per plan. Output is byte-identical to the legacy `queryAll()` reduce path on the parity fixture (text and `--json`); `limitedData` flagging, reset-day boundaries, multi-plan ordering, and built-in presets all carry over. Pass `--no-archive` (or set `RELAYBURN_ARCHIVE=0`) to opt back into the in-memory reduce while the migration shakes out.

## [0.31.0] - 2026-04-27

### Changed

- **`burn summary` now reads from `archive.sqlite` instead of streaming `ledger.jsonl`** ([#82](https://github.com/AgentWorkforce/burn/issues/82)). The default hot path calls `buildArchive()` (cheap incremental tail scan after the per-invocation `ingestAll`) and issues SQL with filters lowered to indexed `WHERE` clauses against `turns`, replacing the per-invocation full ledger walk + stamp fold. Subagent-tree (`--subagent-tree`) and `--by-subagent-type` modes consume the same archive-derived turn slice. Output (text + `--json`) is parity-preserved against the legacy reader for the `byModel`, `totalCost`, and `fidelity` blocks. Two escape hatches preserve the old behavior: a new `--no-archive` flag and the `RELAYBURN_ARCHIVE=0` env var both revert to `queryAll`. If the archive path throws (corrupt sqlite, schema mismatch we couldn't recover from cleanly), the command transparently falls back to the streaming reader and surfaces the reason on stderr — the archive can never wedge `burn summary`.

## [0.30.0] - 2026-04-27

### Changed

- **`burn mcp-server` runs an incremental `buildArchive()` at startup** ([#97](https://github.com/AgentWorkforce/burn/issues/97)) so the first `burn__sessionCost` / `burn__currentBlock` tool call hits the SQL archive instead of re-walking the JSONL ledger. The MCP tool handlers themselves run another incremental build before each query so turns appended by hooks mid-session also show up in tool responses. The build is idempotent — a no-op when nothing has changed since the last build — and a build failure logs to stderr but never refuses to serve. Tool fallbacks to `queryAll` are wired through the new `onLog` hook so any persistent archive breakage is visible in the MCP host's stderr stream.

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

### Changed

- **`burn compare` now reads from the SQLite archive by default** ([#88](https://github.com/AgentWorkforce/burn/issues/88)). The compare table is built from a single grouped `SELECT … GROUP BY model, activity, source` over `archive.sqlite` plus a tiny per-cell median-retries follow-up, instead of streaming every turn through `queryAll()` + an in-memory reduce. Output (text, CSV, `--json`) is byte-identical to the legacy path for the parity fixture; all existing flags (`--models`, `--since`, `--project`, `--session`, `--workflow`, `--agent`, `--min-sample`) work through the SQL path. New `--no-archive` flag (also honored via `RELAYBURN_ARCHIVE=0`) preserves the in-memory path as a parity-validation / safety-net fallback.

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
