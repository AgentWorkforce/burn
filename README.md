![relayburn](./burn-readme-banner.png)

# relayburn

> Know where every token went — and why. An attribution layer for AI agent spend.

Part of the `relay*` family of single-concern primitives alongside `relayfile`, `relaycast`, `relayauth`.

## What this is for

Agent spend happens in a blind spot. You can see a daily dollar total, and maybe a breakdown by model. You cannot see *which tool call, which file, which subagent, or which workflow step* drove the cost. That is the question burn exists to make answerable.

The deeper question burn is built around is:

> **Would the same work cost less with a different model, harness, or tool choice — in dollars or quota consumption?**

You cannot answer that from aggregate spend. It requires attribution at the level of the actual work: *this Read cost $0.47 because it added 8,200 tokens to context that rode in every one of the next 23 turns' cache-reads.* Once spend is visible at that grain, the choice between Opus and Haiku, between Claude Code and another harness, or between letting an agent re-read a file and passing it a cached summary, becomes a decision you can reason about — not a guess.

Three concrete sub-questions follow from the meta-goal:

1. **How much did I spend?** — per agent, per workflow, per session, per model, per tool call.
2. **Why was it spent?** — which tool calls, which files, which subagents, cache-hit vs fresh input vs persistence.
3. **Where can I save?** — redundant re-reads, bloated context prefixes, retry loops, model choices that cost more than they needed to.

Burn is local-first. Data lives in an append-only JSONL ledger on your machine. Burn never phones home. Pricing is looked up at query time from a vendored snapshot, so rate corrections never require rewriting the ledger.

## What burn is not

- Not a dashboard or a product with a UI of its own.
- Not an automatic optimizer — it surfaces the choices; you decide.
- Not a leaderboard or a social service.

## Composability: how burn plugs into a spawner

Burn is designed to be called by whatever code spawns agent sessions. If you control the spawn, you know things the session log never records on its own — the workflow this session is part of, the persona it's running as, the agent ID, the tier. Burn's job is to accept that context, attach it to the session, and make it queryable later alongside the usage data that came from the session log.

The primitive is **stamping**: attach metadata to a session by ID, before or after any turns have been recorded.

```ts
import { stamp } from '@relayburn/ledger';

// Your spawner knows metadata the session log doesn't carry.
// Stamp it against the session ID before (or after) spawn.
await stamp(
  { sessionId: 'some-session-uuid' },
  {
    workflowId: 'wf-refactor-auth',
    agentId: 'ag-42',
    persona: 'senior-eng',
    tier: 'best',
  }
);

// Later, every turn with this sessionId inherits the enrichment at query time.
// Stamps can arrive before the first turn or after the last —
// last-write-wins per key.
```

Stamp selectors:

- `{ sessionId }` — all turns in a session.
- `{ messageId }` — exactly one turn.
- `{ sessionId, range: { fromTs, toTs } }` — scoped to a time window (e.g. a single workflow step within a long-lived session).

Enrichment values are plain strings (`Record<string, string>`). Burn doesn't care what keys you use. Typical: `agentId`, `parentAgentId`, `workflowId`, `stepId`, `persona`, `tier`, `harness`, `userLabel`.

This is the composability surface. Burn stays small; the spawner owns the context and decides what to attach.

### Spawner-integrated ingest

All three supported harnesses (Claude, Codex, OpenCode) ship under one verb:

```
burn run <claude|codex|opencode> [--tag k=v ...] [-- <harness args>]
```

For Claude specifically, the adapter generates a session UUID up-front so metadata can be stamped before the agent starts. It passes `--session-id` to Claude, applies any `--tag k=v` pairs as stamps, and ingests the session into the ledger when Claude exits. If you are building an orchestrator, the same pattern applies: generate the UUID, stamp first, spawn with the UUID, let burn pick up the session log on ingest.

```
burn run claude --tag workflow=refactor --tag persona=senior-eng -- --resume abc
```

