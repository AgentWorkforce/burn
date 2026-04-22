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

For Claude Code specifically, burn generates session UUIDs up-front so metadata can be stamped before the agent starts:

```
burn claude [--tag k=v ...] [-- <claude args>]
```

The wrapper pre-assigns a session ID, passes `--session-id` to Claude, applies any `--tag k=v` pairs as stamps, and ingests the session into the ledger when Claude exits. If you are building an orchestrator, the same pattern applies: generate the UUID, stamp first, spawn with the UUID, let burn pick up the session log on ingest.

```
burn claude --tag workflow=refactor --tag persona=senior-eng -- --resume abc
```

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

Retention defaults to 90 days for the sidecar, forever for the main ledger. Configure via `RELAYBURN_CONTENT_TTL_DAYS`.

Three content modes:

- `content.store=full` (default) — everything above.
- `content.store=hash-only` — usage + hashes + metadata, no prompt/response content. Restores burn's minimal-storage behavior for sensitive environments.
- `content.store=off` — skip the sidecar entirely; no content directory.

Set via `RELAYBURN_CONTENT_STORE=<mode>` or the config file.

Content lives on your machine. Burn makes no outbound requests beyond optional pricing updates. If the device you run on is sensitive to conversation leak (shared dev machines, cloud-synced home directories, compliance contexts), switch to `hash-only`.

## Activity classification

Every turn is tagged with an `activity` label so cost can be compared like-for-like. "Sonnet cost more than Haiku last month" isn't useful on its own; "Sonnet and Haiku both attempted 43 refactoring turns, Sonnet landed them first-try 75% of the time vs. Haiku's 60%" is. That's what per-turn activity enables.

Classification is deterministic and rule-based — no LLM in the loop. Every turn with the same tool calls and prompt text produces the same label.

Twelve categories, from codeburn's taxonomy so cross-tool comparison stays possible:

| Category | Trigger |
|---|---|
| `planning` | `ExitPlanMode` tool, or planning/roadmap keywords with no tool use |
| `delegation` | `Agent` / `Task` spawn — dominates other signals |
| `testing` | `Bash` matching `pytest`, `vitest`, `bun test`, `jest`, `go test`, `cargo test`, `npm test`, etc. |
| `git` | `Bash` matching `git push/pull/commit/merge/rebase/checkout/cherry-pick/...` |
| `build-deploy` | `Bash` matching `docker build`, `cargo build`, `npm run build`, `kubectl apply`, `terraform apply`, etc. |
| `coding` | `Edit` / `Write` / `NotebookEdit` with no stronger keyword signal |
| `debugging` | Edit turn where prompt mentions bug/error/crash/traceback, or any tool call errored this turn |
| `refactoring` | Edit turn with keywords: `refactor`, `cleanup`, `rename`, `extract`, `restructure` |
| `feature` | Edit turn with keywords: `add`, `create`, `implement`, `new`, `introduce` |
| `exploration` | `Read` / `Grep` / `Glob` / `WebFetch` / `WebSearch` without edits |
| `brainstorming` | No tool use; prompt asks *what if*, *think through*, *should we*, *design* |
| `conversation` | No tool use, no category keywords — the fallback |

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

## CLI

```
burn summary [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>]
burn by-tool [--since 7d] [--project <path>] [--session <id>]
burn claude  [--tag k=v ...] [-- <claude args>]
```

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
