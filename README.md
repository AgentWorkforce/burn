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
- Not a content store. Prompts and model responses are never captured.
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
}
```

## What burn records, and what it doesn't

Recorded:
- Token counts, timestamps, model identifiers.
- Tool names, tool-call argument hashes, file paths touched.
- Session IDs, message IDs, and whatever metadata you stamp against them.

Not recorded:
- Prompt content.
- Model responses.
- Tool-call arguments beyond their hash.
- Tool-call outputs.

The ledger can live locally without leaking conversation content. This is deliberate: it keeps adoption reviewable — you can read the reader source and verify what enters the ledger.

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