Codex and OpenCode do not expose Claude-style hooks or a pre-spawn session ID. Their adapters write a v1 pending-stamp manifest under `$RELAYBURN_HOME/pending-stamps/` before spawning, then resolve it against the first matching session file before the first turn is appended. They also run burn's foreground watch loop for the child process lifetime, so long sessions become visible incrementally instead of only after exit. Abandoned pending manifests are cleaned up after 24 hours.

For passive ingest without a wrapper, run:

```
burn watch [--interval <ms>] [--opencode-stream] [--opencode-url <url>]
```

`burn watch` scans Claude, Codex, and OpenCode stores in the foreground and uses the same cursor + dedup path as the reporting commands. `--opencode-stream` also subscribes to OpenCode's local SSE endpoint (`http://127.0.0.1:4096/event` by default, or `OPENCODE_SERVER_URL`) and ingests sessions observed from creation directly from the stream at completed tool-call grain; polling remains enabled as the fallback for already-running sessions, missed events, and drift checks. If the server uses Basic auth, `OPENCODE_SERVER_USERNAME` / `OPENCODE_SERVER_PASSWORD` are forwarded. `burn watch --daemon` is reserved but not implemented yet.

### Spawner env-var contract (workflow / agent attribution)

For orchestrators that spawn many agent sessions, threading `--tag` through every wrapper invocation is awkward. All three adapters also read a fixed set of `RELAYBURN_*` env vars and fold them into the stamp bag:

| Env var                       | Stamp key       |
|-------------------------------|-----------------|
| `RELAYBURN_WORKFLOW_ID`       | `workflowId`    |
| `RELAYBURN_STEP_ID`           | `stepId`        |
| `RELAYBURN_AGENT_ID`          | `agentId`       |
| `RELAYBURN_PARENT_AGENT_ID`   | `parentAgentId` |
| `RELAYBURN_PERSONA`           | `persona`       |
| `RELAYBURN_TIER`              | `tier`          |

`--tag k=v` flags win on key collision (explicit beats implicit). The merged values are re-exported on the child harness's environment under their canonical names, so a transitive `burn …` invocation inside the child session inherits the same context without the orchestrator having to re-thread it. This gives Codex and OpenCode the same orchestrator-level attribution that Claude already had via stamps, independent of whether the harness reports `isSidechain` / `parentID` natively.

```bash
export RELAYBURN_WORKFLOW_ID=wf-refactor-auth
export RELAYBURN_AGENT_ID=ag-42
burn run codex   # workflowId=wf-refactor-auth, agentId=ag-42 stamped
burn run opencode --tag agentId=ag-43   # --tag wins → agentId=ag-43, workflowId still inherited
```

Other `RELAYBURN_*` variables (`RELAYBURN_HOME`, `RELAYBURN_SESSION_ID`, `RELAYBURN_CONTENT_STORE`, `RELAYBURN_CONTENT_TTL_DAYS`) are burn internals and are **not** treated as stamp tags.

### Hook-based ingest for orchestrators

If your code already controls the Claude Code spawn, you can install burn's hooks per-invocation via Claude's `--settings` flag — no global `~/.claude/settings.json` mutation needed. Hook payloads land on stdin and get forwarded to `burn ingest`, which incrementally parses the transcript with the same cursor+dedup machinery as `burn run claude`.

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

