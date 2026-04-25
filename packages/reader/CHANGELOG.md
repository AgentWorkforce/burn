# Changelog

All notable changes to `@relayburn/reader` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Execution graph foundation: `SessionRelationshipRecord` and `ToolResultEventRecord`** (#42, first PR). Two new normalized record shapes that sit beside `TurnRecord` and preserve cross-source metadata that's currently flattened or lost: how sessions relate (`root` / `continuation` / `fork` / `subagent`) and chronological tool-result events keyed by `toolUseId`. Both carry `v: 1`, `source`, and a `sessionId`; relationship rows include `parentToolUseId` / `agentId` / `subagentType` / `description` for subagent edges; tool-result events carry `status` (`running` / `completed` / `errored` / `cancelled` / `unknown`), an `eventSource` discriminator (`tool_result` / `subagent_notification` / `queue_event` / `progress_event` / `function_call_output`), `contentLength` + `contentHash` (metadata only — no raw content), and `agentId` for spawn events.
- **Claude passive reader populates the execution graph.** `parseClaudeSession` and `parseClaudeSessionIncremental` now return `relationships` and `toolResultEvents` alongside the existing `turns` / `content` / `events` arrays. Roots are emitted once per session id; one `subagent` row is emitted per distinct invocation discovered (joining to `Subagent.agentId`); each `tool_result` block in a user line becomes a `ToolResultEventRecord` with monotonic `eventIndex` and per-`toolUseId` `callIndex`. Spawn events (Agent/Task tool_results that map to a sidechain) are post-annotated with the resolved subagent's `agentId` so consumers can join the two record types. Incremental parser respects the same `endOffset` deferral the existing content/event paths use, so resumed ingest doesn't double-emit. Codex / OpenCode population deferred to follow-up PRs.

## [0.9.0] - 2026-04-24

### Added

- **Subagent tree reconstruction from Claude JSONL.** `parseClaudeSession` / `parseClaudeSessionIncremental` now walk `parentUuid` chains to resolve the subagent invocation each sidechain turn belongs to. `TurnRecord.subagent` gains `agentId` (stable id per invocation — the root user uuid), `parentAgentId` (the enclosing invocation's agentId, or the session id for first-level subagents), `parentToolUseId` (the Agent/Task `tool_use.id` that spawned the invocation), `subagentType`, and `description` (both lifted from the spawning Agent/Task tool input). Tool_result continuations within the same invocation are distinguished from nested spawns via `tool_use_id` matching, so long subagent chains don't get mis-split. Walk results are memoized per uuid so deeply nested trees stay linear. `Subagent.isSidechain` is unchanged — existing consumers keep working. Closes [#8](https://github.com/AgentWorkforce/burn/issues/8).

## [0.8.0] - 2026-04-24

### Added

- **Add Claude hook-based ingest and settings**

## [0.7.0] - 2026-04-24

### Added

- **Content capture for Codex and OpenCode parsers** (#33 follow-up). Both parsers now emit `ContentRecord` entries when `contentMode === 'full'`, matching the shape the Claude parser already produced. Covers `text` (user/assistant), `thinking` (codex reasoning), `tool_use`, and — most importantly for `burn waste` attribution — `tool_result` keyed by the same `call_id` / `callID` the tool call carries. In codex, content only emits for turns that commit at `task_complete`; uncommitted content is dropped and will be re-emitted once the turn commits. Removes the `TODO(#33-followup)` markers in `codex.ts` and `opencode.ts`.

## [0.6.0] - 2026-04-24

### Added

- **Waste-pattern detectors** — reader-side signals (`toolCalls[].isError`, per-turn `retries`) feeding the analyze-side detectors for retry loops, failure runs, compaction loss, and edit-revert. Closes [#11](https://github.com/AgentWorkforce/burn/issues/11).

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
