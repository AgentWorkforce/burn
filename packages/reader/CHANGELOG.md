# Changelog

All notable changes to `@relayburn/reader` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **OpenCode passive reader populates the execution graph** ([#93](https://github.com/AgentWorkforce/burn/issues/93)). `parseOpencodeSession` / `parseOpencodeSessionIncremental` now return `relationships: SessionRelationshipRecord[]` and `toolResultEvents: ToolResultEventRecord[]` alongside the existing `turns` / `content` / `userTurns` arrays. One `root` row is emitted per session; a `subagent` row is added when the session payload carries `parentID`, with `relatedSessionId` pointing at the parent session. Each tool part with a resolved `state.output` produces a terminal-status `ToolResultEventRecord` whose `status` follows the same failure rules as the existing `erroredCallIds` set (`state.status === 'error'` or `metadata.exit !== 0` for bash-family tools), with `contentLength` / `contentHash` computed from the stringified output (metadata only — no raw bytes stored). Per-`toolUseId` `callIndex` and per-pass monotonic `eventIndex` mirror the Claude convention. `Subagent.isSidechain` continues to populate from `parentID` unchanged. Resumed incremental passes don't re-emit events for already-seen assistant messages; relationship rows re-emit on every pass and the writer dedups them by hash.

## [0.19.0] - 2026-04-26

### Added

- **Execution graph foundation: `SessionRelationshipRecord` and `ToolResultEventRecord`** (#42, first PR). Two new normalized record shapes that sit beside `TurnRecord` and preserve cross-source metadata that's currently flattened or lost: how sessions relate (`root` / `continuation` / `fork` / `subagent`) and chronological tool-result events keyed by `toolUseId`. Both carry `v: 1`, `source`, and a `sessionId`; relationship rows include `parentToolUseId` / `agentId` / `subagentType` / `description` for subagent edges; tool-result events carry `status` (`running` / `completed` / `errored` / `cancelled` / `unknown`), an `eventSource` discriminator (`tool_result` / `subagent_notification` / `queue_event` / `progress_event` / `function_call_output`), `contentLength` + `contentHash` (metadata only — no raw content), and `agentId` for spawn events.
- **Claude passive reader populates the execution graph.** `parseClaudeSession` and `parseClaudeSessionIncremental` now return `relationships` and `toolResultEvents` alongside the existing `turns` / `content` / `events` arrays. Roots are emitted once per session id; one `subagent` row is emitted per distinct invocation discovered (joining to `Subagent.agentId`); each `tool_result` block in a user line becomes a `ToolResultEventRecord` with monotonic `eventIndex` and per-`toolUseId` `callIndex`. Spawn events (Agent/Task tool_results that map to a sidechain) are post-annotated with the resolved subagent's `agentId` so consumers can join the two record types. Incremental parser respects the same `endOffset` deferral the existing content/event paths use, so resumed ingest doesn't double-emit. Codex / OpenCode population deferred to follow-up PRs.

## [0.16.0] - 2026-04-25

### Added

- **Per-user-turn block-size capture for Claude sessions** ([#2](https://github.com/AgentWorkforce/burn/issues/2)). `parseClaudeSession` / `parseClaudeSessionIncremental` now return a `userTurns: UserTurnRecord[]` alongside `turns`, recording each user-turn's content blocks (`tool_result` and free-text) with `byteLen`, `approxTokens` (bytes/4 heuristic), `toolUseId`, and `isError`. Each `UserTurnRecord` carries `precedingMessageId` / `followingMessageId` so consumers can place the user turn between two assistant turns without re-walking `parentUuid` chains. This is the prerequisite for per-tool-call cost attribution (`burn waste`): combined with the existing per-turn `usage`, callers can recover the input-side delta caused by individual tool calls (Anthropic only reports usage at message granularity). Additive — no on-disk schema change, existing `TurnRecord` consumers are unaffected. Codex and OpenCode parsers are scoped for follow-up.

## [0.14.0] - 2026-04-25

### Added

- **Coverage and fidelity metadata on `TurnRecord`** ([#41](https://github.com/AgentWorkforce/burn/issues/41) — first cut). New optional `TurnRecord.fidelity` field with three pieces: `granularity` (`per-turn` | `per-message` | `per-session-aggregate` | `cost-only`), per-field `coverage` flags (`hasInputTokens`, `hasOutputTokens`, `hasReasoningTokens`, `hasCacheReadTokens`, `hasCacheCreateTokens`, `hasToolCalls`, `hasToolResultEvents`, `hasSessionRelationships`, `hasRawContent`), and a derived `class` (`full` | `usage-only` | `aggregate-only` | `cost-only` | `partial`). Coverage is strictly about *availability*: `hasOutputTokens: false` means "we don't know," not "0 output tokens." New `EMPTY_COVERAGE`, `classifyFidelity`, and `makeFidelity` helpers exported alongside the types. The Claude parser populates fidelity on every turn; usage-coverage flags reflect which fields the upstream `usage` block actually carried (so a turn with no `cache_creation` reports `hasCacheCreateTokens: false`). Codex/OpenCode parsers do not yet populate fidelity — deferred to a follow-up. Older ledger writers leave `fidelity` undefined; downstream code treats absence as best-effort full fidelity for backward compat.

## [0.13.1] - 2026-04-25

### Changed

- Bump packages to v0.13.0

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