`buildClaudeHookSettings({ burnBin? })` returns a fresh UUID and a JSON string wiring every Claude Code hook event (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Notification`, `Stop`, `SubagentStop`, `SessionEnd`) to `burn ingest --runtime claude --quiet`. The command is safe to re-fire on every hook — the ledger's cursor and dedup index keep ingestion idempotent, so the hook path and the JSONL-reader path are reconcilable against the same session. Tool-call failures ride in the normal `PostToolUse` payload (surfaced as `ToolCall.isError` on the resulting `TurnRecord`), not a distinct `PostToolUseFailure` event.

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

## What burn records

The main ledger stores usage and metadata:

- Token counts, timestamps, model identifiers.
- Tool names, tool-call argument hashes, file paths touched.
- Session IDs, message IDs, and whatever metadata you stamp against them.

A **content sidecar** (enabled by default) stores the full conversation separately:

- User prompts and assistant responses.
- Tool inputs and outputs, verbatim.
- Lives at `~/.relayburn/content/<sessionId>.jsonl` — separate from the main ledger so aggregate queries stay fast.
- **Currently captured for Claude Code sessions only.** Codex and OpenCode readers flow through the same `content.store` modes but do not yet populate content records; this is tracked as a follow-up.

Content is stored because it meaningfully strengthens several attribution and diagnostic paths — tool-call sizing becomes exact (no delta estimation), outcome inference gets a real signal, CLAUDE.md adherence checking becomes possible, and waste patterns can surface the specific error text that caused a retry loop rather than just a count.

Retention defaults to 90 days for the sidecar, forever for the main ledger. Configure via `RELAYBURN_CONTENT_TTL_DAYS`. Prune is **source-aware**: a sidecar whose upstream session file (`~/.claude/projects/…`, `~/.codex/sessions/…`, `~/.local/share/opencode/storage/…`) still exists is left in place, because `burn rebuild --content` can rederive it. Run `burn content prune --force` (or set `RELAYBURN_PRUNE_FORCE=1`) to delete recoverable sidecars anyway.

Three content modes:

- `content.store=full` (default) — everything above.
- `content.store=hash-only` — usage + hashes + metadata, no prompt/response content. Restores burn's minimal-storage behavior for sensitive environments.
- `content.store=off` — skip the sidecar entirely; no content directory.

Set via `RELAYBURN_CONTENT_STORE=<mode>` or the config file.

Content lives on your machine. Burn makes no outbound requests beyond optional pricing updates. If the device you run on is sensitive to conversation leak (shared dev machines, cloud-synced home directories, compliance contexts), switch to `hash-only`.

## Activity classification

Every turn is tagged with an `activity` label so cost can be compared like-for-like. "Sonnet cost more than Haiku last month" isn't useful on its own; "Sonnet and Haiku both attempted 43 refactoring turns, Sonnet landed them first-try 75% of the time vs. Haiku's 60%" is. That's what per-turn activity enables.

Classification is deterministic and rule-based — no LLM in the loop. Every turn with the same tool calls and prompt text produces the same label.

Eighteen categories, chosen so cross-tool comparison stays possible:

| Category | Trigger |
|---|---|
| `planning` | `ExitPlanMode` tool, or planning/roadmap keywords with no tool use |
| `delegation` | `Agent` / `Task` spawn — dominates other signals |
| `testing` | `Bash` matching `pytest`, `vitest`, `bun test`, `jest`, `go test`, `cargo test`, `npm test`, `playwright`, `cypress`, `puppeteer`, etc. |
| `review` | Read-only inspection work: `git status/diff/show/log/blame`, `gh pr diff/view/checks`, or explicit review/audit keywords |
| `git` | `Bash` matching `git push/pull/commit/merge/rebase/checkout/cherry-pick/...` |
| `deps` | `Bash` matching `npm install`, `pnpm add`, `pip install`, `uv add`, `cargo add`, `go get`, `brew install`, etc. |
| `format` | `Bash` matching mutating formatter commands such as `prettier --write`, `eslint --fix`, `black`, `ruff format`, `cargo fmt`, `gofmt`, etc. |
| `verification` | `Bash` matching lint/typecheck/static-analysis commands: `npm run lint`, `eslint`, `ruff check`, `cargo check`, `tsc --noEmit`, `prettier --check`, etc. |
| `build-deploy` | `Bash` matching `docker build`, `cargo build`, `npm run build`, `kubectl apply`, `terraform apply`, etc. |
| `coding` | `Edit` / `Write` / `NotebookEdit` with no stronger keyword signal |
| `docs` | Edit turn where **every** edited file is a doc (`*.md`, `*.mdx`, `*.rst`, `*.adoc`, `*.txt`, `README*`, `CHANGELOG*`, anything under `docs/`) |
| `debugging` | Edit turn where prompt mentions bug/error/crash/traceback, or any tool call errored this turn, or the turn contains ≥2 edit→bash→edit retry cycles |
| `refactoring` | Edit turn with keywords: `refactor`, `cleanup`, `rename`, `extract`, `restructure` |
| `feature` | Edit turn with keywords: `add`, `create`, `implement`, `new`, `introduce` |
| `exploration` | `Read` / `Grep` / `Glob` / `WebFetch` / `WebSearch` without edits |
| `reasoning` | No tool use, no keyword hit, but the turn billed reasoning tokens (extended thinking, Codex `reasoning_output_tokens`) |
| `brainstorming` | No tool use; prompt asks *what if*, *think through*, *should we*, *design* |
| `conversation` | No tool use, no category keywords, no reasoning tokens — the fallback |

Keyword refinement can also promote non-edit turns out of `exploration` when the ask is explicit, especially for `review`, `debugging`, `refactoring`, and `feature`. Doc-only edit turns stay `docs` unless the turn actually hit a failure signal.

Two companion fields fall out of the same pass:

- `hasEdits` — true when a turn called any file-mutating tool. Lets `coding`/`refactoring`/`feature`/`debugging` share a cross-cutting filter.
- `retries` — count of edit→bash→edit cycles *within a single turn*. Non-zero values surface reactive edits (wrote, tested, fixed) without waiting for whole-session retry analysis. Cross-turn retry loops are tracked separately.

Together these make `oneShotRate = oneShotTurns / editTurns` computable directly on the ledger (a "one-shot turn" has `hasEdits && retries === 0`), which is the secondary quality signal feeding model comparison.

## Packages

| Package | Purpose |
|---|---|
| `@relayburn/reader` | Pure parsers: session log → `TurnRecord[]`. No I/O writes, no state. |
| `@relayburn/ledger` | Append-only JSONL ledger at `~/.relayburn/ledger.jsonl`. Exposes `appendTurns`, `stamp`, `query`. |
| `@relayburn/analyze` | Pricing loader (models.dev) + per-record cost derivation. |
| `@relayburn/cli` | `burn` binary: spawn wrapper + read/report commands. |

Override ledger location with `RELAYBURN_HOME=/path/to/dir`.

### Reasoning-token pricing semantics

`usage.reasoning` on a `TurnRecord` is always preserved for observability, but how it's billed depends on the source and model:

- **Codex (`source: 'codex'`)** — `output_tokens` already includes reasoning. `burn` does **not** double-bill reasoning on top of output. `usage.reasoning` is informational only. (Matches `ccusage`'s Codex semantics.)
- **Models with a distinct `cost.reasoning` tariff in `models.dev`** — billed at that tariff (e.g. Alibaba Qwen reasoning models). The flattened `ModelCost` carries `reasoning` and `reasoningMode: 'separate'`.
- **Everything else (Anthropic Claude, default)** — billed at the model's `output` rate. `reasoningMode: 'same_as_output'`.

You can override per-call via `costForUsage(usage, model, pricing, { reasoningMode })`.

## CLI

```
burn summary [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--provider <p>]
             [--by-provider | --by-tool | --by-subagent-type | --by-relationship[=subagent] | --subagent-tree <session-id>]
burn waste [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--provider <p>]
burn compare <model_a,model_b[,...]> [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--min-sample <n>] [--fidelity <class>] [--include-partial] [--json|--csv]
burn run <claude|codex|opencode>  [--tag k=v ...] [-- <harness args>]
burn watch   [--interval <ms>] [--once]
```

Provider filters are applied at query time; raw ledger model strings are not rewritten. `burn summary --by-provider --provider synthetic` groups Synthetic-routed turns under provider `synthetic` while pricing against the normalized model id. Recognized Synthetic model patterns are `hf:*`, `accounts/fireworks/models/*`, and `synthetic/*`.

### `burn compare` — model comparison by observed activity

Looking at work you actually did, which model handled each activity category best?
`burn compare` buckets every turn by `(model, activity)` and shows cost-per-turn, one-shot rate, and turn count side-by-side. The model list is a required comma-separated positional argument naming at least two models — use `burn summary --by-provider` (or `--by-tool`) to discover which models have data in your ledger.

```
$ burn compare claude-sonnet-4-6,claude-haiku-4-5 --since 30d

              claude-sonnet-4-6           claude-haiku-4-5
Activity      Turns  Cost/turn  1-shot   Turns  Cost/turn  1-shot
coding          243    $0.020    68%        89   $0.004    51%
debugging       108    $0.031    41%        34   $0.008    28%
refactoring      71    $0.024    75%        14   $0.006    64%
testing          42    $0.012    89%        18   $0.003    83%
exploration     118    $0.013     —         52   $0.003     —
```

One-shot rate = `turns with edits and zero intra-turn retries / edit turns`. It's `—` for categories that don't produce edits (`exploration`, `brainstorming`, etc.). Missing-data cells render as `—`, never `$0.00` or `0%`.

This is observed data, not counterfactual: it tells you what happened when you actually used both models, not what *would have* happened if you'd picked differently. Cells with `turns < --min-sample` (default 5) are flagged as indicative; categories where only one model has data surface a coverage note beneath the table. The JSON cell shape exposes both `noData` (we never saw this combination) and `insufficientSample` (we have data but not much) so consumers can tell them apart cleanly.

Standard filters apply: `--session <id>` limits to a single session, `--agent <id>` limits to a stamped agent ID, `--workflow <id>` to a stamped workflow ID, `--project <path>` to a project path or git-canonical projectKey.

By default, `burn compare` only aggregates turns with `usage-only` fidelity or better — `aggregate-only`, `cost-only`, and `partial` turns are excluded so a session with mixed fidelity can't silently bias the cost/turn or one-shot rate of full-fidelity peers from the same model. When the gate dropped anything, the table prints an `excluded N turns below <class> fidelity (… aggregate-only, … cost-only, … partial)` coverage note. Override the floor with `--fidelity full | usage-only | aggregate-only | cost-only | partial`; `--include-partial` is shorthand for `--fidelity partial` and includes every turn. Records emitted before `TurnRecord.fidelity` existed always pass for backward compatibility.

Output formats: TTY table (default), `--json` for scripts, `--csv` for spreadsheets. `--json` and `--csv` are mutually exclusive. The `--json` payload includes a `fidelity` block (`{ minimum, excluded, summary }`) computed against the unfiltered slice so consumers can render their own coverage UI.

### `burn rebuild --reclassify` — backfill activity labels on old turns

Ingested turns are classified at write time. If you upgrade burn or a classifier rule changes, already-ingested turns keep the label they had when they were written. Run `burn rebuild --reclassify` to re-run the classifier across the whole ledger using whatever signals are still available (tool calls from the ledger, user prompts and errored tool_results from the content sidecar when present).

```
burn rebuild --reclassify           # only fills in turns with no activity set
burn rebuild --reclassify --force   # overwrite every turn's activity
```

Default is non-destructive — turns that already have an activity stay as-is, so re-running is safe. `--force` is useful after a rule change when you want the whole ledger to reflect the new rules. The ledger is rewritten atomically under the same lock that `ingest` uses.

## Install (local dev)

```bash
git clone <repo> && cd burn
npm install
npm run build
node packages/cli/dist/cli.js summary --since 24h
```

Published npm flow (pending publish):

```bash
npx @relayburn/cli summary --since 24h
```

## Pricing

Ships with a vendored snapshot of [models.dev](https://models.dev). Refresh with:

```bash
npm run pricing:update
```

User overrides go at `$RELAYBURN_HOME/models.dev.json` (or `~/.relayburn/models.dev.json`) and take precedence over the vendored snapshot at lookup time.

## Status

v0: Claude Code reader shipped. OpenCode reader in progress. Codex reader scaffolded. Per-tool-call attribution (`burn waste`), CLAUDE.md hot-path analysis, quota-window tracking (`burn limits`), waste-pattern detection, and subagent-tree queries are scoped and tracked as open issues.
