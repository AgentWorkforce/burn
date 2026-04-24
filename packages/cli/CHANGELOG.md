# Changelog

All notable changes to `@relayburn/cli` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`burn summary --quality`** — appends a quality rollup to the summary output: outcome counts (completed / abandoned / errored / unknown) plus the weighted one-shot edit rate across the matched sessions. Closes [#6](https://github.com/AgentWorkforce/burn/issues/6).
  - Opportunistically loads per-session content sidecars (when available) so give-up phrase detection can downgrade assistant-ended confidence. Sidecar reads run with a concurrency cap of 8 so large ledgers don't serialize I/O.

## [0.6.0] - 2026-04-24

### Added

- **Add outcome inference + one-shot rate quality signals (closes #6)** (#6)
- **Implement waste-pattern detectors (retry loops, failure runs, compaction loss, edit-revert)** (#11)

### Changed

- Address PR #53 review comments (#53)

### Documentation

- Document quality signals in CHANGELOGs (#53)

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
