# Competitor landscape

Reference notes from surveying ten token-usage tools in the Claude Code / multi-agent space. The goal was to decide what burn should port, what it should deliberately skip, and what unique territory it owns.

**Burn's meta-goal** (repeated here because it shapes every evaluation below): answer "would the same work cost less on a different model, harness, or tool — in dollars or quota consumption?" Everything in this doc is graded against that question, not against "is this tool well-built."

## Summary

| # | Project | Stars | Stack | Shape | Load-bearing contribution |
|---|---|---|---|---|---|
| 1 | [TokenTracker](https://github.com/mm7894215/TokenTracker) | 242 | JS + Swift | Local aggregator + macOS menu bar | Quota API endpoints, incremental cursors, git canonicalization |
| 2 | [lazyagent](https://github.com/chojs23/lazyagent) | 46 | Go + SQLite | Live TUI observability | Hook-based ingest — PostToolUse carries `tool_response` content |
| 3 | [prism](https://github.com/jakeefr/prism) | 19 | Python + Textual | CLAUDE.md waste diagnosis | CLAUDE.md hot-path concept; waste-pattern detectors |
| 4 | [TokenArena](https://github.com/poco-ai/TokenArena) | 93 | TS + Postgres | Social leaderboard | — (orthogonal) |
| 5 | [opencode-tokenscope](https://github.com/ramtinJ95/opencode-tokenscope) | 138 | TS, OpenCode plugin | Deep single-session analyzer | First-`cache_write` context decomposition; OpenCode skill mechanics |
| 6 | [agentic-metric](https://github.com/MrQianjinsi/agentic-metric) | 182 | Python + TUI | Observability dashboard | Cursor-is-dead evidence |
| 7 | [ccusage](https://github.com/ryoppippi/ccusage) | — | TS monorepo | Multi-harness CLI family | 5-hour block forecasting; MCP server pattern; Amp reader |
| 8 | [tokscale](https://github.com/junhoyeo/tokscale) | 2057 | Rust core + TS | Most mature multi-collector | Content-fingerprint dedup; 5 new collectors (Roo Code, Kilo CLI, Mux, Crush, Synthetic) |
| 9 | [agentsview](https://github.com/wesm/agentsview) | 816 | Go + SQLite + Svelte + Tauri | Local-first session intelligence | Outcome-inference algorithm (triggered content-storage reversal); 3 new collectors (OpenHands, iFlow, Cortex Code) |
| 10 | [codeburn](https://github.com/getagentseal/codeburn) | 3247 | TS + Swift (macOS menubar) | TUI dashboard with activity/optimize/compare | Activity classifier; model-comparison-by-category (closest to meta-goal in the field); three new waste detectors; plan-based quota tracking |

## Per-project detail

### 1. TokenTracker

**What it is:** Claude-Code-focused local aggregator with 11 parsers and a native macOS menu bar app. Zero-config hook install (`npx tokentracker-cli` writes a SessionEnd hook into the user's Claude `settings.json`). Optional cloud leaderboard.

**Key files:**
- `src/lib/rollout.js` — all 11 parsers in one file (Claude, Codex, Cursor, Gemini, OpenCode, OpenClaw, EveryCode, Kiro, Kimi, Hermes, Copilot)
- `src/lib/usage-limits.js:55-588` — quota-endpoint reverse engineering across providers
- `src/lib/local-api.js:10-117` — `MODEL_PRICING` table (100+ models)

**Distinctive mechanisms:**
- Incremental file cursor: `rollout.js:74-183` tracks `{inode, offsetBytes, mtimeMs}` in `cursors.json`, seeks to tail on subsequent runs.
- Git-canonical project key: `rollout.js:1608-1630` walks up from cwd, parses `[remote "origin"]` from `.git/config`, canonicalizes to `host/owner/repo`.
- Quota-API endpoints documented:
  - Claude: `GET https://api.anthropic.com/api/oauth/usage` — returns `five_hour`, `seven_day`, `seven_day_opus`, `extra_usage`
  - Codex: `GET https://chatgpt.com/backend-api/wham/usage` — returns `primary_window` + `secondary_window`
  - Cursor: SQLite auth extract + CSV fetch (now broken post-2026-01, see agentic-metric)
  - Gemini: per-model remaining-token budget endpoint
- Dedup: hash on `messageId:requestId` (`rollout.js:901-904`)

**Ceiling:** Message-level usage only — no tool-call content, no per-tool-call attribution.

**Burn issues from this survey:**
- #4 (incremental cursors + git canonicalization + dedup index)
- #5 (`burn budget` quota-window tracking — endpoint list cribbed directly)

**Explicitly not adopted:** 30-minute UTC bucket-at-ingest (wrong for per-tool-call attribution), SessionEnd hook installer (burn's spawner-controlled `--settings` path is strictly better), native macOS app (out of scope), optional cloud leaderboard (off-mission).

### 2. lazyagent

**What it is:** Go + SQLite TUI for live inspection of running Claude/Codex/OpenCode sessions. Installs Claude Code's full hook repertoire via `lazyagent init claude`.

**Key files:**
- `internal/claude/parser.go:31-75` — hook event parsing; `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SubagentStop`, `Notification`, `SessionStart`, `SessionEnd`
- `internal/app/ingest_claude.go:58-117` — subagent correlation via `PendingAgentSpawn` queue keyed by `tool_use_id`
- `internal/app/ingest_claude.go:26-33` — OpenCode-wraps-Claude-Code dedup guard
- `scripts/claude-hook.sh` — 2 lines: `exec "$BIN" ingest --runtime claude`

**The breakthrough finding:** Claude Code's `PostToolUse` hook fires with the full `tool_response` content in the payload. Confirmed at `parser.go:58-61` (`raw["tool_input"]` and `raw["tool_response"]` extraction). That means per-tool-call token sizing doesn't need the delta-math of #2 — the size can be read directly.

**Other distinctive patterns:**
- Subagent hierarchy as a tree: `root_agent_id` + `owner_agent_id` per event, with `PreToolUse(Agent)` → `PostToolUse(Agent)` correlated through the pending-spawn queue. Strictly richer than burn's flat `subagent.isSidechain` flag at `packages/reader/src/claude.ts:127-128`.
- Pending-spawn correlation: any tool with async resolution can use the pattern (stash by `tool_use_id` on Pre, pop on Post).

**Burn issues from this survey:**
- #7 (hook-based Claude ingest via spawner-injected `--settings`)
- #8 (subagent tree as first-class primitive)

**Explicitly not adopted:** TUI itself (out of scope), Go rewrite (no), SQLite storage (burn chose JSONL ledger deliberately), pricing table (narrower than burn's models.dev snapshot).

### 3. prism

**What it is:** Python + Textual, narrow-but-deep CLAUDE.md waste diagnosis. Single-session analyzer. Real-data examples from their README: *"6738% CLAUDE.md re-read cost"*, *"480% of total session tokens"*.

**Key files:**
- `prism/analyzer.py:173` — CLAUDE.md cost proxy: `reread_cost = tool_call_count × claude_md_size_tokens`
- `prism/advisor.py:24-32` — action taxonomy: `ADD | TRIM | WARN | RESTRUCTURE` (no `MOVE` despite README mention)
- `prism/advisor.py:119-125, 234-235` — section ranking by position zones (top 20%, mid 20-75%, bottom 25%) with keyword regex, not heading-based
- `prism/analyzer.py:299-300` — retry loop detection, N=3 consecutive, `tool_name + tool_input` dict equality
- `prism/analyzer.py:347` — edit-revert detection: same `file_path` within 3 tool calls (no content comparison — weak)
- `prism/parser.py:67` — compaction marker: `SystemRecord.subtype == "compact_boundary"`
- `prism/analyzer.py:507-583` — CLAUDE.md adherence ("Mr. Tinkleberry") detector, hand-coded per-rule checkers

**Load-bearing insight:** CLAUDE.md rides in every turn's cached context — the cumulative cost compounds for every session in the repo. This is the highest-leverage "switch a choice, save spend" insight in the survey. Prism's framing is right; their math is crude (proxy ratio rather than real `cacheRead` share). Burn can do it properly.

**Waste-pattern detectors also mapped:**
1. Retry loops (N=3 consecutive same call with failures)
2. Edit-revert cycles (prism's version is weak — positional, not content-based)
3. Consecutive tool failures (`is_error: true` sequences)
4. Compaction loss events (`compact_boundary` marker)

**Burn issues from this survey:**
- #10 (CLAUDE.md hot-path cost attribution — with better math than prism)
- #11 (waste-pattern detection — retry/failure/compaction kept from prism; edit-revert replaced with content-hash based detection)

**Explicitly not adopted:** CLAUDE.md adherence detector (hand-coded per-rule, doesn't scale — see rejection note on #6), Python stack, Textual TUI, HTML dashboard, `advise --apply` auto-mutation (too much autonomy), letter-grade health scores.

### 4. TokenArena

**What it is:** Chinese-language hosted social leaderboard at token.poco-ai.com. OAuth login (Discord, GitHub, GitLab, Google, Linux.do, Watcha), hashed project names for anonymity, shareable badges, community rankings.

**Key files (for reference only — nothing adopted):**
- `cli/src/parsers/` — 12 parsers: `claude-code`, `codex`, `copilot-cli`, `droid`, `gemini-cli`, `hermes`, `kimi-code`, `openclaw`, `opencode`, `pi-coding-agent`, `qwen-code`
- `cli/src/domain/project-identity.ts` — HMAC-sha256 hashing of project names with user salt (privacy primitive, not a rollup key — opposite of burn's need)

**Why nothing was adopted:** The product is entirely social. Project identity hashing is for leaderboard anonymity, not cross-worktree rollup. SQLite upload to Postgres violates local-first. No issues filed.

**Value preserved:** The 12 parsers are a reference library if burn ever needs to add a specific collector — TokenArena's parsers are cleanly separated one-file-per-source (unlike TokenTracker's monolithic `rollout.js`).

### 5. opencode-tokenscope

**What it is:** TypeScript plugin that runs inside OpenCode as a `/tokenscope` slash command. Produces a detailed text report per session. Focused on one session at a time, in-session invocation model.

**Key files:**
- `plugin/tokenscope-lib/context.ts:303-354` — **first-`cache_write` context decomposition** (the big takeaway)
- `plugin/tokenscope-lib/telemetry.ts:85-109` — `collectTelemetryCalls` iterates OpenCode's `step-finish` parts per message
- `plugin/tokenscope-lib/skill.ts` (765 lines) — OpenCode-specific skill mechanics with source links to OpenCode's code

**The context-decomposition method (better than rolling-share):**
```
cachedContextAtStart = firstCacheWriteTokens(session)      // real API-reported value
claude_md_share = claude_md_tokens / cachedContextAtStart  // measured / measured
attributed_cost = Σ (cacheRead_T × claude_md_share × cache_read_price)
```
Two measured numbers, one division, computed once per session — versus prism's `count × size` proxy or our original per-turn rolling-share estimate.

**OpenCode skill mechanics documented:**
- Always-injected verbose XML skill catalog on every API call (cached prefix tax)
- `skill({name})` tool results are NOT deduplicated — calling the same skill N times adds the content N times
- Skill content is in `PRUNE_PROTECTED_TOOLS = ["skill"]` — never pruned during compaction

**Burn issues from this survey:**
- Comment on #10 (first-`cache_write` decomposition as the preferred math — universal: applies to Claude's CLAUDE.md and OpenCode's AGENTS.md)
- Comment on #11 (three OpenCode-specific waste detectors: skill catalog bloat, skill recall non-dedup, skill content pruning protection)
- Prompted the PR #9 step-finish review (later softened after tokscale cross-reference)

**Explicitly not adopted:** Plugin architecture (burn is CLI-first and cross-session), multi-tokenizer infrastructure (burn reads `cacheRead` from the API, doesn't tokenize).

### 6. agentic-metric

**What it is:** Python + TUI observability tool. `top`-like framing for live agent monitoring. Process detection + JSONL/SQLite parsing.

**Key files:**
- `src/agentic_metric/collectors/` — Python collectors for Claude Code, Codex, OpenCode, Qwen Code, VS Code Copilot Chat
- `src/agentic_metric/collectors/_process.py` — process detection of running agents

**The one critical datum:** Their README documents Cursor's server-side migration. Verbatim:
> *"Cursor stopped writing token usage data (tokenCount) to its local state.vscdb database around January 2026 (approximately version 2.0.63+). All inputTokens/outputTokens values are now zero. Cursor has moved usage tracking to a server-side system."*

This is the basis for burn's **#22 Cursor wontfix** decision.

**Other observations (noted, not adopted):**
- Process detection for "currently running agents" — burn doesn't need it because relay/workforce own the spawn
- `agentic-metric bar` status-line output (`AM: $1.23 | 4.5M`) — parked as a v0.3+ polish candidate
- Pricing CLI (`pricing set`, `reset`, family fallback) — UX parity with burn's existing `$RELAYBURN_HOME/models.dev.json` overrides

No issues filed directly. The Cursor datum blocked one decision (#22).

### 7. ccusage

**What it is:** pnpm monorepo with per-harness packages — `ccusage` (Claude Code), `@ccusage/codex`, `@ccusage/opencode`, `@ccusage/pi`, `@ccusage/amp`, `@ccusage/mcp`. Shared core via `packages/internal/` (pricing) and `packages/terminal/` (rendering).

**Key files:**
- `apps/ccusage/src/_session-blocks.ts:8` — `DEFAULT_SESSION_DURATION_HOURS = 5` (hardcoded, inferred from Anthropic docs, not API)
- `apps/ccusage/src/_session-blocks.ts:295-321` — `projectBlockUsage()` — burn-rate forecast math
- `apps/ccusage/src/_session-blocks.ts:128-133, 216-246` — `isGap` block insertion for idle periods
- `apps/mcp/src/mcp.ts:46-184` — MCP tool registration (6 tools: `daily`, `session`, `monthly`, `blocks`, `codex-daily`, `codex-monthly`)
- `apps/mcp/src/mcp.ts:192-218` — stdio / HTTP transport
- `apps/amp/src/data-loader.ts` — Amp thread reader (`~/.local/share/amp/threads/`)
- `apps/amp/src/_consts.ts:7, 12, 22, 27, 32` — Amp path resolution with `AMP_DATA_DIR` env fallback

**Block forecasting math:**
```
burnRate.tokensPerMinute = totalTokensSoFar / minutesSinceBlockStart
projectedAdditionalTokens = burnRate × remainingMinutes
projectedBlockTotal = actualSoFar + projectedAdditionalTokens
```
Requires per-turn usage within the current 5-hour window (from ledger). No network dependency.

**MCP server as closed-loop architecture:**
Spawner registers `@relayburn/mcp` on every agent. Agent mid-session queries `burn__currentBlock()` or `burn__sessionCost()` to self-check. Relay's routing logic can ask *"you're at 80% of your window — downgrade to Haiku for the rest?"* and let the model decide. None of the surveyed tools do this.

**Amp's distinctive log shape:** `usageLedger.events[]` carries per-event tool-call granularity with `operationType`, `fromMessageId`, `toMessageId`, `credits`, `tokens`. Uses **credits, not USD** — Sourcegraph's credit-to-dollar conversion is external.

**Burn issues from this survey:**
- Comment on #5 adding local-derived block forecasting alongside the OAuth endpoint
- #25 Amp collector
- #26 `@relayburn/mcp` server

**Explicitly not adopted:** Per-harness separate NPM packages (burn already has the internal/core split done), 3D contributions graph (out of scope).

### 8. tokscale

**What it is:** 2057 stars. Rust core (`tokscale-core`, `tokscale-cli`) + TypeScript wrappers. Ratatui TUI. Supports 20 distinct collectors, LiteLLM pricing with 1-hour disk cache + OpenRouter fallback. The most production-mature tool in the survey.

**Key files:**
- `crates/tokscale-core/src/sessions/` — 20 reader implementations, one file per source
- `crates/tokscale-core/src/sessions/opencode.rs:185-197, 217-223` — **content-fingerprint dedup** `hash(timestamp, model, tokens, cost)` as secondary key
- `crates/tokscale-core/src/sessions/opencode.rs:233-306` — migration cache at `~/.cache/tokscale/opencode-migration.json`
- `crates/tokscale-cli/src/cursor.rs:13-14` — Cursor API endpoint
- `crates/tokscale-cli/src/cursor.rs:407-447` — session token storage at `~/.config/tokscale/cursor-credentials.json`
- `crates/tokscale-core/src/sessions/synthetic.rs:61-82, 99` — model-name normalization + provider reassignment

**Content-fingerprint dedup:** Primary `(source, sessionId, messageId)` hash catches exact re-parses; secondary `hash(ts, model, tokens, cost)` in a rolling window catches path-migration cases where IDs regenerate. Two-tier.

**Cursor: they do support it, but as an online service.** User runs `tokscale cursor login`, extracts session token from Cursor's web dashboard, token is stored at `~/.config/tokscale/cursor-credentials.json`, tokscale polls `https://cursor.com/api/dashboard/export-usage-events-csv?strategy=tokens`. Not portable to burn's local-first model. Reinforces #22 wontfix.

**OpenCode handling:** Reads message-level tokens from either SQLite (`~/.local/share/opencode/opencode.db`) or legacy JSON (`~/.local/share/opencode/storage/message/*.json`), with dedup across both during migration. **Confirms message-level is the mainstream choice** — softened the PR #9 step-finish concern.

**Synthetic reattribution pattern:** Novel. Not a traditional collector — scans turns from OTHER collectors (Claude, OpenCode) for model-ID prefixes (`hf:`, `accounts/fireworks/models/`, `synthetic/`), relabels them as `provider: synthetic`, normalizes model names. Post-processing layer, not a reader. Modeled in burn as a query-time classifier in `@relayburn/analyze`.

**Burn issues from this survey:**
- Comment on #22 (Cursor wontfix reinforced with tokscale's approach documented)
- Comment on PR #9 (step-finish concern softened)
- Comment on #4 (fingerprint dedup as secondary)
- #27 Roo Code (+ KiloCode VS Code extension)
- #28 Kilo CLI
- #29 Mux
- #30 Crush
- #31 Synthetic reattribution

**Explicitly not adopted:** Rust rewrite, Ratatui TUI, social leaderboard / Wrapped feature, 3D contributions graph, multi-language READMEs.

### 9. agentsview

**What it is:** Go + SQLite + Svelte SPA + Tauri desktop wrapper. Built by Wes McKinney. 816 stars. Local-first session browser with full-text search, activity heatmaps, per-session analytics, and per-model cost dashboards. 16 supported agents.

**Key files:**
- `internal/db/schema.sql:55-82` — `messages` table with full `content TEXT` columns for prompts and responses
- `internal/db/schema.sql:140-191` — `tool_calls` and `tool_result_events` tables storing raw `input_json` and `result_content` (not hashed)
- `internal/db/db.go:54-71` — FTS5 virtual table `messages_fts` indexing message content
- `internal/signals/outcome.go:1-95` — session outcome inference from metadata + last-assistant-text pattern matching
- `internal/signals/toolhealth.go`, `context.go`, `score.go` — tool-health, compaction-detection, and health-scoring implementations for #11's detector suite
- `internal/sessionwatch/watcher.go:55-109` — polling + mtime-fallback live updates (not fsnotify)
- `internal/parser/openhands.go:23-47`, `iflow.go:27-46`, `cortex.go:342+`, `kiro.go:34-59`, `kiro_ide.go:18-49` — parser references

**The breakthrough finding: outcome inference.** `outcome.go` classifies sessions as `completed | abandoned | errored | unknown` with `high | medium | low` confidence based on `MessageCount`, `EndedWithRole`, `FinalFailureStreak`, recency, and give-up-pattern matching on `LastAssistantText`. Automatic (no user tagging), general (not per-rule checkers), confidence-weighted. This became the primary candidate for #6 quality signal.

**The design reversal it triggered:** agentsview stores full message content (FTS5-indexed prompts and responses; raw tool inputs and outputs) — the opposite of burn's original no-content stance. After weighing analytical upside vs. local-only storage concerns, burn flipped to a **sidecar content store** with a 90-day retention default and a `content.store=hash-only | off` opt-out. This strengthened #2, #3, #6, #7, #10, #11.

**Reference implementations for scoped issues:** agentsview independently implemented much of what #11 specs (retry counting, consecutive failures, compaction detection). Threshold tuning already done empirically — port rather than rederive.

**READMEware caveat:** the README advertises session archetypes `automation / quick / standard / deep / marathon`, but source only implements `is_automated` (binary flag on hardcoded prefix matching in `internal/db/automated.go:32-46`). The others don't exist in code. Don't chase.

**Burn issues from this survey:**
- #33 content sidecar with retention + opt-out (the design reversal)
- Comments on #2, #3, #6, #7, #10, #11 reflecting content-store primacy
- Comment on #6 proposing outcome-inference as primary quality signal
- Comment on #11 citing agentsview's `signals/` as reference implementations
- Comment on #17 splitting Kiro into CLI + IDE variants
- #34 OpenHands CLI collector
- #35 iFlow collector
- #36 Cortex Code collector

**Explicitly not adopted:** Go rewrite, SQLite as primary storage (JSONL ledger + JSONL sidecar keeps the design consistent), Tauri desktop app, FTS5 indexing (deferred — may come back if full-text search becomes a user need), PostgreSQL team-sync (out of scope; relay/workforce's job if they want team-visible content), the quick/standard/deep/marathon archetypes (don't exist in code).

### 10. codeburn

**What it is:** TypeScript CLI + TUI dashboard (Ink + React), Swift macOS menubar app. 3247 stars — most-starred in the survey. Distinguished from the rest by its depth on *analysis*, not just collection. Supports Claude Code, Codex, Cursor (claimed), cursor-agent, OpenCode, Pi, OMP, GitHub Copilot.

**Key files:**
- `src/classifier.ts:1-164` — rule-based 11-category activity classifier (tool-pattern + keyword)
- `src/classifier.ts:120-140` — `countRetries()` detecting edit→bash→edit cycles (one-shot success metric)
- `src/compare-stats.ts:26-74` — per-model aggregation of calls, cost, tokens, one-shot rate, cache hit rate
- `src/compare-stats.ts:188-240` — `computeCategoryComparison` — one-shot rate by (model, category) pair
- `src/optimize.ts` (1135 lines, 11 finders) — waste detectors with structured fix actions (`{type: 'paste' | 'command' | 'file-content'}`)
- `src/optimize.ts:731-821` — ghost agents/skills/commands detection
- `src/optimize.ts:596-639` — low read:edit ratio detection
- `src/optimize.ts:835-857` — BASH_MAX_OUTPUT_LENGTH bloat detection
- `src/context-budget.ts:1-150` — static setup-cost measurement (complement to dynamic hot-path math in #10)
- `src/plan-usage.ts:1-149` + `src/plans.ts:1-56` — Claude Pro/Max + Cursor Pro monthly quota tracking
- `src/providers/cursor.ts:76-88` — Cursor SQLite query (unvalidated against current Cursor schema)

**The three breakthrough findings:**

1. **Activity classifier.** 11 rule-based categories (`planning | delegation | testing | git | build-deploy | coding | debugging | refactoring | feature | exploration | brainstorming | conversation`). Deterministic, no LLM dependency. The missing primitive that no other tool in the landscape has. Unlocks honest like-to-like comparison.

2. **Model comparison by observed activity category.** Given both Sonnet and Haiku in the user's history, computes one-shot rate and cost-per-turn *per category*. Sidesteps the unrealistic counterfactual problem (*\"would Haiku have succeeded on that past Sonnet session?\"*) by using observed parallel data. This is the closest any surveyed tool has gotten to burn's meta-goal.

3. **One-shot rate as a quality proxy.** `oneShotRate = oneShotTurns / editTurns` — turns with edits that had zero retries. No content dependency, works per-turn. Complements agentsview's outcome inference (which operates per-session with content needed).

**Waste detectors to port** (three new, beyond #3/#10/#11 scope):
- Ghost agents/skills/commands (static system-prompt bloat)
- Low read:edit ratio (simpler symptom detector for careless editing)
- Bash output-limit config bloat (raised `BASH_MAX_OUTPUT_LENGTH` creates per-call waste)

Plus **structured fix actions** (`WasteAction = { type: 'paste' | 'command' | 'file-content' }`) — better output shape than free-text recommendations. Direct upgrade for #11.

**Plan tracking** (`plan-usage.ts`, `plans.ts`) covers **monthly quota** — now surfaced with the 5-hour window under `burn budget`. Both matter for Pro/Max users.

**Cursor support: unresolved.** Claims to work via the local `cursorDiskKV` SQLite table. Code has no schema validation — silently returns empty on post-Cursor-migration installs. Either Cursor reversed course or codeburn's parser is broken but unreported (3247 stars and April 2026 push doesn't prove the parser works for current Cursor). Empirical verification needed before reopening #22.

**Burn issues from this survey:**
- #37 rule-based activity classifier (the foundational primitive)
- #38 `burn compare` model comparison by activity category (the meta-goal query)
- #39 plan-based monthly quota tracking (complement to #5)
- Comment on #6 proposing one-shot rate as a second primary quality signal
- Comment on #10 proposing `burn context-budget` as a static-cost sibling command
- Comment on #11 adding three detectors + upgrading fix-action shape
- Comment on #22 documenting codeburn's conflicting Cursor evidence and the empirical-verification path

**Explicitly not adopted:** TUI dashboard (`dashboard.tsx`, `compare.tsx`) — orthogonal; Swift macOS menubar app — out of scope; \"provider plugin system\" (advertised as pluggable in the README; source shows hardcoded imports with lazy loading for optional dependencies — not actually user-extensible); daily cache (`daily-cache.ts`) — burn's #4 incremental cursor addresses the same concern for JSONL.

## Landscape takeaways

1. **Nobody else does per-tool-call attribution.** All 10 tools stop at per-message or per-turn totals. Anthropic returns `usage` at the message level, and every surveyed project treated that as a ceiling. Burn's plan — delta-math fallback (#2), hook-path precise (#7), consumed by `burn waste` (#3) — is novel territory.

2. **Nobody has a workflow/stamp concept.** Burn's `@relayburn/ledger.stamp` API for attributing turns to `workflowId` / `agentId` / `persona` at query time is unique in the landscape. Every competitor aggregates by source-harness or model. This is the primitive that makes cross-harness comparison possible for the meta-goal.

3. **Nobody closes the loop.** All 10 tools are report-only. `@relayburn/mcp` (#26) turns burn into in-session self-awareness for spawned agents. Ccusage is closest (their MCP server) but exposes only broad queries, not session-scoped self-reporting.

4. **Stars track different axes.** Tokscale (2057 stars) has the broadest collector coverage. Codeburn (3247 stars) has the deepest analytical surface — activity classification + model comparison by category is the closest anyone has come to answering burn's meta-goal. Agentsview (816 stars) is middle ground with its outcome-inference algorithm. The most-starred project is not always the deepest on any given axis; depth on the meta-goal question clusters around codeburn.

5. **Activity classification is the missing primitive across the rest of the landscape.** Only codeburn labels what kind of work a session is doing. Without this, any comparison between models or harnesses collapses to crude aggregates — you can't say \"Sonnet is 20% more efficient than Haiku on refactoring\" if you don't know which sessions *were* refactoring. Burn adopts this (#37) and builds the comparison query on top of it (#38).

6. **CLAUDE.md hot-path is the highest-leverage specific waste finder.** Prism surfaced it but with crude math. Burn can do it properly (#10) using the first-`cache_write` anchor from opencode-tokenscope. Compounds across every future session in the repo — higher leverage than model-switching.

7. **Hooks beat post-hoc log parsing** for any collector that supports them. Lazyagent showed this: `PostToolUse` carries `tool_response` content directly. No delta math. The constraint — "we always control the spawn" (relay/workforce) — makes hook installation per-invocation via `--settings` cleaner than global config mutation.

8. **Cursor is not going to be viable locally.** Confirmed by agentic-metric and cross-verified against tokscale's online-service workaround. Unless Cursor reverses course, burn does not support it.

9. **Storing content is a deliberate reversal, not an oversight.** Burn originally avoided storing prompts and responses. After surveying agentsview's outcome-inference algorithm (which needs `LastAssistantText`), and realizing #2, #3, #6, #10, #11 all get meaningfully stronger with content available, burn flipped to a sidecar content store (#33) with 90-day retention and a `content.store=hash-only | off` opt-out. The sidecar is separate from the main ledger so aggregate queries stay fast; hash-only mode restores the original minimal-storage behavior for sensitive environments.

## Projects noted but not surveyed

These came up in passing but weren't explored in depth. Potential future survey targets if a specific feature gap appears:

- **ccusage-family reach** — `@ccusage/pi`, `@ccusage/amp`, `@ccusage/mcp` are in our clone but only Amp and MCP were examined deeply.
- **Cline** (cline/cline) — ancestor of Roo Code and KiloCode. If the VS Code extension family matters, reading Cline directly would let us validate Roo Code / KiloCode's shared format against the upstream.
- **Aider** (paul-gauthier/aider) — popular terminal agent, not in any surveyed project's collector list. Likely has a JSONL session log.
- **Warp AI** — Warp terminal's agent. Unknown data surface.
- **Zed AI** — Zed editor's AI integration. Unknown.

## Issue-to-source cross-reference

For each filed issue, the project(s) it drew from:

| Issue | Title | Primary source(s) |
|---|---|---|
| #2 | Preserve user-turn block sizes | lazyagent (fallback when hooks unavailable) |
| #3 | `burn waste` per-tool-call attribution | Original concept |
| #4 | Incremental cursors + git-canonical project keys | TokenTracker (cursors, git); tokscale (fingerprint dedup, added later) |
| #5 | `burn budget` quota-window tracking | TokenTracker (endpoints); ccusage (forecasting, added later) |
| #6 | Outcome/quality signal design | Original concept; prism adherence rejected as primary |
| #7 | Hook-based Claude ingest via `--settings` | lazyagent |
| #8 | Subagent tree as first-class | lazyagent |
| #10 | CLAUDE.md hot-path cost attribution | prism (concept); opencode-tokenscope (better math) |
| #11 | Waste-pattern detection | prism; opencode-tokenscope (OpenCode-specific detectors added later) |
| #12 | Codex collector | TokenTracker, TokenArena, agentic-metric, lazyagent |
| #13 | Gemini CLI collector | TokenTracker, TokenArena |
| #14 | GitHub Copilot CLI | TokenTracker, TokenArena |
| #15 | VS Code Copilot Chat | agentic-metric |
| #16 | Qwen Code collector | TokenArena, agentic-metric |
| #17 | Kiro collector | TokenTracker |
| #18 | Kimi Code collector | TokenTracker, TokenArena |
| #19 | Hermes Agent collector | TokenTracker, TokenArena |
| #20 | OpenClaw collector | TokenTracker, TokenArena |
| #21 | Every Code collector | TokenTracker |
| #22 | Cursor (wontfix) | agentic-metric (evidence); tokscale (online workaround, reinforces wontfix) |
| #23 | Droid collector | TokenArena; later tokscale reference |
| #24 | Pi Coding Agent collector | TokenArena; later tokscale reference |
| #25 | Amp collector | ccusage |
| #26 | `@relayburn/mcp` server | ccusage |
| #27 | Roo Code + KiloCode collectors | tokscale |
| #28 | Kilo CLI collector | tokscale |
| #29 | Mux collector | tokscale |
| #30 | Crush collector | tokscale |
| #31 | Synthetic reattribution | tokscale |
| #33 | Content sidecar with retention and opt-out | agentsview (design reversal trigger) |
| #34 | OpenHands CLI collector | agentsview |
| #35 | iFlow collector | agentsview |
| #36 | Cortex Code collector | agentsview |
| #37 | Activity classifier | codeburn |
| #38 | `burn compare` by activity category | codeburn (method); Original concept (framing) |
| #39 | Plan-based monthly quota tracking | codeburn |

## Methodology note

Survey conducted over one extended session in April 2026. For each project, the flow was: fetch README via `gh api`, skim for distinctive claims, then read specific source files that supported or undermined those claims. Generic README features were not treated as findings — only what the source revealed beyond the marketing got captured. File:line references preserved above are the load-bearing pointers for future porting work.
