# Changelog

All notable changes to `@relayburn/cli` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`burn claude-md`** — CLAUDE.md hot-path cost attribution. Reports per-session average / p95 cost attributable to CLAUDE.md, aggregated over the query window, with a section-level ranked breakdown. Closes #10.
  - Flags: `--project <path>`, `--since 7d`, `--json`.
  - Detects root `CLAUDE.md` and nested `.claude/CLAUDE.md`.
  - Uses the git-canonical `projectKey` (via `resolveProject`) for the ledger query when available, so multiple worktrees of the same repo roll up together; falls back to the filesystem path when no git remote is set.
- **`burn claude-md advise`** — emits unified-diff TRIM hunks for the most expensive sections. Paths render POSIX-relative to the project root so they apply with `git apply` / `patch`. No `--apply` flag: burn never mutates CLAUDE.md.
  - Flags: `--top <n>` (default 3), `--project <path>`, `--since 7d`.

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
