![relayburn](./burn-readme-banner.png)

Understand how you're spending tokens in agent CLIs. Burn ingests Claude Code,
Codex, and OpenCode session logs into a local ledger, then shows cost by model,
provider, tool, file, workflow, agent, session, and overhead file.

## Quick Start

```bash
npm i -g relayburn
burn summary
```

Burn stores data under `~/.relayburn/` by default. Set `RELAYBURN_HOME` to use a
different location.


## Commands

| Command | Use it to |
|---|---|
| [`burn summary`](#burn-summary) | See total usage and cost by model, provider, tool, subagent, relationship, or session tree. |
| [`burn hotspots`](#burn-hotspots) | Find expensive files, commands, subagents, retries, failures, compactions, and other waste patterns. |
| [`burn overhead`](#burn-overhead) | Attribute cached prompt cost to `CLAUDE.md`, `.claude/CLAUDE.md`, and `AGENTS.md`. |
| [`burn compare`](#burn-compare) | Compare observed model performance by activity: cost per turn, one-shot rate, and sample size. |
| [`burn run`](#burn-run) | Spawn Claude, Codex, or OpenCode with attribution tags and automatic ingest. |
| [`burn ingest`](#burn-ingest) | Import existing or live session logs without wrapping the harness. |
| [`burn mcp-server`](#burn-mcp-server) | Expose read-only cost queries to an agent through stdio MCP. |
| [`burn state`](#burn-state) | Inspect, rebuild, and prune derived ledger artifacts. |

## `burn summary`

Use `burn summary` when you want the fast answer: how many turns ran, how many
tokens they used, and what they cost.

| Option | What it does |
|---|---|
| `--since <range>` | Limit to a relative range like `24h`, `7d`, or `4w`, or an ISO timestamp. |
| `--project <path>` | Limit to a project path or git-canonical project key. |
| `--session <id>` | Limit to one session. |
| `--workflow <id>` | Limit to turns stamped with `workflowId`. |
| `--agent <id>` | Limit to an agent, its parent/child sessions, or stamped `agentId`. |
| `--provider <p>` | Limit to an effective provider such as `anthropic`, `openai`, or `synthetic`. |
| `--by-provider` | Group totals by provider instead of model. |
| `--by-tool` | Show tool calls and attributed tool-ingest cost. |
| `--by-subagent-type` | Group subagent spend by subagent type. |
| `--by-relationship[=subagent]` | Group spend by session relationship, optionally narrowing to subagents. |
| `--subagent-tree <session-id>` | Render the root session plus connected subagent sessions. |
| `--quality` | Add session outcomes and one-shot edit rate. |
| `--json` | Emit machine-readable output. |
| `--no-archive` | Bypass `archive.sqlite` and stream the JSONL ledger directly. |

| Example | Result |
|---|---|
| `burn summary` | All-time cost by model. |
| `burn summary --since 24h` | Cost from the last 24 hours. |
| `burn summary --by-provider --provider synthetic` | Synthetic-routed usage only, grouped as provider `synthetic`. |
| `burn summary --by-tool --since 7d` | Tool calls ranked by attributed cost. |
| `burn summary --subagent-tree <session-id>` | Root session plus subagent cost tree. |

Provider filters are applied at query time; ledger rows are not rewritten.
Synthetic-routed models are recognized from `hf:*`,
`accounts/fireworks/models/*`, and `synthetic/*`.

## `burn hotspots`

Use `burn hotspots` when you want to know what made a session or time window
expensive. The default view attributes spend to files, bash commands, and
subagents. Pattern mode finds loops, failures, compactions, reverts, and prompt
surface problems.

| Option | What it does |
|---|---|
| `--since <range>` | Limit to a relative range or ISO timestamp. |
| `--project <path>` | Limit to a project. |
| `--workflow <id>` | Limit to stamped `workflowId`. |
| `--provider <p>` | Limit to an effective provider. |
| `--all` | Show every row instead of the top 10. |
| `--json` | Emit machine-readable output. |
| `--session [id]` | Diagnose one session. With no ID, diagnose the most recent matching session. |
| `--explain-drift` | In session mode, explain attribution drift between totals and attributed cost. |
| `--patterns[=<list>]` | Run pattern detectors. Bare `--patterns` runs all detectors. |
| `--findings` | Render pattern output as sorted findings. Also implies `--patterns`. |

Pattern names: `retries`, `failures`, `cancellations`, `compaction`,
`reverts`, `edit-heavy`, `opencode-skill-recall`,
`opencode-skill-pruning`, `opencode-system-prompt`, `ghost-surface`,
`tool-output-bloat`, `tool-call-pattern`.

`tool-call-pattern` flags vanilla call sequences with consolidatable
overhead: Glob → Grep → Read sequences, single-file edit clusters, and bash
calls for git state, test runs, and `gh pr` / `gh api`. Each finding reports
estimated tokens of overhead and the per-occurrence count, so downstream
tools (or you) can map patterns to a specific consolidation.

| Example | Result |
|---|---|
| `burn hotspots --since 7d` | Top costly files, bash commands, and subagents for the week. |
| `burn hotspots --all --project .` | Full project hotspot list. |
| `burn hotspots --patterns --since 7d` | All waste-pattern detectors over the week. |
| `burn hotspots --patterns=retries,failures --findings` | Retry and failure findings only. |
| `burn hotspots --session <session-id> --explain-drift` | Deep dive on one session. |

## `burn overhead`

Use `burn overhead` when you want to know how much standing instruction files
cost. Burn discovers `CLAUDE.md`, `.claude/CLAUDE.md`, and `AGENTS.md`, then
attributes cached prompt cost to files and headed sections.

| Option | What it does |
|---|---|
| `trim` | Print projected-savings diffs for high-cost headed sections. Burn does not modify files. |
| `--project <path>` | Project to inspect. Defaults to the current directory. |
| `--since <range>` | Limit attribution to a time window. |
| `--kind <k>` | Limit to `claude-md` or `agents-md`. |
| `--top <n>` | In `trim` mode, recommendations per file. |
| `--json` | Emit machine-readable attribution for report mode or structured trim recommendations in `trim` mode. |

| Example | Result |
|---|---|
| `burn overhead` | Cost per overhead file and section. |
| `burn overhead --since 30d` | Overhead cost from the last 30 days. |
| `burn overhead --kind claude-md` | Claude instruction files only. |
| `burn overhead trim --top 3` | Top three trim recommendations per file. |
| `burn overhead trim --json` | Structured trim recommendations with projected savings and unified diffs. |

Harnesses pay for different files: Claude Code pays for `CLAUDE.md`; Codex and
OpenCode pay for `AGENTS.md`.

## `burn compare`

Use `burn compare` when you want evidence for model choice. It compares models
on the work you actually ran, grouped by activity such as coding, debugging,
testing, review, exploration, docs, and refactoring.

| Option | What it does |
|---|---|
| `<model_a,model_b[,...]>` | Required comma-separated model list. At least two models. |
| `--provider <list>` | Include only effective providers such as `anthropic`, `openai`, or `synthetic`. |
| `--since <range>` | Limit to a relative range or ISO timestamp. |
| `--project <path>` | Limit to a project. |
| `--session <id>` | Limit to one session. |
| `--workflow <id>` | Limit to stamped `workflowId`. |
| `--agent <id>` | Limit to stamped `agentId`. |
| `--min-sample <n>` | Flag cells below the sample threshold. Default: `5`. |
| `--fidelity <class>` | Minimum data quality: `full`, `usage-only`, `aggregate-only`, `cost-only`, or `partial`. |
| `--include-partial` | Include every turn. Shorthand for `--fidelity partial`. |
| `--json` | Emit a stable JSON object. |
| `--csv` | Emit one row per model/activity pair. |
| `--no-archive` | Bypass `archive.sqlite` and stream the ledger directly. |

| Example | Result |
|---|---|
| `burn compare claude-sonnet-4-6,claude-haiku-4-5 --since 30d` | Side-by-side activity table. |
| `burn compare claude-opus-4-7,claude-sonnet-4-6 --workflow wf-refactor --json` | Workflow-scoped JSON comparison. |
| `burn compare claude-sonnet-4-6,claude-haiku-4-5 --fidelity full` | Compare only full-fidelity turns. |
| `burn compare claude-sonnet-4-6,claude-haiku-4-5 --include-partial` | Include lower-fidelity records too. |

Run `burn summary --by-provider` or `burn summary --by-tool` to discover model
IDs present in your ledger.

## `burn run`

Use `burn run` when you want Burn to launch the harness, stamp metadata, watch
for session logs, and ingest turns as the child exits. Supported harnesses:
`claude`, `codex`, and `opencode`.

| Option | What it does |
|---|---|
| `<harness>` | Required harness: `claude`, `codex`, or `opencode`. |
| `--tag k=v` | Stamp metadata onto the session. Repeatable. |
| `-- <harness args>` | Pass everything after `--` to the underlying harness. |

| Example | Result |
|---|---|
| `burn run claude --tag workflow=refactor -- --resume` | Resume Claude and stamp `workflow=refactor`. |
| `burn run codex --tag workflow=refactor` | Run Codex with workflow attribution. |
| `burn run opencode --tag agentId=ag-42 --tag tier=best` | Run OpenCode with agent and tier tags. |

Burn also reads these attribution environment variables and re-exports them to
the child process:

| Env var | Stamp key |
|---|---|
| `RELAYBURN_WORKFLOW_ID` | `workflowId` |
| `RELAYBURN_STEP_ID` | `stepId` |
| `RELAYBURN_AGENT_ID` | `agentId` |
| `RELAYBURN_PARENT_AGENT_ID` | `parentAgentId` |
| `RELAYBURN_PERSONA` | `persona` |
| `RELAYBURN_TIER` | `tier` |

Explicit `--tag k=v` values win over environment-derived tags.

## `burn ingest`

Use `burn ingest` when sessions already exist, or when another process owns the
harness spawn. Default mode scans Claude Code, Codex, and OpenCode stores once.

| Option | What it does |
|---|---|
| `--watch` | Keep polling session stores in the foreground. |
| `--interval <ms>` | Poll interval in milliseconds. Default: `1000`. |
| `--quiet` | Suppress normal ingest messages. |
| `--hook claude` | Read one Claude Code hook payload from stdin and ingest its transcript. |
| `--opencode-stream` | With `--watch`, subscribe to OpenCode's local SSE event stream. |
| `--opencode-url <url>` | OpenCode stream URL. Defaults to `OPENCODE_SERVER_URL` or `http://127.0.0.1:4096`. |
| `--opencode-global` | Use the global OpenCode stream scope. |

| Example | Result |
|---|---|
| `burn ingest` | Scan all known session stores once. |
| `burn ingest --watch` | Keep the ingest loop running. |
| `burn ingest --watch --opencode-stream` | Poll stores and stream OpenCode events. |
| `burn ingest --hook claude --quiet` | Claude Code hook path for orchestrators. |

If the OpenCode server uses Basic auth, set `OPENCODE_SERVER_USERNAME` and
`OPENCODE_SERVER_PASSWORD`.

## `burn mcp-server`

Use `burn mcp-server` when an agent should query its own spend mid-session via
MCP. The server is stdio-only and read-only.

| Option | What it does |
|---|---|
| `--session-id <uuid>` | Default session ID used by MCP tools when the caller omits one. |

| Tool | What it returns |
|---|---|
| `burn__sessionCost` | Total USD, tokens, turns, and models for a session. |

| Example | Result |
|---|---|
| `burn mcp-server --session-id <uuid>` | Start a session-scoped stdio MCP server. |
| `burn mcp-server` | Start a server where tools require explicit session IDs. |

## `burn state`

Use `burn state` when reports look stale, you upgraded Burn, or you want to
reclaim sidecar storage. Burn's canonical ledger is append-only JSONL; indexes,
content sidecars, activity labels, and `archive.sqlite` are rebuildable.

| Subcommand or option | What it does |
|---|---|
| `burn state` or `burn state status` | Print status for indexes, content, classifier, and archive. |
| `--json` | Emit machine-readable status or archive rebuild/vacuum output. |
| `rebuild index` | Rebuild ledger ID and content-fingerprint indexes. |
| `rebuild classify [--force]` | Re-run activity classification. `--force` overwrites existing labels. |
| `rebuild content` | Re-parse source session files to backfill content sidecars and user turns. |
| `rebuild archive` | Apply the ledger tail to `archive.sqlite`. |
| `rebuild archive --full` | Drop and rebuild `archive.sqlite` from zero. |
| `rebuild archive vacuum` or `rebuild archive --vacuum` | Reclaim unused SQLite pages. |
| `rebuild all [--force]` | Run content, index, classify, then archive. |
| `prune [--days <n>]` | Delete expired content sidecars. Use `forever` to disable. |
| `prune --force` | Delete recoverable sidecars even if source session files still exist. |
| `reset [--force] [--reingest] [--json]` | Wipe all derived state (ledger, indexes, cursors, archive, content sidecars). Dry-run without `--force`; preserves config, pricing overrides, and source harness logs. |

| Example | Result |
|---|---|
| `burn state` | Derived artifact status. |
| `burn state status --json` | Machine-readable status. |
| `burn state rebuild classify --force` | Reclassify every turn with current rules. |
| `burn state rebuild archive --full` | Rebuild the SQLite read model from scratch. |
| `burn state prune --days 30` | Prune content older than 30 days, keeping recoverable sidecars. |

## Local Data

| Path or setting | Purpose |
|---|---|
| `~/.relayburn/ledger.jsonl` | Canonical append-only ledger: usage, tool calls, IDs, stamps. |
| `~/.relayburn/content/<sessionId>.jsonl` | Content sidecar for prompts, responses, and tool I/O when available. |
| `~/.relayburn/archive.sqlite` | Rebuildable SQLite read model for fast reports. |
| `RELAYBURN_HOME` | Override the whole Burn data directory. |
| `RELAYBURN_CONTENT_STORE=full|hash-only|off` | Control content sidecar storage. Default: `full`. |
| `RELAYBURN_CONTENT_TTL_DAYS=<n>` | Sidecar retention. Default: `90`. |

Reports read local data from the ledger and derived sidecars.

## Packages

| Package | Purpose |
|---|---|
| `@relayburn/reader` | Pure parsers: session logs to `TurnRecord`. |
| `@relayburn/ledger` | JSONL ledger, stamps, indexes, content sidecars, archive. |
| `@relayburn/analyze` | Pricing, attribution, comparisons, overhead, and quality. |
| `@relayburn/ingest` | Session-store discovery, parse-and-append orchestration, pending stamps, watch loop. |
| `@relayburn/mcp` | Read-only MCP tools for in-session self-query. |
| `@relayburn/cli` | The `burn` binary. |
| `@relayburn/sdk` | Embeddable Node API: `ingest`, `summary`, `hotspots`. |
| `relayburn` | Thin install wrapper so `npm i -g relayburn` exposes `burn`. |

## Development

```bash
pnpm install
pnpm run build
pnpm run test
pnpm dev:cli summary --since 24h
```

Tests run against built `dist/` output. Use `pnpm run test:ts` to build and test
in one command.

## Pricing

Burn ships with a vendored [models.dev](https://models.dev) pricing snapshot.
Refresh it with:

```bash
pnpm run pricing:update
```

User overrides live at `$RELAYBURN_HOME/models.dev.json` and take precedence at
lookup time.

## What this is for

Agent spend happens in a blind spot. You can see a daily dollar total, and
maybe a breakdown by model. You cannot see which tool call, file, subagent, or
workflow step drove the cost. Burn makes that question answerable.

The deeper question burn is built around is:

> **Would the same work cost less with a different model, harness, or tool
> choice - in dollars or token usage?**

You cannot answer that from aggregate spend. It requires attribution at the
level of the actual work: this Read cost $0.47 because it added 8,200 tokens to
context that rode in every one of the next 23 turns' cache-reads. Once spend is
visible at that grain, the choice between Opus and Haiku, between Claude Code
and another harness, or between letting an agent re-read a file and passing it
a cached summary, becomes a decision you can reason about instead of a guess.

Three concrete questions follow:

1. **How much did I spend?** - per agent, workflow, session, model, and tool call.
2. **Why was it spent?** - tool calls, files, subagents, cache-hit vs fresh input, and persistence.
3. **Where can I save?** - redundant reads, bloated context prefixes, retry loops, and model choices that cost more than they needed to.

Burn is local-first. Data lives in an append-only JSONL ledger on your machine.
Burn never phones home. Pricing is looked up at query time from a vendored
snapshot, so rate corrections never require rewriting the ledger.

## What burn is not

- Not a dashboard or a product with a UI of its own.
- Not an automatic optimizer. It surfaces the choices; you decide.
- Not a leaderboard or a social service.

## Composability: how burn plugs into a spawner

Burn is designed to be called by whatever code spawns agent sessions. If you
control the spawn, you know things the session log never records on its own:
the workflow this session is part of, the persona it is running as, the agent
ID, and the tier. Burn accepts that context, attaches it to the session, and
makes it queryable later alongside the usage data from the session log.

The primitive is **stamping**: attach metadata to a session by ID, before or
after any turns have been recorded.

```ts
import { stamp } from '@relayburn/ledger';

// Your spawner knows metadata the session log does not carry.
// Stamp it against the session ID before or after spawn.
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
// Stamps can arrive before the first turn or after the last.
// Last write wins per key.
```

Stamp selectors:

- `{ sessionId }` - all turns in a session.
- `{ messageId }` - exactly one turn.
- `{ sessionId, range: { fromTs, toTs } }` - a time window, such as a single workflow step within a long-lived session.

Enrichment values are plain strings (`Record<string, string>`). Burn does not
care what keys you use. Typical keys: `agentId`, `parentAgentId`,
`workflowId`, `stepId`, `persona`, `tier`, `harness`, `userLabel`.

This is the composability surface. Burn stays small; the spawner owns the
context and decides what to attach.

### Spawner-integrated ingest

All three supported harnesses - Claude, Codex, and OpenCode - ship under one
verb:

```bash
burn run <claude|codex|opencode> [--tag k=v ...] [-- <harness args>]
```

For Claude, the adapter generates a session UUID up front so metadata can be
stamped before the agent starts. It passes `--session-id` to Claude, applies
any `--tag k=v` pairs as stamps, and ingests the session into the ledger when
Claude exits. If you are building an orchestrator, the same pattern applies:
generate the UUID, stamp first, spawn with the UUID, then let burn pick up the
session log on ingest.

```bash
burn run claude --tag workflow=refactor --tag persona=senior-eng -- --resume abc
```

Codex and OpenCode do not expose Claude-style hooks or a pre-spawn session ID.
Their adapters write a v1 pending-stamp manifest under
`$RELAYBURN_HOME/pending-stamps/` before spawning, then resolve it against the
first matching session file before the first turn is appended. They also run
burn's foreground watch loop for the child process lifetime, so long sessions
become visible incrementally instead of only after exit. Abandoned pending
manifests are cleaned up after 24 hours.

For passive ingest without a wrapper, run:

```bash
burn ingest
burn ingest --watch [--interval <ms>] [--opencode-stream] [--opencode-url <url>]
```

`burn ingest` scans Claude, Codex, and OpenCode stores once and uses the same
cursor and dedup path as the reporting commands. `burn ingest --watch` keeps
that scan loop running in the foreground. `--opencode-stream` also subscribes
to OpenCode's local SSE endpoint (`http://127.0.0.1:4096/event` by default, or
`OPENCODE_SERVER_URL`) and ingests sessions observed from creation directly
from the stream at completed tool-call grain. Polling remains enabled as the
fallback for already-running sessions, missed events, and drift checks. If the
server uses Basic auth, `OPENCODE_SERVER_USERNAME` and
`OPENCODE_SERVER_PASSWORD` are forwarded.

### Spawner env-var contract

For orchestrators that spawn many agent sessions, threading `--tag` through
every wrapper invocation is awkward. All three adapters also read a fixed set
of `RELAYBURN_*` env vars and fold them into the stamp bag:

| Env var | Stamp key |
|---|---|
| `RELAYBURN_WORKFLOW_ID` | `workflowId` |
| `RELAYBURN_STEP_ID` | `stepId` |
| `RELAYBURN_AGENT_ID` | `agentId` |
| `RELAYBURN_PARENT_AGENT_ID` | `parentAgentId` |
| `RELAYBURN_PERSONA` | `persona` |
| `RELAYBURN_TIER` | `tier` |

`--tag k=v` flags win on key collision. The merged values are re-exported on
the child harness environment under their canonical names, so a transitive
`burn ...` invocation inside the child session inherits the same context
without the orchestrator having to re-thread it.

```bash
export RELAYBURN_WORKFLOW_ID=wf-refactor-auth
export RELAYBURN_AGENT_ID=ag-42
burn run codex
burn run opencode --tag agentId=ag-43
```

Other `RELAYBURN_*` variables (`RELAYBURN_HOME`, `RELAYBURN_SESSION_ID`,
`RELAYBURN_CONTENT_STORE`, `RELAYBURN_CONTENT_TTL_DAYS`) are burn internals and
are not treated as stamp tags.

### Hook-based ingest for orchestrators

If your code already controls the Claude Code spawn, you can install burn's
hooks per invocation via Claude's `--settings` flag without mutating global
`~/.claude/settings.json`. Hook payloads land on stdin and get forwarded to
`burn ingest`, which incrementally parses the transcript with the same cursor
and dedup machinery as `burn run claude`.

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

`buildClaudeHookSettings({ burnBin? })` returns a fresh UUID and a JSON string
wiring every Claude Code hook event (`PreToolUse`, `PostToolUse`,
`UserPromptSubmit`, `Notification`, `Stop`, `SubagentStop`, `SessionEnd`) to
`burn ingest --hook claude --quiet`. The command is safe to re-fire on every
hook. The ledger cursor and dedup index keep ingestion idempotent, so the hook
path and JSONL-reader path are reconcilable against the same session. Tool-call
failures ride in the normal `PostToolUse` payload, surfaced as
`ToolCall.isError` on the resulting `TurnRecord`, not a distinct
`PostToolUseFailure` event.
