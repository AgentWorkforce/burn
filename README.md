# relayburn

Token usage & cost attribution for agent CLIs (Claude Code today; Codex / opencode coming).

Part of the `relay*` family of single-concern primitives alongside `relayfile`, `relaycast`, `relayauth`.

## What it does

Answers three questions for any Claude Code user:

1. **How much did I spend?** — per agent, per workflow, per day, per model.
2. **Why was it spent?** — which tool calls, which files, which subagents, cache vs fresh.
3. **Where can I save?** — cache-miss ratios, redundant file re-reads, Opus-when-Haiku-would-do (coming in v0.1).

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

## CLI

```
burn summary [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>]
burn by-tool [--since 7d] [--project <path>] [--session <id>]
burn claude  [--tag k=v ...] [-- <claude args>]
```

`burn claude` wraps Claude Code: it pre-generates a session UUID, passes `--session-id`, stamps any `--tag k=v` pairs onto every turn, and ingests the session into the ledger when Claude exits.

```
burn claude --tag workflow=refactor -- --resume abc
```

## Packages

| Package | Purpose |
|---|---|
| `@relayburn/reader` | Pure parsers: session log → `TurnRecord[]`. No I/O writes, no state. |
| `@relayburn/ledger` | Append-only JSONL ledger at `~/.relayburn/ledger.jsonl`. Exposes `appendTurns`, `stamp`, `query`. |
| `@relayburn/analyze` | Pricing loader (models.dev) + per-record cost derivation. |
| `@relayburn/cli` | `burn` binary: spawn wrapper + read/report commands. |

Override ledger location with `RELAYBURN_HOME=/path/to/dir`.

## Stamping (for integrators)

`agent-relay` and `agent-workforce` spawn agent sessions and know metadata that session logs don't carry (agent id, workflow id, persona, tier). relayburn is the substrate they stamp into.

```ts
import { stamp } from '@relayburn/ledger';

// Spawn-time, after you know the sessionId
await stamp(
  { sessionId: 'claude-session-uuid' },
  { agentId: 'ag-42', workflowId: 'wf-refactor', persona: 'posthog', tier: 'best' }
);

// Later turns in the same session automatically inherit this enrichment
// at query time. Last-write-wins per key; stamps can arrive before or
// after the turn record.
```

Stamp selectors:

- `{ sessionId }` — all turns in a session.
- `{ messageId }` — exactly one turn.
- `{ sessionId, range: { fromTs, toTs } }` — scoped to a time window (e.g. a single workflow step within a long-lived session).

Enrichment values are plain strings (`Record<string, string>`); relayburn doesn't care what keys you use. Typical: `agentId`, `parentAgentId`, `workflowId`, `stepId`, `persona`, `tier`, `harness`, `userLabel`.

## Data model

One `TurnRecord` per distinct `message.id` in the session log. Cost is never stored — only `usage` — so pricing corrections don't require a migration.

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

## Pricing

Shipped with a vendored snapshot of [models.dev](https://models.dev). Refresh with:

```bash
npm run pricing:update
```

User overrides go at `$RELAYBURN_HOME/models.dev.json` (or `~/.relayburn/models.dev.json`) and take precedence over the vendored snapshot at lookup time.

## Status

v0: Claude Code only. Codex / opencode reader implementations deferred. `burn tree`, `burn waste`, `burn watch`, `burn where`, `burn tag`, `burn export` deferred.
