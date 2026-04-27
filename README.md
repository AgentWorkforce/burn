![relayburn](./burn-readme-banner.png)

# relayburn

> Know where every token went — and why. An attribution layer for AI agent spend.

Part of the `relay*` family of single-concern primitives alongside `relayfile`, `relaycast`, `relayauth`.

## What this is for

Agent spend happens in a blind spot. You can see a daily dollar total, and maybe a breakdown by model. You cannot see *which tool call, which file, which subagent, or which workflow step* drove the cost. That is the question burn exists to make answerable.

The deeper question burn is built around is:

> **Would the same work cost less with a different model, harness, or tool choice — in dollars or quota consumption?**

You cannot answer that from aggregate spend. It requires attribution at the level of the actual work: *this Read cost $0.47 because it added 8,200 tokens to context that rode in every one of the next 23 turns' cache-reads.* Once spend is visible at that grain, the choice between Opus and Haiku, between Claude Code and another harness, or between letting an agent re-read a file and passing it a cached summary, becomes a decision you can reason about — not a guess.

Burn is local-first. Data lives in an append-only JSONL ledger on your machine. Burn never phones home. Pricing is looked up at query time from a vendored snapshot of [models.dev](https://models.dev), so rate corrections never require rewriting the ledger.

## Quick start

```bash
git clone <repo> && cd burn
pnpm install && pnpm run build
alias burn="node $PWD/packages/cli/dist/cli.js"   # or use npx @relayburn/cli once published

# Wrap your agent — burn captures the session log on exit.
burn claude --tag workflow=refactor-auth -- --resume

# Read it back.
burn summary --since 7d
```

That's the whole loop: **wrap → query**. Everything below is what you can ask once the ledger has data.

## What burn can do

The `burn` binary is a single CLI with subcommands for spawning, querying, and diagnosing.

```
burn summary       [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--provider <p>] [--quality]
                   [--by-provider] [--subagent-tree <session-id>] [--by-subagent-type] [--no-archive]
burn by-tool       [--since 7d] [--project <path>] [--session <id>] [--provider <p>]
burn waste         [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--provider <p>] [--all] [--json]
                   [--patterns[=retries,failures,compaction,reverts]]
burn compare       [--models a,b] [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--min-sample <n>] [--json|--csv]
burn diagnose      <session-id> [--json]
burn limits        [--watch [5s]] [--json] [--no-api] [--no-forecast]
burn plans         [add|remove|set-reset-day] …  (run `burn plans help` for full usage)
burn context       [advise] [--project <path>] [--since 7d] [--kind <k>] [--top <n>] [--json]
burn claude        [--tag k=v ...] [-- <claude args>]
burn codex         [--tag k=v ...] [-- <codex args>]
burn opencode      [--tag k=v ...] [-- <opencode args>]
burn watch         [--interval <ms>] [--once]
burn ingest        --runtime claude [--quiet]     (reads hook payload on stdin)
burn mcp-server    [--session-id <uuid>]          (stdio MCP server for in-session self-query)
burn content prune [--days <n>] [--force]
burn archive       build | rebuild | status [--json]
burn rebuild         --index | --reclassify [--force]
```

The walkthrough below shows what each one returns, with real output from a live ledger.

### `burn summary` — where the money went

Total spend, broken down by model, with input/output/cache token splits. The cache-read line is usually the headline.

```
$ burn summary --since 7d

turns analyzed: 6,798

model              turns  input      output     reasoning  cacheRead    cacheCreate  cost
claude-opus-4-7    6,725  19,466     4,104,867  0          929,310,844  21,630,067   $702.56
gpt-5.4               18  1,726,955  163,717    93,726     24,859,008   0            $12.99
claude-sonnet-4-6     39  796        12,061     0          1,177,505    70,772       $0.80

total cost: $716.35
  input $4.42 / output $105.26 / cacheRead $471.22 / cacheCreate $135.45
```

`--quality` adds outcome counts (completed / abandoned / errored) and the one-shot rate across edit turns. `--by-provider` groups Synthetic-routed (`hf:*`, `accounts/fireworks/models/*`, `synthetic/*`) traffic without rewriting ledger rows. `--subagent-tree <id>` and `--by-subagent-type` inspect spawned subagents.

Filters compose: `--since`, `--project`, `--session`, `--workflow`, `--agent`. They apply to every query command, not just summary.

### `burn by-tool` — which tool calls cost the most

Attributes session cost down to individual tool names.

```
$ burn by-tool --since 7d

tool          calls  attributedCost
Bash          3,614  $256.39
Edit          1,248  $109.57
Read          1,229  $87.39
TaskUpdate      540  $27.03
Write           303  $19.78
ToolSearch       70  $10.08
TaskCreate      295  $9.82
Grep            105  $5.45
Agent            76  $3.28
```

The mix is itself a signal — heavy `Read` cost relative to `Edit` usually means the agent is re-reading files instead of relying on prompt cache.

### `burn waste` — what's riding in every turn

`waste` answers "*which file did I add to context that I'm now paying for in every subsequent turn's cacheRead?*" Same for Bash commands and subagent calls.

```
$ burn waste --since 7d

session grand total: $698.10
attributed to tool calls: $116.47  /  unattributed: $581.63

Top files by cumulative cost
path                                          firstTurn  initial(tok)  persist(tok)   rideTurns  cost   %attr
packages/reader/src/claude.ts                 18         54,599        7,854,085      8,729      $4.27  3.7%
packages/cli/src/cli.ts                       22         46,238        4,718,510      6,950      $2.65  2.3%
.github/workflows/publish.yml                 138        14,386        4,177,237      7,248      $2.18  1.9%
packages/reader/src/codex.ts                  18         31,987        3,668,540      4,384      $2.03  1.7%

Top Bash commands by cost
command                                       calls  initial(tok)  persist(tok)  cost
gh run view 24899... --log-failed             1      209           1,464,340     $0.733
cat .github/workflows/publish.yml             1      908           1,059,723     $0.536
gh issue view 770 --json title,body,...       1      3,833         916,087       $0.482

Top subagent calls by cost
subagent         calls  initial(tok)  persist(tok)  cost
Explore          24     40,983        2,196,617     $1.35
general-purpose  50     12,502        337,887       $0.247
```

`initial` is the size of the read; `persist` is total tokens that file/output rode through every later turn's prompt cache. A 50k-token Read that rides 9000 turns is a $4 line item even though you never re-read it.

#### `burn waste --patterns` — retry loops, failures, compaction loss, edit reverts

The same command with `--patterns` surfaces specific waste shapes:

```
$ burn waste --patterns --since 7d

sessions with patterns: 9  /  total pattern cost: $4.40

Retry loops (≥3 identical failing tool calls in a row)
  (none)

Consecutive tool-failure runs (≥3 distinct tools failing in sequence)
session   length  turns  tools  cost
ee4aa256  3       73–73  Bash   $0.154
df324ec7  4       8–9    Bash   $0.134

Edit-revert cycles (file returned to a prior state)
session   file                              firstEdit  revert  span  cost
89583818  packages/sdk/...                  299        312     13    $0.493
89583818  package.json                      298        328     30    $0.488
89583818  scripts/validate.ts               238        250     12    $0.429
```

Pick the patterns you care about: `--patterns=retries,failures,compaction,reverts` (any subset).

### `burn compare` — model comparison by observed activity

Bucket turns by `(model, activity)` and show cost-per-turn, one-shot rate, and turn count side-by-side. This is observed data, not counterfactual — it tells you what happened when you actually used both models, scoped to like-for-like work.

```
$ burn compare --since 30d --models claude-opus-4-7,claude-opus-4-6

               claude-opus-4-7           claude-opus-4-6
Activity       Turns  Cost/turn  1-shot  Turns  Cost/turn  1-shot
exploration    3,872  $0.094     —       691    $0.130     —
coding         1,313  $0.112     100%    211    $0.163     100%
debugging        677  $0.112     100%     33    $0.048     100%
feature          310  $0.136     100%     23    $0.045     100%
refactoring      118  $0.144     100%      3    $0.035     —
docs             197  $0.134     100%     15    $0.070     100%

claude-opus-4-7: 8,376 turns, $873.25 total
claude-opus-4-6: 1,151 turns, $146.35 total
```

One-shot rate = `turns with edits and zero intra-turn retries / edit turns`. Cells render `—` when the activity doesn't produce edits or sample size is below `--min-sample` (default 5). `--json` and `--csv` are mutually exclusive output modes.

### `burn diagnose` — single-session deep dive

Everything `summary`, `waste`, and `waste --patterns` know about one session, in one view.

```
$ burn diagnose 55b39623-be9d-44bc-953f-1b9a7e9240db

session: 55b39623-be9d-44bc-953f-1b9a7e9240db
turns: 39
cost: $2.22 (attributed $0.469, unattributed $1.75)
patterns: none detected

Retry loops          (none)
Consecutive failures (none)
Compaction events    (none)
Edit-revert cycles   (none)

Top files by cost
path                                          calls  cost
packages/cli/src/cli.ts                       1      $0.208
packages/sdk/src/workflow.ts                  3      $0.030
README.md                                     1      $0.024
```

Use this when a session looks unusually expensive — it tells you which files persisted, which Bash commands lingered, and whether any waste pattern fired.

### `burn context` — what your CLAUDE.md / AGENTS.md is costing

Every project context file (CLAUDE.md, AGENTS.md, opencode rules, `.cursorrules`, etc.) ships in every turn. `burn context` measures what that costs, broken down by section.

```
$ burn context --since 30d

Context files in /path/to/repo:

AGENTS.md (AGENTS.md) — 82 lines, ~1.5k tokens — applies to: codex, opencode
  Cost per session:   avg $0.0004, p95 $0.0026
  Cost over 30d: $0.0038 across 10 sessions
  Sections ranked by cost:
    lines      heading             tokens  cost/session  %file
      34-  56  ## Changelog        705     $0.0002       46.8%
      77-  82  ## When in doubt    230     $0.0001       15.2%
      57-  76  ## Releases         215     $0.0001       14.2%
       6-  19  ## Layout           180     $0.0000       11.9%
```

`burn context advise --top 3` goes one step further — it produces unified diffs you could apply to trim the highest-cost sections, with projected savings:

```
$ burn context advise --top 3

# TRIM: ## Changelog
# projected savings per session: $0.0002
# projected savings across window: $0.0018
--- a/AGENTS.md
+++ b/AGENTS.md
@@ -34,23 +34,0 @@
-## Changelog
-...
```

Burn never modifies your context files — it only recommends.

### `burn limits` — quota window forecast

For Claude/Codex/OpenCode plans with rolling token windows, `burn limits` forecasts when you'll hit them. `--watch` re-renders on a timer; `--no-api` keeps everything local; `--json` is for scripting.

```
$ burn limits --no-api
Claude
  Forecast (5-hour window, local ledger):
    burn rate 37.2k tok/min
```

### `burn plans` — quota plan tracking

If you pay for a plan with hard limits, `burn plans add` registers it and turns spend tracking into quota tracking.

```
$ burn plans
No plans configured. Add one with `burn plans add --provider claude --preset pro`.

$ burn plans add --provider claude --preset max
$ burn plans set-reset-day claude-max 15
$ burn plans                          # list cycles + per-cycle confidence
```

Cycle totals annotate fidelity: low-confidence rows surface a footer note when some turns lack per-turn token data, so you know a number is a lower bound rather than the truth.

### `burn watch` — live ingest while an agent runs

Tails active session logs on an interval and ingests new turns as they appear, so subsequent `summary` / `waste` queries reflect the in-flight session without re-running the wrapper.

### `burn mcp-server` — query burn from inside an agent

A stdio MCP server. Wire it into Claude Code or another MCP-aware harness and the agent itself can call `burn summary` / `burn waste` on its own session — useful for self-aware cost decisions ("which files have I made expensive?") without leaving the chat.

### Maintenance commands

| Command | Purpose |
|---|---|
| `burn ingest --runtime claude --quiet` | Reads a Claude Code hook payload on stdin and folds it into the ledger. Idempotent — safe to fire on every hook event. |
| `burn archive build / rebuild / status` | Manage `~/.relayburn/archive.sqlite`, the disposable read model that backs fast queries. `rm archive.sqlite && burn archive rebuild` always reproduces from the canonical JSONL. |
| `burn rebuild --index` | Rebuild the dedup/cursor index after a manual ledger edit. |
| `burn rebuild --reclassify [--force]` | Re-run the activity classifier across the ledger. Default is non-destructive (only fills missing labels); `--force` overwrites. |
| `burn content prune [--days <n>] [--force]` | Trim the content sidecar. Source-aware: sidecars whose upstream session log still exists are kept (rederivable) unless `--force`. |

## Spawning agents

The primitive is **stamping**: attach metadata to a session by ID, before or after any turns have been recorded.

```ts
import { stamp } from '@relayburn/ledger';

await stamp(
  { sessionId: 'some-session-uuid' },
  {
    workflowId: 'wf-refactor-auth',
    agentId: 'ag-42',
    persona: 'senior-eng',
    tier: 'best',
  }
);
```

Stamp selectors:

- `{ sessionId }` — all turns in a session.
- `{ messageId }` — exactly one turn.
- `{ sessionId, range: { fromTs, toTs } }` — scoped to a time window.

Enrichment values are plain strings (`Record<string, string>`). Last-write-wins per key. Burn doesn't care what keys you use.

### Wrappers (the easy path)

```bash
burn claude    [--tag k=v ...] [-- <claude args>]
burn codex     [--tag k=v ...] [-- <codex args>]
burn opencode  [--tag k=v ...] [-- <opencode args>]
```

The wrapper pre-assigns a session UUID, applies any `--tag k=v` pairs as stamps, passes `--session-id` to the agent, and ingests the session into the ledger when the agent exits.

### Spawner env-var contract

For orchestrators that spawn many sessions, threading `--tag` through every wrapper is awkward. All three wrappers also read a fixed set of `RELAYBURN_*` env vars:

| Env var                       | Stamp key       |
|-------------------------------|-----------------|
| `RELAYBURN_WORKFLOW_ID`       | `workflowId`    |
| `RELAYBURN_STEP_ID`           | `stepId`        |
| `RELAYBURN_AGENT_ID`          | `agentId`       |
| `RELAYBURN_PARENT_AGENT_ID`   | `parentAgentId` |
| `RELAYBURN_PERSONA`           | `persona`       |
| `RELAYBURN_TIER`              | `tier`          |

`--tag k=v` flags win on key collision. The merged values are re-exported on the child harness's environment so a transitive `burn …` invocation inside the child session inherits the same context without re-threading.

```bash
export RELAYBURN_WORKFLOW_ID=wf-refactor-auth
export RELAYBURN_AGENT_ID=ag-42
burn codex                                         # stamps workflowId, agentId
burn opencode --tag agentId=ag-43                  # --tag wins; workflowId still inherited
```

`RELAYBURN_HOME`, `RELAYBURN_SESSION_ID`, `RELAYBURN_CONTENT_STORE`, `RELAYBURN_CONTENT_TTL_DAYS` are burn internals, not stamp tags.

### Hook-based ingest for orchestrators

If your code already controls the Claude Code spawn, install burn's hooks per-invocation via `--settings` — no global `~/.claude/settings.json` mutation.

```ts
import { buildClaudeHookSettings, stamp } from '@relayburn/ledger';

const { sessionId, settings } = buildClaudeHookSettings();

await stamp(
  { sessionId },
  { workflowId: 'wf-refactor-auth', agentId: 'ag-42', persona: 'senior-eng' },
);

spawn('claude', [
  '--session-id', sessionId,
  '--settings', settings,
  ...existingArgs,
]);
```

`buildClaudeHookSettings({ burnBin? })` returns a fresh UUID and a JSON string wiring every Claude Code hook event (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Notification`, `Stop`, `SubagentStop`, `SessionEnd`) to `burn ingest --runtime claude --quiet`. The command is safe to re-fire on every hook — the ledger's cursor and dedup index keep ingestion idempotent.

## Data model

One `TurnRecord` per distinct `message.id` in the session log. Cost is never stored, only `usage` — so pricing corrections don't require a migration.

```ts
interface TurnRecord {
  v: 1;
  source: 'claude-code' | 'codex' | 'opencode' | 'anthropic-api' | 'openai-api' | 'gemini-api';
  sessionId: string;
  messageId: string;
  turnIndex: number;
  ts: string;
  model: string;
  project?: string;
  usage: {
    input: number;
    output: number;
    cacheRead: number;
    cacheCreate5m: number;
    cacheCreate1h: number;
  };
  toolCalls: ToolCall[];
  filesTouched?: string[];
  subagent?: { isSidechain: boolean };
  activity?: ActivityCategory;  // what kind of work this turn did
  retries?: number;              // edit→bash→edit cycles within the turn
  hasEdits?: boolean;            // at least one Edit/Write/NotebookEdit call
}
```

### Content sidecar

A content sidecar (enabled by default) stores the full conversation separately from the ledger:

- User prompts and assistant responses.
- Tool inputs and outputs, verbatim.
- Lives at `~/.relayburn/content/<sessionId>.jsonl` — separate from the main ledger so aggregate queries stay fast.
- Currently captured for Claude Code sessions only. Codex and OpenCode readers flow through the same `content.store` modes but do not yet populate content records (tracked as a follow-up).

Content meaningfully strengthens attribution and diagnostic paths — tool-call sizing becomes exact (no delta estimation), outcome inference gets a real signal, CLAUDE.md adherence checking becomes possible, and waste patterns can surface the specific error text that caused a retry loop rather than just a count.

Three content modes:

- `content.store=full` (default) — everything above.
- `content.store=hash-only` — usage + hashes + metadata, no prompt/response content. Restores burn's minimal-storage behavior for sensitive environments.
- `content.store=off` — skip the sidecar entirely; no content directory.

Set via `RELAYBURN_CONTENT_STORE=<mode>` or the config file. Retention defaults to 90 days for the sidecar (configurable via `RELAYBURN_CONTENT_TTL_DAYS`), forever for the main ledger. Prune is source-aware: a sidecar whose upstream session file still exists is left in place because `burn rebuild --content` can rederive it. Run `burn content prune --force` (or `RELAYBURN_PRUNE_FORCE=1`) to delete recoverable sidecars anyway.

Content lives on your machine. Burn makes no outbound requests beyond optional pricing updates. If the device you run on is sensitive to conversation leak (shared dev machines, cloud-synced home directories, compliance contexts), switch to `hash-only`.

## Activity classification

Every turn is tagged with an `activity` label so cost can be compared like-for-like. "Sonnet cost more than Haiku last month" isn't useful on its own; "Sonnet and Haiku both attempted 43 refactoring turns, Sonnet landed them first-try 75% of the time vs. Haiku's 60%" is. That's what per-turn activity enables.

Classification is deterministic and rule-based — no LLM in the loop. Eighteen categories, chosen so cross-tool comparison stays possible:

| Category | Trigger |
|---|---|
| `planning` | `ExitPlanMode` tool, or planning/roadmap keywords with no tool use |
| `delegation` | `Agent` / `Task` spawn — dominates other signals |
| `testing` | `Bash` matching `pytest`, `vitest`, `bun test`, `jest`, `go test`, `cargo test`, `npm test`, `playwright`, `cypress`, etc. |
| `review` | Read-only inspection: `git status/diff/show/log/blame`, `gh pr diff/view/checks`, or explicit review/audit keywords |
| `git` | `Bash` matching `git push/pull/commit/merge/rebase/checkout/cherry-pick/...` |
| `deps` | `Bash` matching `npm install`, `pnpm add`, `pip install`, `uv add`, `cargo add`, `go get`, `brew install`, etc. |
| `format` | `Bash` matching mutating formatter commands (`prettier --write`, `eslint --fix`, `black`, `ruff format`, `cargo fmt`, `gofmt`) |
| `verification` | `Bash` matching lint/typecheck/static analysis (`eslint`, `ruff check`, `cargo check`, `tsc --noEmit`, `prettier --check`) |
| `build-deploy` | `Bash` matching `docker build`, `cargo build`, `npm run build`, `kubectl apply`, `terraform apply`, etc. |
| `coding` | `Edit` / `Write` / `NotebookEdit` with no stronger keyword signal |
| `docs` | Edit turn where every edited file is a doc (`*.md`, `*.rst`, `README*`, anything under `docs/`) |
| `debugging` | Edit turn where prompt mentions bug/error/crash/traceback, or any tool call errored, or ≥2 edit→bash→edit retry cycles |
| `refactoring` | Edit turn with keywords: `refactor`, `cleanup`, `rename`, `extract`, `restructure` |
| `feature` | Edit turn with keywords: `add`, `create`, `implement`, `new`, `introduce` |
| `exploration` | `Read` / `Grep` / `Glob` / `WebFetch` / `WebSearch` without edits |
| `reasoning` | No tool use, no keyword hit, but the turn billed reasoning tokens |
| `brainstorming` | No tool use; prompt asks *what if*, *think through*, *should we*, *design* |
| `conversation` | No tool use, no category keywords, no reasoning tokens — the fallback |

Two companion fields fall out of the same pass:

- `hasEdits` — true when a turn called any file-mutating tool. Lets `coding`/`refactoring`/`feature`/`debugging` share a cross-cutting filter.
- `retries` — count of edit→bash→edit cycles within a single turn. Non-zero values surface reactive edits without waiting for whole-session retry analysis.

`oneShotRate = oneShotTurns / editTurns` is computable directly on the ledger (a one-shot turn has `hasEdits && retries === 0`) and is the secondary quality signal feeding `burn compare`.

If you upgrade burn or a classifier rule changes, run `burn rebuild --reclassify` to re-run the classifier across already-ingested turns. Default is non-destructive; `--force` overwrites every label.

## Packages

| Package | Purpose |
|---|---|
| `@relayburn/reader` | Pure parsers: session log → `TurnRecord[]`. No I/O writes, no state. |
| `@relayburn/ledger` | Append-only JSONL ledger at `~/.relayburn/ledger.jsonl`. Exposes `appendTurns`, `stamp`, `query`. |
| `@relayburn/analyze` | Pricing loader (models.dev) + per-record cost derivation. |
| `@relayburn/mcp` | Stdio MCP server (`burn mcp-server`) for in-session self-query. |
| `@relayburn/cli` | `burn` binary: spawn wrappers + read/report commands. |

Override ledger location with `RELAYBURN_HOME=/path/to/dir`.

### Reasoning-token pricing semantics

`usage.reasoning` on a `TurnRecord` is always preserved for observability, but how it's billed depends on the source and model:

- **Codex (`source: 'codex'`)** — `output_tokens` already includes reasoning. Burn does **not** double-bill reasoning on top of output. `usage.reasoning` is informational only. (Matches `ccusage`'s Codex semantics.)
- **Models with a distinct `cost.reasoning` tariff in `models.dev`** — billed at that tariff (e.g. Alibaba Qwen reasoning models). The flattened `ModelCost` carries `reasoning` and `reasoningMode: 'separate'`.
- **Everything else (Anthropic Claude, default)** — billed at the model's `output` rate. `reasoningMode: 'same_as_output'`.

Override per-call via `costForUsage(usage, model, pricing, { reasoningMode })`.

## Pricing

Ships with a vendored snapshot of [models.dev](https://models.dev). Refresh with:

```bash
npm run pricing:update
```

User overrides at `$RELAYBURN_HOME/models.dev.json` (or `~/.relayburn/models.dev.json`) take precedence over the vendored snapshot at lookup time.

## What burn is not

- Not a dashboard or a product with a UI of its own.
- Not an automatic optimizer — it surfaces the choices; you decide.
- Not a leaderboard or a social service.

## Status

v0: Claude Code reader shipped. OpenCode reader in progress. Codex reader scaffolded. Per-tool-call attribution (`burn waste`), CLAUDE.md hot-path analysis (`burn context`), quota-window tracking (`burn limits`), waste-pattern detection (`burn waste --patterns`), single-session deep dive (`burn diagnose`), and model comparison (`burn compare`) all shipped. Subagent-tree queries and additional adapter content capture are scoped and tracked as open issues.
