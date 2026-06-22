# Changelog

Cross-package release notes for relayburn. Package changelogs contain package-level detail.

## [Unreleased]

- `burn` subagent-tree views now require a re-ingest to render pre-Root-emission event logs (legacy reconstruction path removed).

## [3.4.0] - 2026-06-20

- `burn summary` and `burn compare` accept `--bucket <DURATION>` to emit a per-bucket time-series across the `--since` window instead of a single total (`{ "bucketSeconds": N, "buckets": [...] }` in JSON). Bucket units: `30s` / `5m` / `1h` / `12h` / `1d` / `7d` — note `m` is minutes here, unlike `--since` where `m` is months. `summary --bucket` supports only the default grouped (`byModel` / `--by-provider`) modes; `hotspots` / `overhead` are unchanged.

## [3.3.0] - 2026-06-17

- `burn summary` and `burn hotspots` no longer run a pre-query ingest sweep by default — both return in well under 100ms instead of seconds on large ledgers (`hotspots` was ~3s). Pass `--ingest` for a one-off freshen; keep the ledger current out of band with `burn ingest --watch` or the Claude Stop hook.
- Faster reads on large ledgers: time-windowed turn queries (`--since`/`--until` without a `--session`) now seek the `ts` index instead of scanning the whole `turns` table, cutting `summary`/`hotspots`/`overhead`/`compare` query time by roughly 3-4x. Output is unchanged.

## [3.2.2] - 2026-06-10

- `burn summary` now reports turns whose model has no pricing entry (`unpricedTurns`/`unpricedModels` in JSON output, warning footer in human output) instead of silently counting them at $0.

## [3.2.1] - 2026-06-09

### Added

- Pricing: added `claude-fable-5` to the vendored models.dev snapshot ($10/$50 per Mtok input/output, $1 cache-read, $12.5 cache-write, 1M context, 128K output) so cost reporting recognizes Claude Fable 5.

## [3.2.0] - 2026-06-03

### Fixed

- `@relayburn/sdk` `hotspots({ groupBy: "findings" })` now returns the exported findings result instead of rejecting the option.

## [3.1.2] - 2026-06-03

### Changed

- `ingest()` is near-instant when nothing upstream changed: a no-op sweep returns `{ ingested: 0 }` in roughly source-walk time (~0.2s) instead of ~0.7s. Adds `archive_state.source_fingerprint` (schema v6, auto-migrated).

## [3.1.1] - 2026-05-31

### Changed

- Pricing: added `claude-opus-4-8` to the vendored models.dev snapshot ($5/$25 per Mtok input/output, $0.5 cache-read, $6.25 cache-write, 1M context) so cost reporting recognizes Claude Opus 4.8.

## [3.1.0] - 2026-05-29

### Added

- `burn update`: upgrade to the latest release through whichever package manager installed the binary (npm or cargo); `--check` reports availability without installing.
- On-launch update check: interactive `burn` invocations offer to install a newer release and restart, throttled to one network probe per 24h and suppressed once a version is declined.
- `burn update toggle-auto-update [--on|--off]`: enable or disable the on-launch update check (state stored in `$RELAYBURN_HOME/update.json`).

## [3.0.0] - 2026-05-26

### Added

