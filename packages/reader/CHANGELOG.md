# Changelog

All notable changes to `@relayburn/reader` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Content capture for Codex and OpenCode parsers** (#33 follow-up). Both parsers now emit `ContentRecord` entries when `contentMode === 'full'`, matching the shape the Claude parser already produced. Covers `text` (user/assistant), `thinking` (codex reasoning), `tool_use`, and — most importantly for `burn waste` attribution — `tool_result` keyed by the same `call_id` / `callID` the tool call carries. In codex, content only emits for turns that commit at `task_complete`; uncommitted content is dropped and will be re-emitted once the turn commits. Removes the `TODO(#33-followup)` markers in `codex.ts` and `opencode.ts`.

## [0.7.0] - 2026-04-24

### Added

- **Add content capture for Codex and OpenCode parsers (#33 follow-up)** (#33)

## [0.6.0] - 2026-04-24

### Added

- **Implement waste-pattern detectors (retry loops, failure runs, compaction loss, edit-revert)** (#11)

## [0.5.0] - 2026-04-24

### Changed

- Clean up changelogs: move [Unreleased] content into 0.3.0/0.4.0 sections

## [0.4.0] - 2026-04-23

Synchronized version bump alongside `@relayburn/cli@0.4.0` / `@relayburn/analyze@0.4.0` / `@relayburn/ledger@0.4.0`. No functional changes in this package.

## [0.3.0] - 2026-04-23

Synchronized version bump alongside `@relayburn/cli@0.3.0` / `@relayburn/analyze@0.3.0` / `@relayburn/ledger@0.3.0`. No functional changes in this package.

## [0.2.0] - 2026-04-23

### Added

- **Activity classifier now runs for Codex and OpenCode turns.** Previously only Claude Code turns received an `activity` label; everything else fell into `unclassified` and `burn compare` could not bucket cross-harness work.
- **`TOOL_ALIASES` map** in the classifier normalizes harness-specific tool names (`apply_patch`, `exec_command`, `shell`, lowercase `read`/`write`/`edit`/`bash`/`grep`/`glob`/`webfetch`/`task`, plus codex agent tools) onto canonical Claude names so the rule tables stay single-source. Exported as `normalizeToolName(name)`.
- **Six new activity categories** (taxonomy expanded from 12 → 18): `reasoning`, `docs`, `deps`, `format`, `review`, `verification`.
  - `reasoning`: tool-less turns billed reasoning tokens (extended thinking, Codex `reasoning_output_tokens`).
  - `docs`: edit turns where every edited file is a doc (`*.md`, `*.mdx`, `*.rst`, `*.adoc`, `*.txt`, `README*`, `CHANGELOG*`, `docs/**`).
  - `deps`: bash matching `npm/pnpm/yarn/bun install|add`, `pip install`, `uv add`, `cargo add`, `go get`, `bundle install`, `brew install`, etc.
  - `format`: bash matching `prettier --write`, `eslint --fix`, `black`, `ruff format`, `cargo fmt`, `gofmt`, etc.
  - `review`: read-only inspection (`git status/diff/show/log/blame`, `gh pr view/diff/checks`) or read-only turns whose prompt asks for review.
  - `verification`: lint/typecheck/check commands (`eslint`, `tsc --noEmit`, `cargo check`, `mypy`, `ruff check`, `prettier --check`, etc.).
- **Retries-based debugging fallback.** Edit turns with `retries >= 2` (≥2 edit→bash→edit cycles in one turn) classify as `debugging` even without an explicit error keyword.
- **Codex reader carries user prompt text** (skipping `<environment_context>` / `<permissions>` / `# AGENTS.md` boilerplate), assistant `output_text`, and errored call IDs from `exec_command_end.exit_code !== 0` and `patch_apply_end.success === false` into the classifier.
- **OpenCode reader reads user messages alongside assistants**, skips `synthetic: true` text parts (harness-injected nudges), and detects failed tool parts via `state.status === "error"` or `state.metadata.exit !== 0`.
- **Reasoning tokens flow into classification.** `ClassificationInput.reasoningTokens` lets the classifier distinguish reasoning-only turns from chit-chat conversation.
- **Expanded `TEST_PATTERNS`** to catch e2e/browser runners: `playwright`, `cypress`, `puppeteer`, `make test`, `ctest`.

### Changed

- `BUILD_DEPLOY_PATTERNS` tightened. The catch-all `/\bdeploy\b/` is replaced with explicit verbs per-tool (`vercel/netlify/flyctl/railway/sst deploy|up`, `kubectl apply/rollout/set`, `helm install/upgrade`, `terraform apply/plan/destroy`, `make build|release|dist|package|deploy`).

## [0.1.0] - 2026-04-22

### Added

- **Initial release.** Pure parsers (no I/O writes, no shared state) that turn agent session logs into `TurnRecord[]` and `ContentRecord[]`.
- Claude Code reader (`parseClaudeSession`, `parseClaudeSessionIncremental`).
- Codex reader (`parseCodexSession`, `parseCodexSessionIncremental`) with cumulative-token-delta accounting and resume state across `task_complete` boundaries.
- OpenCode reader (`parseOpencodeSession`, `parseOpencodeSessionIncremental`) reading the `~/.local/share/opencode/storage` per-session/message/part layout.
- Activity classifier (`classifyActivity`, `countRetries`) — 12 categories, deterministic and rule-based, no LLM in the loop. Runs against Claude Code turns only at this version.
- Git-canonical project resolution (`resolveProject`, `canonicalizeRemoteUrl`, `parseGitConfig`) so projectKey survives across worktrees.
- `argsHash` content fingerprinting for tool-call dedup.
