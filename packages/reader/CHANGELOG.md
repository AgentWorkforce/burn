# Changelog

All notable changes to `@relayburn/reader` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Claude parser emits `fork` and `continuation` `SessionRelationshipRecord` rows** ([#112](https://github.com/AgentWorkforce/burn/issues/112)). Closes the deferred-work item from #77/#42: the Claude passive reader now populates the full `RelationshipType` lattice instead of only `root` / `subagent`. Per-file evidence — in-log `sessionId` mismatches against the on-disk filename, the first user line's `parentUuid`, the first non-empty `version` field, all in-file uuids, and `/resume` / `/continue` slash-command markers — is collected during the existing parse pass and surfaced as a new `evidence: ClaudeRelationshipEvidence` field on `ParseResult` / `ParseIncrementalResult`. A `/resume` marker emits a local `continuation` row with `relatedSessionId` set to the resumed-from id; a new exported `reconcileClaudeSessionRelationships(inputs)` helper takes per-file evidence from a multi-file pass and emits the cross-file `fork` / `continuation` rows that single-file parsers can't surface. Existing `root` / `subagent` rows are stamped with `sourceSessionId` (foreign in-log id) and `sourceVersion` whenever the file carries them. Reconciliation strategy is **append, not mutate**: a prior `root` row and a later `continuation` / `fork` row for the same session id produce different `relationshipIdHash` values, so both rows coexist on disk and consumers prefer the more specific row when both are present. Re-ingesting a session is idempotent — the writer's existing dedup folds duplicates. New `ParseOptions.fileSessionId` lets callers pin the canonical session id explicitly; when omitted but `sessionPath` is set, the parser derives it from the `.jsonl` basename.
- **Codex parser populates `TurnRecord.fidelity`** ([#84](https://github.com/AgentWorkforce/burn/issues/84)). `parseCodexSession` and `parseCodexSessionIncremental` now stamp `fidelity` on every emitted turn at `granularity: 'per-turn'`, mirroring the Claude parser. Coverage flags follow the rollout source: `hasInputTokens` / `hasOutputTokens` / `hasReasoningTokens` / `hasCacheReadTokens` flip to `true` only when a `token_count` event with `total_token_usage` arrived between `task_started` and `task_complete`; turns whose source omitted token counts now report `class: 'partial'` (the numeric `usage` fields still default to 0, but the coverage flag is the honest signal). `hasToolCalls` / `hasToolResultEvents` / `hasRawContent` are capability flags — true even on tool-less turns. `hasCacheCreateTokens` and `hasSessionRelationships` stay `false` (Codex rollouts have no cache-create or parent-tracking concept yet — the latter waits on #42 / #63). Closes the `unknown === 0` requirement from #41 for Codex sessions.
- **OpenCode parser populates `TurnRecord.fidelity`** ([#89](https://github.com/AgentWorkforce/burn/issues/89), follow-up to [#41](https://github.com/AgentWorkforce/burn/issues/41) / [#76](https://github.com/AgentWorkforce/burn/issues/76)). `parseOpencodeSession` and `parseOpencodeSessionIncremental` now stamp `fidelity` on every emitted turn at `granularity: 'per-turn'`. Usage coverage flags (`hasInputTokens`, `hasOutputTokens`, `hasReasoningTokens`, `hasCacheReadTokens`, `hasCacheCreateTokens`) reflect *presence* on the upstream `tokens` block — folded across both the assistant message and any `step-finish` parts that carry tokens — so a turn that never received cache fields reports `hasCacheReadTokens: false` instead of silently rendering `cacheRead === 0`. Capability flags (`hasToolCalls`, `hasToolResultEvents`, `hasSessionRelationships`, `hasRawContent`) are always true. Closes the "0 vs unknown" ambiguity for OpenCode in `summarizeFidelity` and `hasMinimumFidelity`.

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