- `burn overhead deltas`: per-inference context-window attribution. New `--session`, `--top`, `--min-delta`, `--owner`, `--explain`, `--json` flags surface "what blew up my context between inference N and inference N+1?" — pairs same-rail `Inference` spans, attributes the delta in `input + cache_read + cache_write` to intervening `ToolResult` / `UserPrompt` / `SystemReminder` leaves, surfaces compaction events as their own row (never a negative delta), and isolates main-rail deltas from subagent rails. SDK entry point: `LedgerHandle::context_delta(opts)`. (#432)
- `burn flow --session <id>`: inference-flow DAG over a session's span
  trees. One column per turn on the main rail; dispatched subagents
  branch onto their own rails inheriting the dispatching inference's
  Y, with `dispatch` / `return` edges between rails and `unattached`
  edges for orphan subagents. Renders Mermaid (default), SVG
  (`--output flow.svg`), and JSON (`--json`). `--max-turns` defaults
  to 50. New SDK surface: `LedgerHandle::flow_graph(session, opts)`
  and free-function `flow_graph_from_trees`. (#431)
- `relayburn-sdk`: per-turn span tree as derived analytical primitive.
  New `LedgerHandle::turn_span_tree(session_id, turn_id)` and
  `session_span_trees(session_id)` verbs project `TurnRecord` +
  `tool_result_event` rows + Claude subagent sidecars into an
  OTel-style `TurnSpanTree { Turn -> { UserPrompt, Inference -> ToolUse ->
{ ToolResult, Subagent } } }`. Pure projection — no schema change,
  no caching. Orphan subagents surface as sibling `Subagent` spans
  with `unattached=true`. Locked attribute keys (`tokens.*`, `model`,
  `request_id`, `tool_use_id`, `agent_id`, `stop_reason`) for
  downstream consumers (inference-flow DAG, context-delta attribution).
  (#430)
- `relayburn-sdk`: `ingest_claude_transcript_path(ledger, path, opts)` —
  per-transcript Claude fast-path used by `burn ingest --hook claude` so
  the hook ingests only the one JSONL the payload points at instead of a
  full sweep.

### Changed

- `relayburn-cli`: `burn ingest --hook claude` now drives the new
  single-transcript fast-path, bounding per-hook cost to one JSONL parse.
- `relayburn-cli`: `burn ingest --quiet` is now accepted in default
  (one-shot) and `--watch` modes (no longer hook-only). Suppresses the
  progress spinner, watch banner, and per-tick summaries; one-shot mode
  still writes its final summary line to stdout for pipeline capture.
- `relayburn-sdk`: `Inference` aggregate keys per-API-call rollups by
  `(source, session_id, request_id)` with merged usage and `kind`
  (`reasoning` / `message` / `tool-use` / `mixed`). Read via
  `LedgerHandle::inferences(opts)` (free function `inferences()` too);
  persisted at ingest into the new `inferences` table. Falls back to
  `message_id` for harnesses without a `requestId` (Codex, opencode,
  older Claude). (#434)
- `burn summary`: one-line `Turn outcomes: …` breakdown of assistant
  `stop_reason` counts, plus a `stopReasons` block in `--json`. (#437)
- Ledger fingerprint primitive (`{count}:{maxMtimeUnix}:{totalBytes}`) for
  cheap "did anything change" polling. Exposed as `LedgerHandle::fingerprint`
  on the Rust SDK, `sdk.fingerprint()` on `@relayburn/sdk`,
  `burn state fingerprint [--session | --project]` on the CLI, and
  `burn__fingerprint` on the MCP server. Optional `Session(id)` /
  `Project(path)` scopes; all-sessions is the default. (#440)
- `burn hotspots`: new `Bytes` column on the per-tool tables and `--rank-by bytes` mode rank tools by raw output payload size, so a 4 MB Bash result that got truncated to a small token count still surfaces alongside small-bytes / large-tokens reads. JSON output gains `totalOutputBytes`, `maxOutputBytes`, and `truncatedCount` on every aggregation row. (#436)
- `relayburn-sdk`: `ToolResultEventRecord` carries new `outputBytes` and `outputTruncated` fields populated at ingest from `content.as_bytes().len()` plus Claude truncation-marker detection; `ToolAttribution` / `FileAggregation` / `BashAggregation` / `BashVerbAggregation` / `SubagentAggregation` expose the rolled-up `total_output_bytes`, `max_output_bytes`, and `truncated_count`. (#436)
- `relayburn-sdk`: Claude Task subagent sidecar discovery + pairing. New `discover_subagents` / `pair_to_main` / `count_subagents_under` helpers under `crate::reader::claude::subagents` walk `<sessionId>/subagents/agent-*.jsonl`, pair each sidecar against the parent's `toolUseResult.agentId`, and surface unpaired sidecars (slash-command synthetic dispatches and crash-mid-dispatch) as the `UnattachedGroup` bucket. Discovery is lazy — the directory is only stat'd when something asks for it. (#435)
- `burn summary`: new `subagents: X paired, Y orphan` line (and matching `subagents` key in `--json`) populated by a lazy walk over `~/.claude/projects/`. Skipped entirely when no sidecars exist anywhere reachable so pre-#435 outputs stay byte-identical. Honors `BURN_CLAUDE_PROJECTS_DIR` for test sandboxing. (#435)
- Ledger schema bumped to v4 — new nullable `turns.subagent_id TEXT` column denormalizes `TurnRecord.subagent.agent_id` so subagent rows are queryable without re-deserializing `record_json`. Migrated in place by `ALTER TABLE … ADD COLUMN`; pre-v4 rows stay `NULL` and are backfilled by `burn state rebuild`. (#435)
- `relayburn-sdk`: Claude slash-command triads (`/review`, `/init`, custom skills) now collapse into one synthetic `Skill` activity instead of inflating the activity count three rows at a time. Detection pins on the caveat → invocation → stdout parent-UUID chain shape with `<command-name>` / `<local-command-stdout>` purpose checks, so real user prompts that happen to look structurally similar are not misdetected. Token attribution stays on the underlying assistant rows — `Skill` is a view, not a billing reattribution. New `ActivityCategory::Skill` variant and `detect_slash_triads` helper. (#438)
- `relayburn-sdk`: Claude Code parser now skips harness-injected
  `<task-notification>` rows when emitting `UserTurnRecord`s. The detector
  matches shape AND purpose across three envelope variants
  (`type: "queue-operation"` + content prefix, `origin.kind`, and
  `queued_command` attachment with `commandMode`), so a real prompt that
  literally types `<task-notification>` is not filtered. Drops user-turn
  inflation from background Bash completions.
- `relayburn-sdk`: Claude Code activity classifier now associates each
  assistant turn with its user prompt by walking the `parentUuid` chain
  to the nearest user-prompt ancestor, instead of file order. Fixes
  mis-classification of late-arriving assistant rows under out-of-order
  JSONL flushes and interrupt + resume sessions. Falls back to the
  previous file-order map for legacy/malformed rows without UUIDs.
  Codex and opencode readers are unaffected — their rollouts don't carry
  an equivalent chain field. (#433)
- **BREAKING** `relayburn-sdk`: `TurnRecord.stop_reason` is now an
  `Option<StopReason>` enum (kebab-case wire form); deserialization is
  lenient so pre-3.0 ledgers replay cleanly. (#437)
- `relayburn-sdk` ledger schema bumps to v3: `turns` gains a `stop_reason
TEXT` column (#437) and `tool_result_events` gains nullable `output_bytes`
  / `output_truncated` columns (#436). Both are migrated in place on
  `Ledger::open`; existing rows leave the new columns `NULL`. Run `burn
state rebuild` to backfill an older ledger.
- `relayburn-sdk` ledger schema bumps to v5: adds the `inferences`
  derived table for per-API-call aggregates. Created idempotently on
  open; rebuilt by `burn state rebuild`. Pre-v5 ledgers stay empty
  until rebuild or the next ingest run. (#434)
- `relayburn-sdk`: Claude Code parser now correctly merges `usage`
  from the carrier row of a multi-block assistant message. Previously,
  if the row carrying the `usage` block was not the first row for a
  given `message_id`, its tokens were dropped. The new merge adopts
  the carrier row's usage values whichever row owns them. (#434)

## [2.10.0] - 2026-05-24

### Added

- `burn hotspots`: new MCP-server rollup that collapses every
  `mcp__<server>__<tool>` tool call into one row per server, so a chatty
  MCP server (e.g. relaycast) shows up as a single line instead of 50+.
  Surfaced in the human renderer (when non-empty) and as `mcpServers`
  in `--json`. (#424)

## [2.9.0] - 2026-05-21

### Added

- `@relayburn/sdk`: `writeStamp({ sessionId | messageId, enrichment })` for
  launchers that know the session id up front (e.g. preallocated Claude
  `--session-id`), bypassing the sidecar `writePendingStamp` matching path.

### Changed

- `relayburn-sdk`: dedupe ingest filesystem walks (`list_dirs`,
  `list_jsonl_files`, `walk_jsonl`) into `ingest::walk`, fix the
  `walk_jsonl` filter to match `.JSONL` case-insensitively, and collapse
  the per-harness append boilerplate (`apply_parsed_extras`) and the
  three single-harness verb skeletons (`run_single_harness`). No
  behavior change beyond the case-sensitivity fix. (#343)

## [2.8.7] - 2026-05-21

### Changed

- `relayburn-sdk`: analyze hot paths (`overhead`, `hotspots`, `quality`,
  `compare`) now aggregate per-session/per-file groups by reference instead
  of cloning `TurnRecord`s, cutting working-set memory on the most expensive
  verbs. Behavior is unchanged.

## [2.8.6] - 2026-05-12

### Changed

- `relayburn-cli`: `burn compare --since` now uses the same normalization path
  as `burn summary` and `burn hotspots`, keeping date-filter behavior
  consistent across commands.
- `relayburn-sdk`: `normalize_since` now canonicalizes outputs to UTC
  `YYYY-MM-DDTHH:MM:SS.mmmZ`. Relative ranges (`7d`, `24h`) emit `.000Z`
  instead of `Z`, and ISO inputs with offsets (e.g. `...-07:00`) are
  converted to UTC. This fixes `burn summary` / `burn hotspots` /
  `burn compare --since` silently dropping same-second turns and misordering
  offset cutoffs against the ledger's `ts >= ?` lex compare.

## [2.8.5] - 2026-05-12

### Changed

- `relayburn-sdk`: ingest verbs are now synchronous; async callers should
  run them via `tokio::task::spawn_blocking`.
- `relayburn-sdk`: lower per-record allocations in reader hashing, tool-result
  sizing, relationship dedup, and project resolution. Cuts overhead during
  large session imports and concurrent `resolve_project` calls.
- `relayburn-sdk`: `parse_claude_session` now delegates to the incremental
  parser with `start_offset = 0`, dropping the duplicate `ParseState`
  codepath. Behavior is unchanged — trailing in-progress turns and final
  JSON lines lacking a trailing newline still surface in the single-shot
  output.

### Removed

- `relayburn-sdk`: removed `run_ingest_tick`.

## [2.8.3] - 2026-05-11

### Changed

- `relayburn-cli`: `burn compare` now applies `--provider`, `--workflow`, and
  `--agent` filters at runtime. It still reads the current ledger snapshot
  (no pre-query ingest); run `burn ingest` first for freshest data.

## [2.8.1] - 2026-05-11

### Changed

- `relayburn-cli`: stdout-write failures in `--json` mode now surface as a
  non-zero exit instead of being silently dropped.
- `relayburn-sdk`: ledger query verbs (`query_turns`, `query_compactions`,
  `query_relationships`, `query_tool_result_events`, `query_user_turns`)
  now push `since` / `until` / `session_id` / `source` / `project` filters
  into SQL and reuse compiled statements via `prepare_cached`. Cuts the
  per-call cost of `burn ingest --watch` and any verb that drives a
  filtered query against a multi-month ledger.

## [2.7.4] - 2026-05-10

### Changed

- `relayburn-cli`: `--no-archive` on `burn compare` and `burn summary` is now
  an explicit no-op (accepted for TS CLI flag parity).

## [2.7.2] - 2026-05-09

### Changed

- `relayburn-cli`: `burn sessions list` human output now keeps full session ids,
  shows a single human-readable last-seen date column, and truncates long
  project paths from the beginning.
- `relayburn-sdk` / `relayburn-cli`: `burn ingest --watch` now wakes on
  filesystem events (with burst coalescing and a 30s polling backstop),
  reducing steady-state polling; pass `--no-fsevents` to force polling.

## [2.7.0] - 2026-05-09

### Changed

- `relayburn-cli`: `burn state rebuild classify` now exits 0 and runs
  the rebuild instead of erroring with "not yet implemented".

### Removed

- `relayburn-cli`: dropped the no-op flags `burn state prune --force`,
  `burn state rebuild archive --full`/`--vacuum` (and the legacy
  `vacuum` positional), `burn state rebuild all --force`, and `burn
state rebuild classify --force`. They had no effect against the
  SQLite layout; passing them now fails at parse time.

## [2.6.1] - 2026-05-09

### Changed

- `relayburn-cli`: `burn summary` partial-coverage footers now name the
  token field with the largest gap and clarify that totals still include all
  matched turns.
- `relayburn-sdk`: `ingest::pending_stamps` and `query_verbs` now use the
  `time` crate for ISO-8601 formatting/parsing (`format_iso_8601`,
  `format_iso_z`, `parse_iso_ms`). Output and the pending-stamp on-disk wire
  format are unchanged.

## [2.6.0] - 2026-05-08

### Added

- `relayburn-cli`: new `burn sessions list` subcommand that enumerates
  recent sessions (most-recent first) so callers can find a session id
  without dropping into raw SQLite. Flags: `--since` (defaults to `7d`),
  `--project`, `--grep` (case-insensitive substring against session id +
  project), `--limit` (defaults to 20), and the global `--json`. Pipes
  cleanly into `burn summary --session <id>` for drill-down.
- `relayburn-sdk`: new `sessions_list` query verb (`LedgerHandle::sessions_list`
  - free-function form) returning `SessionsListResult { sessions, limit,
truncated }`. Derived from the `turns` table so older ledgers with an
    empty `sessions` table still enumerate correctly.

### Fixed

- `relayburn-sdk`: price Codex `codex-auto-review` judge turns using the
  GPT-5.2 Codex tariff so auto-approval review spend is no longer reported as
  zero-cost.

## [2.5.1] - 2026-05-08

### Changed

- `relayburn-sdk` (Rust): refresh the bundled `models.dev` pricing snapshot so
  GPT-5.5 and current model metadata are available for cost lookups.

## [2.5.0] - 2026-05-08

### Added

- `relayburn-cli`: `burn hotspots` accepts `--session <id>`, `--workflow <id>`,
  `--provider <csv>`, `--patterns [csv]`, and `--findings`. The per-session
  aggregate view (`--session` with no id) and `--explain-drift` remain
  explicit stubs that exit 2.
- `relayburn-sdk`: `HotspotsOptions` gains `workflow` and `provider` filter
  fields, matching the shape `compare()` already exposes.
- `relayburn-cli` / `relayburn-sdk`: `burn state reset` is now a real
  presenter over the new SDK `Ledger::reset()` verb. Without flags it
  dry-runs (counts derivable rows, stamps, and content rows that would
  drop, exits 0); `--force` actually wipes both DBs and blanks the
  ingest cursors so a follow-up `burn ingest` walks every upstream file
  from offset 0; `--force --reingest` runs that ingest sweep in the
  same invocation. Replaces the prior "not yet implemented" stub.
  (#341)

### Changed

- `relayburn-sdk` (Rust): reduced hot-path CPU/alloc overhead in pricing and
  classifier parsing by caching the bundled pricing snapshot and reusing
  compiled regexes. **Breaking (Rust SDK):** `CostBreakdown::model` is now
  `Cow<'static, str>`; struct-literal construction must pass a `Cow` (for
  example, `model: value.into()`).

## [2.4.0] - 2026-05-08

### Changed

- `relayburn-sdk` (Rust): `summary_report` now exposes the richer summary result
  used by `burn summary`, so presenters can render one SDK-owned aggregate
  shape.
- `relayburn-cli` / `relayburn-sdk`: `burn summary` now accepts repeatable
  `--tag k=v` filters and `--group-by-tag <key>` to report cost/tokens by
  generic folded enrichment tags; Claude, Codex, and OpenCode pending stamps
  can now be written by external launchers.
- `relayburn-sdk` (Rust): reader hot loops in `claude.rs` and `codex.rs` now stream JSONL line-by-line via `BufReader::read_until` instead of pre-allocating a `(size - start_offset)`-byte buffer up front; only the longest single line stays resident. `memchr_newline` in the codex parser now actually uses the `memchr` crate for SIMD-accelerated newline scanning. The main `parse_claude_session` loop also drops `BufReader::lines()` in favor of `read_line` into a reused `String`. (#323)

### Removed

- `relayburn-cli`: removed the `burn run` launcher wrapper from the CLI
  surface. Launchers should write attribution with `writePendingStamp()` and
  ingest through `burn ingest` / SDK `ingest()`.
- Removed the old TypeScript implementation packages from the workspace. The
  Rust crates now own the SDK and CLI implementation, with npm packages kept
  for the Node SDK facade, MCP server, and prebuilt CLI wrappers.

## [2.0.0] - 2026-05-07

- `relayburn-sdk` (Rust): default ledger home moves from `~/.relayburn` to `~/.agentworkforce/burn` so the Rust 2.0 port and the TS 1.x package can coexist on disk during the #249 cutover. `RELAYBURN_HOME` (and the per-DB path overrides) continue to override the path; TS 1.x users on `~/.relayburn` are unaffected. Rust-port testers with data under the old path can `mv ~/.relayburn ~/.agentworkforce/burn` to carry it over (formats are not compatible — Rust treats any non-2.0 layout as empty and requires a `burn ingest` re-population).
- `relayburn-cli` (Rust): wire opencode `HarnessAdapter` via `pending_stamp::adapter_static` factory; registered in `RUNTIME_ADAPTERS`. (#248 D7)
- `relayburn-cli` (Rust): wire `burn run <harness>` driver + claude adapter (eager unit-struct in `EAGER_ADAPTERS`); `afterExit` ingest folds into `[burn] claude ingest: ...` summary line. (#248 D5)
- `relayburn-cli` (Rust): wire `burn ingest` (no-flag scan, `--watch` poll loop, `--hook claude --quiet`) and `burn mcp-server` stdio subcommand exposing `burn__sessionCost`; closes #210. (#248 D8)
- `relayburn-cli` (Rust): wire codex `HarnessAdapter` via `pending_stamp::adapter_static` factory; registered in `RUNTIME_ADAPTERS`. (#248 D6)
- `relayburn-cli` (Rust): wire `burn compare` as a presenter over `relayburn_sdk::analyze::compare` building blocks (`build_compare_table` + the per-turn fidelity gate), matching the TS CLI flag set (positional comma-separated model list, `--include-partial` / `--fidelity` / `--since` / `--project` / `--session` / `--min-sample` / `--csv` / `--no-archive`) and producing byte-equivalent stdout for the cli-golden `compare` / `compare-json` invocations. (#248 D3)
- `relayburn-cli` (Rust): port `burn overhead` and `burn overhead trim` as thin presenters over `relayburn_sdk::overhead` / `::overhead_trim`. Output (human + `--json`) is byte-equivalent with the TS CLI. (#248 D2)
- `relayburn-cli` (Rust): wire `burn state` as a typed clap subcommand with `status` (default), `rebuild`, `prune`, and `reset` verbs over `relayburn-sdk`. `state status` reports per-table row counts in `burn.sqlite`, the row count in `content.sqlite`, the `archive_state` schema/last-built/last-rebuild fields, and the resolved retention config; `--json` emits the structured `StateStatus` payload. `state rebuild {index,content,archive,all}` drives `Ledger::rebuild_derivable`; `state prune` drives `Ledger::prune_content_older_than`. `state reset` and standalone `state rebuild classify` are stubbed pending a follow-up. (#248 D4)
- `relayburn-cli` (Rust): wire `burn summary` and `burn hotspots` as thin presenters over `relayburn-sdk`, matching the TS CLI's flag set and stdout byte-for-byte (default + `--json`). Un-ignores the four matching golden invocations. (#248 D1)
- `relayburn-sdk-node` (Rust): napi-rs bindings skeleton — `#[napi]` shims for every public verb in `relayburn-sdk` (`summary`, `sessionCost`, `overhead`, `overheadTrim`, `hotspots`, `search`, `exportLedger`, `exportStamps`, async `ingest`, plus `ledgerOpen`), with u64 token counts surfaced as JS `BigInt`, ISO-8601 timestamps as `String`, async verbs returning `Promise<T>`, and a typed `BurnError` mapping for SDK failures. (#247)
- `relayburn-cli` (Rust): introduce the harness substrate — `HarnessAdapter` trait, lazy compile-time `phf` registry (`lookup` / `list_harness_names`), and the shared `pending_stamp::adapter` factory codex + opencode will reuse. Adapter slots in the registry are reserved but empty pending the Wave 2 PRs (#248-d/e/f). `relayburn-sdk` re-exports `start_watch_loop`, `WatchController`, `write_pending_stamp`, `PendingStampHarness`, and friends so the CLI doesn't have to reach into private SDK modules. (#248)
- `relayburn-cli` (Rust): scaffold the clap v4 derive root with global `--json` / `--ledger-path` / `--no-color` flags, eight stub subcommands (`summary`, `hotspots`, `overhead`, `compare`, `run`, `state`, `ingest`, `mcp-server`), and shared `render::{table,json,error}` helpers. Stubs exit `1` with a `not yet implemented` message (or a `{"error": …}` envelope under `--json`); Wave 2 fan-out PRs replace each stub with a thin presenter over `relayburn-sdk`. (#248 part a)
- `relayburn-cli` (Rust): add the CLI golden-output test rig — synthetic fixture ledger under `tests/fixtures/cli-golden/`, a node script that captures TS-CLI stdout/stderr across 16 invocations (summary / hotspots / overhead / overhead-trim / compare / state-status in TTY + `--json`, plus help text for ingest / run / mcp-server / top-level), and `crates/relayburn-cli/tests/golden.rs` — a `BURN_GOLDEN=1`-gated diff runner Wave 2 PRs flip on per-command via `enabled: true` in `invocations.json`. (#248)
- `@relayburn/sdk` (npm 2.x): scaffold the `packages/sdk-node/` umbrella + four per-platform packages (`@relayburn/sdk-{darwin-arm64,darwin-x64,linux-arm64-gnu,linux-x64-gnu}`) resolved via `optionalDependencies`. Adds the napi-rs build matrix in `.github/workflows/napi-build.yml`, an esbuild bundle smoke test, and a deep-equal conformance test gate against the TS 1.x SDK across the six verbs (`ingest`, `summary`, `sessionCost`, `overhead`, `overheadTrim`, `hotspots`). Conformance test is skipped until `crates/relayburn-sdk-node` lands its bindings (#247 part a). (#247 part b)
- `relayburn-ingest` (Rust): port the per-process gap-warning state machine (`gap` module — `record_session_gap`, `emit_gap_warning`, `count_tool_call_gaps`, `reset_ingest_gap_warnings`, `set_ingest_gap_writer`) and `reingest_missing_content` (`reingest` module). Suppression mirrors the TS surface: one warning per fresh affected session, silent on steady-state, re-fires after the affected set decays back to empty. `relayburn-ledger` adds `Ledger::list_user_turn_session_ids` to power the `reingest_missing_content` skip filter alongside `list_content_session_ids`. (#278)
- `relayburn-analyze` (Rust): port the behavioral-pattern detectors (`patterns` module). `detect_patterns` runs retry-loop, failure-run, cancellation-run, compaction-loss, edit-revert, OpenCode skill-recall-dup, OpenCode skill-pruning-protection, OpenCode system-prompt-tax, and edit-heavy detectors against an ordered turn stream, with optional content-sidecar / tool-result-event / user-turn enrichment. Public surface: `detect_patterns`, `DetectPatternsOptions`; per-pattern result structs are re-exported from `findings` (`RetryLoop`, `FailureRun`, `CancellationRun`, `CompactionLoss`, `EditRevertCycle`, `SkillRecallDup`, `SkillPruningProtection`, `SystemPromptTax`, `EditHeavySession`, `SessionPatternSummary`, `PatternsResult`, `PatternEventSource`). (#275)
- `relayburn-analyze` (Rust): port the tool-output-bloat detector — Signal A's `BASH_MAX_OUTPUT_LENGTH` static-config check (with `~/.claude/settings.json` + `<cwd>/.claude/settings.json` loader) and Signal B's cross-harness observed-bloat aggregation, plus the `WasteFinding` adapter. Public surface mirrors `@relayburn/analyze`: `BASH_MAX_OUTPUT_ENV_KEY`, `DEFAULT_BLOAT_TOKEN_THRESHOLD`, `detect_observed_bloat`, `detect_static_config_bloat`, `detect_tool_output_bloat`, `load_claude_settings`, `project_claude_settings_path`, `user_claude_settings_path`, `tool_output_bloat_to_finding`. (#271)
- `relayburn-ingest` (Rust): port the standalone primitives — `pending_stamps` (binary-compatible with the TS `@relayburn/ingest` wire format), `walk` (`walk_jsonl` / `walk_opencode_sessions`), `watch_loop` (`tokio::time::interval`-driven `WatchController` with graceful stop), and the typed `cursors` module layered on the SQLite ledger's cursor blob. Public verb surface (`ingest_all`, per-harness verbs, `reingest_missing_content`) is wired; per-harness orchestration follow-ups deferred to dedicated sub-issues. (#245)
- `relayburn-analyze` (Rust): port the ghost-surface detector — `ghost_surface` and `ghost_surface_inputs` modules with Claude / Codex / OpenCode adapters, slash-command miners, the per-source-scoped orchestrator, and the `WasteFinding` envelope adapter. Findings sort deterministically by `(cost desc, sizeTokens desc, path)` and dedup against the OpenCode catalog-bloat detector via `countedByCatalogBloat`. (#273)
- `relayburn-analyze` (Rust): port the `compare` aggregator — `build_compare_table` for the in-memory `(model, activity)` rollup with per-cell turn / edit / one-shot / priced / cost / cache-hit / median-retries metrics, plus `compare_from_archive` sourced from the SQLite ledger via `Ledger::query_turns`. Public surface: `CompareCell`, `CompareTable`, `CompareTotals`, `CompareOptions`, `CompareCategory`, `DEFAULT_MIN_SAMPLE`, `compare_from_archive`, `CompareFromArchiveResult`. (#269)
- `relayburn-analyze` (Rust): port `subagent_tree` and `claude_md` modules. `build_subagent_tree` / `aggregate_subagent_type_stats` walk per-session subagent invocations (relationship-row substrate with legacy `subagent` fallback) and roll up self/cumulative cost. `parse_claude_md` / `attribute_claude_md` / `build_trim_recommendations` / `render_unified_diff_for_recommendation` produce CLAUDE.md section attribution and trim diffs whose unified-diff format stays byte-aligned with the TS implementation. (#272)
- `relayburn-analyze` (Rust): port the `hotspots` aggregator — `attribute_hotspots` composes the per-tool sized / even-split attribution loop (paying-turn rate, sibling-cap on initial cost, proportional cacheRead allocation on persistence, source-aware reasoning via `cost_for_turn`) with the `aggregate_by_file` / `aggregate_by_bash` / `aggregate_by_bash_verb` / `aggregate_by_subagent` rollups. Public surface mirrors `@relayburn/analyze`: `attribute_hotspots`, `aggregate_by_file`, `aggregate_by_bash`, `aggregate_by_bash_verb`, `aggregate_by_subagent`, `AttributionMethod`, `BashAggregation`, `BashVerbAggregation`, `FileAggregation`, `HotspotsOptions`, `HotspotsResult`, `SessionTotals`, `SubagentAggregation`, `ToolAttribution`. Per-row USD totals match the TS implementation within 1e-9. (#274)

## [1.9.0] - 2026-05-03

### Changed

- **Architecture: `@relayburn/sdk` is now the canonical in-process query surface.** Dependency order moves from `… → mcp → cli → sdk → relayburn` to `… → sdk → mcp → cli → relayburn`; `@relayburn/mcp` now depends on `@relayburn/sdk` and rewrites `burn__sessionCost` as a thin wrapper over the SDK's new `sessionCost()` function. New read verbs should land in the SDK first; MCP and CLI become presenters (tool definitions / table rendering) over the same SDK calls so query logic stops drifting between them.
- `burn compare` joins `summary` / `sessionCost` / `overhead` / `overheadTrim` as a thin presenter over `@relayburn/sdk`'s new `compare()` function. The archive-vs-ledger branching and fidelity-gate logic move into the SDK so a future `burn__compare` MCP tool (and embedders) can wrap the same call without re-implementing them.

### Breaking Changes

- `@relayburn/sdk` `hotspots()` now returns a discriminated union (`{ kind: 'attribution' | 'bash' | 'bash-verb' | 'file' | 'subagent' | 'findings' }`) instead of either a raw attribution blob or a flat findings array. CLI / MCP / embedded callers must branch on `kind`. Mirrors the shape `burn hotspots --json` emits and adds four narrow `groupBy` views for single-axis consumers.

## [1.8.0] - 2026-05-02

### Added

- Recognise `_meta.replaces` / `_meta.collapsedCalls` annotations on Claude `tool_result` blocks across reader → analyze → CLI, so replacement tools (e.g. relaywash) get attributed estimated tokens saved in `burn summary` and `burn summary --by-tool`.

## [1.7.0] - 2026-05-02

### Added

- `burn hotspots --patterns=tool-call-pattern` flags vanilla call patterns with consolidatable overhead (Glob → Grep → Read sequences, single-file edit clusters, `git status` / `pnpm test` / `gh pr` Bash calls), with per-occurrence counts and token-overhead estimates. Vendor-neutral — downstream tools map patterns to specific consolidations.
- `@relayburn/sdk` `hotspots()` now also surfaces `tool-output-bloat`, `ghost-surface`, and `tool-call-pattern` findings (previously only the core `detectPatterns` set).
- New `@relayburn/ingest` package owns session-store discovery, parse-and-append orchestration, pending-stamp resolution, and watch-loop primitives extracted from `@relayburn/cli`. CLI commands and harness adapters now consume ingest from `@relayburn/ingest`; `@relayburn/sdk` drops its `@relayburn/cli` dependency and imports `ingest()` from the new package and `buildGhostSurfaceInputs` from `@relayburn/analyze`.

## [1.5.0] - 2026-05-02

### Added

- `RELAYBURN_STORAGE=sqlite` selects a new single-file SQLite backend (default path `~/.relayburn/burn.sqlite`, override via `RELAYBURN_SQLITE_PATH`). Replaces JSONL ledger + sidecars + `.idx` files with one DB; ingest paths use native `INSERT OR IGNORE` on content-addressed dedup hashes so multi-writer setups converge without external indexes.

### Removed

- Removed Burn's budget and quota tracking surfaces: `burn budget`, monthly plan config APIs, and the MCP `burn__currentBlock` tool are no longer shipped.

## [1.2.2] - 2026-04-30

### Changed

- Publish workflow now always targets all lockstep packages and no longer exposes per-package release choices.

## [1.2.1] - 2026-04-30

### Added

- `burn overhead trim --json` now emits structured trim recommendations for programmatic review while preserving the existing unified diff text output.

## [1.1.0] - 2026-04-29

### Changed

- Publish workflow now creates one GitHub Release per lockstep publish, anchored to `relayburn`, with all package versions listed in the release body.

## [1.0.0] - 2026-04-29

### Added

- `burn ingest --watch --opencode-stream` now ingests stream-owned OpenCode sessions directly at completed tool-call grain while keeping file ingest as the fallback.

### Changed

- Renamed the attribution surface: `burn hotspots` replaces `burn waste` and `burn diagnose`, while `burn overhead` replaces `burn context`.
- `burn summary --subagent-tree` now renders persisted session relationship graphs while preserving legacy subagent-tree output for older data.

### Fixed

- OpenCode stream cursor progress now survives concurrent file-ingest fallback saves.

## [0.45.0] - 2026-04-29

### Added

- Defaulted user-turn block sizing to cl100k, with a measurement script that reports token-count and tool-attribution drift against the bytes/4 heuristic fallback.

### Fixed

- `burn rebuild --content` now fast-skips renamed Codex rollout files using their embedded session metadata.

## [0.43.0] - 2026-04-29

### Changed

- `burn waste --patterns` now bases retry and failure findings on persisted tool-result chronology when available, while preserving legacy fallback behavior.
- Spawn-env and native sidechain attribution now share session relationship records, and `burn diagnose --explain-drift` surfaces sessions where they disagree.
- Provider-aware CLI rendering now uses shared analyze helpers for effective-provider resolution and aggregation.
- Per-tool cost attribution now uses persisted user-turn block sizes in summary and hotspots reports, with rebuild backfill for historical sessions.

## [0.42.0] - 2026-04-28

### Added

- OpenCode passive ingest now records compaction events, so compaction waste analysis covers Claude, Codex, and OpenCode.

## [0.41.0] - 2026-04-28

### Added

- Waste detectors now share a structured `WasteFinding` output. `burn waste --patterns --findings` renders all findings in one severity-ranked table and includes the same list in JSON.

## [0.39.0] - 2026-04-28

### Changed

- `burn compare` now requires a comma-separated model list as its first argument. The old `--models` flag was removed; filters and output schemas are unchanged.

## [0.37.0] - 2026-04-28

### Added

- `burn waste --patterns edit-heavy` flags sessions with many edits and too few reads across Claude, Codex, and OpenCode.

## [0.35.0] - 2026-04-28

### Added

- Codex passive ingest now records compaction events.
- `burn waste --patterns` adds OpenCode skill-recall, skill-pruning, and system-prompt tax detectors.

### Changed

- `burn run <claude|codex|opencode>` replaces the separate harness wrappers with one adapter-backed command.

### Removed

- Removed `burn claude`, `burn codex`, and `burn opencode`. Use `burn run <name>`.
- Removed `burn rebuild-index`. Use `burn rebuild --index`.

## [0.34.0] - 2026-04-27

### Changed

- `burn compare` now excludes low-fidelity turns by default, with `--fidelity` and `--include-partial` for overrides.

## [0.33.0] - 2026-04-27

### Added

- `burn plans` now reports per-cycle fidelity so partial data is marked as a lower-confidence estimate.

## [0.27.0] - 2026-04-26

### Added

- User-turn block sizes are now persisted for Claude, Codex, and OpenCode, giving `burn waste` a better fallback when full content is unavailable.

## [0.19.0] - 2026-04-26

### Added

- Added execution-graph records for session relationships and tool-result events.
- Claude ingest now writes the new graph records alongside turns and content.

## [0.18.0] - 2026-04-26

### Fixed

- Fixed reasoning-token pricing. Codex reasoning is no longer double-billed, and distinct reasoning tariffs from pricing data are honored.

## [0.13.1] - 2026-04-25

### Added

- Added the rebuildable `archive.sqlite` analytics read model and `burn archive build | rebuild | status`.
- Added `@relayburn/mcp` and `burn mcp-server` so agents can query their own session cost and quota state.

## [0.11.0] - 2026-04-25

### Added

- Added monthly plan tracking with built-in Claude and Cursor presets, projections, runway, reset-day support, and `burn limits` integration.

## [0.9.0] - 2026-04-24

### Added

- Added Claude subagent tree reconstruction plus `burn summary --subagent-tree` and `--by-subagent-type`.

## [0.8.0] - 2026-04-24

### Added

- Added Claude Code hook-based ingest without mutating global Claude settings.

## [0.7.0] - 2026-04-24

### Added

- Codex and OpenCode parsers now capture content sidecars.
- Added `burn rebuild --content` and `listContentSessionIds()` for sidecar backfills.

## [0.6.0] - 2026-04-24

### Added

- Added quality signals: inferred session outcome, one-shot edit rate, and retry volume.
- Added waste-pattern detectors for retry loops, failure runs, compaction loss, and edit reverts.

## [0.5.0] - 2026-04-24

### Changed

- Moved earlier unreleased changelog notes into their release sections. No package behavior changed.

## [0.4.0] - 2026-04-23

### Added

- Added `burn waste` for per-tool, per-file, Bash, and subagent cost attribution.

## [0.3.0] - 2026-04-23

### Added

- Added `burn context` for context-file cost attribution.
- Added `burn context advise` for read-only trim recommendations.

## [0.2.0] - 2026-04-23

### Added

- Added `burn compare` for model-by-activity cost and quality comparison.
- Added `burn rebuild --reclassify` and `--index`.
- Activity classification now covers Claude, Codex, and OpenCode, with normalized tool names and six new categories.

### Fixed

- Ledger appends now lock against reclassification rewrites, preventing lost rows.

### Changed

- Tightened deploy/build classifier patterns.
- `burn compare` now rejects `--json` and `--csv` together.

## [0.1.0] - 2026-04-22

### Added

- Initial release of `@relayburn/reader`, `@relayburn/ledger`, `@relayburn/analyze`, and `@relayburn/cli`.
