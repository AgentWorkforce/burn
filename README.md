![relayburn](./burn-readme-banner.png)

Understand how you're spending tokens in agent CLIs. Burn ingests Claude Code,
Codex, and OpenCode session logs into a local ledger, then shows cost by model,
provider, tool, file, workflow, agent, session, and overhead file.

## Quick Start

```bash
npm i -g relayburn
burn summary
```

Burn stores data under `~/.agentworkforce/burn/` by default. Set
`RELAYBURN_HOME` to use a different location.


## Commands

| Command | Use it to |
|---|---|
| [`burn summary`](#burn-summary) | See total usage and cost by model or provider. |
| [`burn hotspots`](#burn-hotspots) | Find expensive files, commands, and subagents. |
| [`burn overhead`](#burn-overhead) | Attribute cached prompt cost to `CLAUDE.md`, `.claude/CLAUDE.md`, and `AGENTS.md`. |
| [`burn compare`](#burn-compare) | Compare observed model performance by activity: cost per turn, one-shot rate, and sample size. |
| [`burn state`](#burn-state) | Inspect, fingerprint, rebuild, prune, or reset local ledger state. |
| [`burn sessions`](#burn-sessions) | Find recent session IDs for drill-down queries. |
| [`burn flow`](#burn-flow) | Render a session's inference and subagent flow as Mermaid, SVG, or JSON. |
| [`burn stamps`](#burn-stamps) | Export enrichment stamps as JSONL. |
| [`burn ingest`](#burn-ingest) | Import existing or live session logs without wrapping the harness. |
| [`burn mcp-server`](#burn-mcp-server) | Expose read-only cost queries to an agent through stdio MCP. |
| [`burn update`](#burn-update) | Check for releases, install an update, or configure automatic checks. |

## `burn summary`

Use `burn summary` when you want the fast answer: how many turns ran, how many
tokens they used, and what they cost.

| Option | What it does |
|---|---|
| `--since <range>` | Limit to a relative range like `24h`, `7d`, or `4w`, or an ISO timestamp. |
| `--project <path>` | Limit to a project path or git-canonical project key. |
| `--session <id>` | Limit to one session. |
| `--tag k=v` | Filter by folded enrichment tag. Repeatable; all tags must match. |
| `--group-by-tag <key>` | Group totals by a folded enrichment tag value. |
| `--by-provider` | Group totals by provider instead of model. |
| `--by-tool` | Attribute each turn's ingest cost to the preceding tool calls. |
| `--by-subagent-type` | Group totals by subagent type. |
| `--by-relationship [subagent]` | Group by session relationship, optionally drilling into subagent leaves. |
| `--subagent-tree [session]` | Render the subagent spawn tree. Uses `--session` when passed without a value. |
| `--provider <csv>` | Limit results to effective providers. |
| `--quality` | Append one-shot and completion-outcome metrics. |
| `--bucket <duration>` | Emit fixed-width time buckets across the `--since` window. |
| `--ingest` | Run one ingest sweep before querying. |
| `--json` | Emit machine-readable output. |

| Example | Result |
|---|---|
| `burn summary` | All-time cost by model. |
| `burn summary --since 24h` | Cost from the last 24 hours. |
| `burn summary --by-provider` | Cost grouped by effective provider. |
| `burn summary --by-tool` | Cost grouped by the tool calls that preceded each turn. |
| `burn summary --quality` | Usage totals with one-shot and completion outcomes. |
| `burn summary --since 24h --bucket 1h` | Hourly usage and cost for the last day. |
| `burn summary --tag persona=code-reviewer` | Cost for sessions stamped with that persona tag. |
| `burn summary --group-by-tag persona` | Cost grouped by persona value. |

Synthetic-routed models are recognized from `hf:*`,
`accounts/fireworks/models/*`, and `synthetic/*`.

## `burn hotspots`

Use `burn hotspots` when you want to know what made a session or time window
expensive. The default view attributes spend to files, bash commands, and
subagents.

| Option | What it does |
|---|---|
| `--since <range>` | Limit to a relative range or ISO timestamp. |
| `--project <path>` | Limit to a project. |
| `--session <id>` | Limit to a single session id. |
| `--workflow <id>` | Limit to turns folded under a `workflowId` enrichment stamp. |
| `--provider <csv>` | Restrict to providers (case-insensitive CSV — e.g. `anthropic,openai`). |
| `--all` | Show every row instead of the top 10. |
| `--group-by <dim>` | Focus one rollup: `attribution`, `bash`, `bash-verb`, `file`, or `subagent`. |
| `--patterns [csv]` | Run hotspot-pattern detectors instead of the attribution view. Pass without a value to enable every detector, or pass a CSV (e.g. `retry-loop,failure-run`). |
| `--findings` | Emit the unified findings table instead of the per-detector grouping. Implies `--patterns` if not already set. |
| `--rank-by <cost|bytes>` | Rank per-tool tables by USD or raw output bytes. Default: `cost`. |
| `--ingest` | Run one ingest sweep before querying. |
| `--json` | Emit machine-readable output. |

| Example | Result |
|---|---|
| `burn hotspots --since 7d` | Top costly files, bash commands, and subagents for the week. |
| `burn hotspots --all --project .` | Full project hotspot list. |
| `burn hotspots --group-by bash-verb --since 7d` | Bash verbs ranked by cost. |
| `burn hotspots --session demo-session` | Restrict the standard attribution view to one session. |
| `burn hotspots --patterns retry-loop,failure-run` | Surface retry/failure hotspot findings only. |
| `burn hotspots --findings --since 7d` | Unified severity-ranked findings list across every detector. |
| `burn hotspots --provider anthropic` | Restrict attribution to Anthropic-served turns. |
| `burn hotspots --rank-by bytes` | Surface large tool outputs even when truncation kept their token count small. |

## `burn overhead`

Use `burn overhead` when you want to know how much standing instruction files
cost. Burn discovers `CLAUDE.md`, `.claude/CLAUDE.md`, and `AGENTS.md`, then
attributes cached prompt cost to files and headed sections.

| Option | What it does |
|---|---|
| `trim` | Print projected-savings diffs for high-cost headed sections. Burn does not modify files. |
| `deltas` | Attribute context growth between consecutive inferences to intervening prompts, tool results, reminders, and compactions. |
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
| `burn overhead deltas --top 10 --owner main` | Largest context-window increases on the main conversation rail. |

`burn overhead deltas` also accepts `--session`, `--min-delta`, and `--explain`.
Use `--owner main`, `--owner subagent`, or `--owner all` to choose which
inference rails contribute.

Harnesses pay for different files: Claude Code pays for `CLAUDE.md`; Codex and
OpenCode pay for `AGENTS.md`.

## `burn compare`

Use `burn compare` when you want evidence for model choice. It compares models
on the work you actually ran, grouped by activity such as coding, debugging,
testing, review, exploration, docs, and refactoring.

| Option | What it does |
|---|---|
| `<model_a,model_b[,...]>` | Required comma-separated model list. At least two models. |
| `--since <range>` | Limit to a relative range or ISO timestamp. |
| `--project <path>` | Limit to a project. |
| `--session <id>` | Limit to one session. |
| `--workflow <id>` | Limit to turns folded with stamp `workflowId=<id>`. |
| `--agent <id>` | Limit to turns folded with stamp `agentId=<id>`. |
| `--provider <csv>` | Comma-separated effective providers (case-insensitive). |
| `--min-sample <n>` | Flag cells below the sample threshold. Default: `5`. |
| `--fidelity <class>` | Minimum data quality: `full`, `usage-only`, `aggregate-only`, `cost-only`, or `partial`. |
| `--include-partial` | Include every turn. Shorthand for `--fidelity partial`. |
| `--json` | Emit a stable JSON object. |
| `--csv` | Emit one row per model/activity pair. |
| `--bucket <duration>` | Emit a time series across `--since` instead of one comparison. |

`burn compare` reads the ledger as-is — it does not run an ingest sweep first.
Run `burn ingest && burn compare claude-sonnet-4-6,claude-haiku-4-5 --since
30d` (or keep `burn ingest --watch` running) when you need the freshest data.

| Example | Result |
|---|---|
| `burn compare claude-sonnet-4-6,claude-haiku-4-5 --since 30d` | Side-by-side activity table. |
| `burn compare claude-opus-4-7,claude-sonnet-4-6 --project . --json` | Project-scoped JSON comparison. |
| `burn compare claude-sonnet-4-6,claude-haiku-4-5 --fidelity full` | Compare only full-fidelity turns. |
| `burn compare claude-sonnet-4-6,claude-haiku-4-5 --include-partial` | Include lower-fidelity records too. |
| `burn compare claude-sonnet-4-6,claude-haiku-4-5 --since 7d --bucket 1d` | Daily comparison buckets for the last week. |

Run `burn summary --by-provider` to discover model IDs present in your ledger.

## `burn ingest`

Use `burn ingest` when sessions already exist, or when another process owns the
harness spawn. Default mode scans Claude Code, Codex, and OpenCode stores once.

| Option | What it does |
|---|---|
| `--watch` | Keep polling session stores in the foreground. |
| `--interval <ms>` | Poll interval in milliseconds. Default: `1000`. |
| `--quiet` | Suppress stderr progress spinner / breadcrumbs. One-shot mode still writes the final summary on stdout. |
| `--hook claude` | Read one Claude Code hook payload from stdin and ingest its single transcript via the SDK fast-path. |
| `--no-fsevents` | In watch mode, use polling instead of filesystem events. |

| Example | Result |
|---|---|
| `burn ingest` | Scan all known session stores once. |
| `burn ingest --watch` | Keep the ingest loop running. |
| `burn ingest --watch --no-fsevents` | Poll session stores when filesystem events are unreliable. |
| `burn ingest --hook claude --quiet` | Claude Code hook path for orchestrators. |

## `burn mcp-server`

Use `burn mcp-server` when an agent should query its own spend mid-session via
MCP. The server is stdio-only and read-only.

| Option | What it does |
|---|---|
| `--session-id <uuid>` | Default session ID used by MCP tools when the caller omits one. |

| Tool | What it returns |
|---|---|
| `burn__sessionCost` | Total USD, tokens, turns, and models for a session. |
| `burn__fingerprint` | A cheap `{count}:{maxMtimeUnix}:{totalBytes}` change token for all turns, one session, or one project. |

| Example | Result |
|---|---|
| `burn mcp-server --session-id demo-session` | Start a session-scoped stdio MCP server. |
| `burn mcp-server` | Start a server where tools require explicit session IDs. |

## `burn state`

Use `burn state` when reports look stale or you want to inspect or rebuild
derived storage. Burn stores events and stamps in `burn.sqlite`, and
content/search data in `content.sqlite`.

| Subcommand or option | What it does |
|---|---|
| `burn state` or `burn state status` | Print status for indexes, content, classifier, and archive. |
| `--json` | Emit machine-readable output. |
| `fingerprint [--session <id> | --project <path>]` | Print a low-cost change token for polling the ledger. |
| `rebuild index|classify|content|archive|all` | Drop derivable rows and stage the ledger for a fresh ingest. All targets use the same SQLite rebuild transaction. |
| `prune [--days <n|forever>]` | Delete content rows older than the retention window. |
| `reset [--force] [--reingest] [--json]` | Wipe derived state. Dry-run without `--force`; preserves config, pricing overrides, and source harness logs. |

| Example | Result |
|---|---|
| `burn state` | Derived artifact status. |
| `burn state status --json` | Machine-readable status. |
| `burn state fingerprint --project .` | Project-scoped ledger change token. |
| `burn state rebuild classify` | Drop derivable rows so the next ingest applies current classifier rules. |
| `burn state prune --days 30` | Prune content older than 30 days. |

Follow any `state rebuild` command with `burn ingest` to repopulate derived
tables from the harness session stores.

## `burn sessions`

`burn sessions list` prints recent sessions newest-first. The default window is
seven days and the default limit is 20 rows. Use `--project`, `--grep`,
`--since`, and `--limit` to narrow or widen the list; `--json` returns the same
records for scripts.

```bash
burn sessions list --since 30d --limit 10
burn sessions list --project . --json
```

The full session IDs copy directly into `summary --session`, `hotspots
--session`, `overhead deltas --session`, and `flow --session`.

## `burn flow`

`burn flow --session <id>` emits a Mermaid inference-flow DAG to stdout. Use
`--output <path>` for SVG, `--json` for the graph payload, and `--max-turns`
to cap wide sessions (default: 50; `0` disables the cap).

```bash
burn flow --session demo-session --json
```

## `burn stamps`

`burn stamps export` streams every enrichment stamp as JSONL. `--out <path>`
writes the same export to a file.

```bash
burn stamps export
```

## `burn update`

`burn update --check` reports whether a release is available without
installing it. Bare `burn update` installs the latest release through the
package manager that installed Burn. Automatic launch checks are controlled by
`burn update toggle-auto-update --on` and `--off`.

```bash
burn update --check
burn update toggle-auto-update --off
```

## Local Data

Burn keeps its 2.x ledger in two SQLite databases. Both use WAL mode, so
reporting readers can run while ingest writes. `burn.sqlite` is the event and
metadata database; `content.sqlite` separates larger prompt, response, and
search content from the compact analytical rows.

| Path or setting | Purpose |
|---|---|
| `~/.agentworkforce/burn/burn.sqlite` | Events, stamps, sessions, relationships, and archive metadata. |
| `~/.agentworkforce/burn/content.sqlite` | Prompt/response content and the FTS5 search index. |
| `~/.agentworkforce/burn/config.json` | Content-storage and retention configuration. |
| `~/.agentworkforce/burn/pending-stamps/` | Temporary manifests used by launchers that do not expose a session ID before spawn. |
| `RELAYBURN_HOME` | Override the whole Burn data directory. |
| `RELAYBURN_SQLITE_PATH` | Override the events database path. |
| `RELAYBURN_CONTENT_PATH` | Override the content database path. |
| `RELAYBURN_CONTENT_STORE=full|hash-only|off` | Control content payload storage. Default: `full`. |
| `RELAYBURN_CONTENT_TTL_DAYS=<days|forever>` | Content retention. Default: `90`. |

`RELAYBURN_HOME` relocates the complete layout. The two per-database overrides
can place event and content data on different volumes. SQLite may create
`-wal` and `-shm` files beside each open database; they are part of normal WAL
operation.

Harness transcripts remain the upstream input for ingest. `burn state status`
shows database paths, row counts, schema metadata, and resolved retention.
`burn state rebuild ...` clears derivable tables for re-ingest, `burn state
prune` applies content retention, and `burn state reset` previews or performs a
full derived-state wipe.

## Packages

| Package | Purpose |
|---|---|
| `relayburn` | npm install wrapper that resolves the prebuilt Rust `burn` binary from `@relayburn/cli-<platform>` optional dependencies. |
| `@relayburn/sdk` | Node facade over the Rust SDK, resolved through `@relayburn/sdk-<platform>` optional dependencies. |
| `@relayburn/cli-<platform>` | Prebuilt `burn` binary packages for supported OS/CPU targets. |
| `@relayburn/sdk-<platform>` | Prebuilt napi-rs packages for supported OS/CPU targets. |
| `relayburn-sdk` | Rust crate with the embedding API and internal reader/ledger/analyze/ingest modules. |
| `relayburn-cli` | Rust crate that produces the `burn` binary. |

## Development

Run `pnpm run verify` to run every check CI runs.

```bash
pnpm install
cargo build --workspace
cargo test --workspace
pnpm run test
pnpm run build:napi
```

The npm workspace contains the Node SDK facade, the `relayburn` install
wrapper, and the platform package manifests used by release automation.

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

Burn is local-first. Data lives in SQLite databases on your machine. Burn never
phones home. Pricing is looked up at query time from a vendored snapshot, so
rate corrections never require rewriting the ledger.

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
after any turns have been recorded. Launchers that do not know the session ID
before spawn should call `@relayburn/sdk` `writePendingStamp()` before
starting the agent, then run `burn ingest` / `ingest()` to fold the tags onto
the discovered turns. Direct Rust embedders with an exact session ID can use
`relayburn_sdk::Stamp` and `relayburn_sdk::StampSelector` against a
`LedgerHandle`.

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

The recommended launcher integration is the Node SDK pending-stamp primitive:

```js
import { writePendingStamp } from "@relayburn/sdk";

await writePendingStamp({
  harness: "codex",
  cwd: process.cwd(),
  enrichment: {
    persona: "code-reviewer",
    personaTier: "senior",
    agentworkforce: "1",
  },
});
```

Then spawn the harness normally and let `burn ingest` or `ingest()` scan the
session stores. Claude launchers can either preallocate `--session-id` and
write an exact session stamp from Rust, or use `writePendingStamp({ harness:
"claude", ... })` when the final session ID is not available before spawn.

Codex and OpenCode do not expose a pre-spawn session ID. `writePendingStamp()`
writes a pending-stamp manifest under `$RELAYBURN_HOME/pending-stamps/` before
the launcher spawns the agent. Ingest resolves the manifest against the first
matching session file before the first turn is appended. Claude launchers can
use the same pending-stamp path when the final session ID is not available
before spawn. Abandoned pending manifests are cleaned up after 24 hours.

For passive ingest, run:

```bash
burn ingest
burn ingest --watch --interval 1000
```

`burn ingest` scans Claude, Codex, and OpenCode stores once and uses the same
cursor and dedup path as the reporting commands. `burn ingest --watch` keeps
that scan loop running in the foreground.

### Hook-based ingest for orchestrators

If your code already controls the Claude Code spawn, you can install burn's
hooks per invocation via Claude's `--settings` flag without mutating global
`~/.claude/settings.json`. Wire the hook command to:

```bash
burn ingest --hook claude --quiet
```

Hook payloads land on stdin and get forwarded to `burn ingest`. The command is
safe to re-fire on every hook; the ledger cursor and dedup path keep ingestion
idempotent, so the hook path and normal session-store path reconcile against
the same session.

## FAQ

### What does `burn ingest` do?

Each harness (Claude Code, Codex, OpenCode) writes its own session transcripts
to disk in its own format. `burn ingest` reads those transcripts, normalizes
them, and writes them into burn's local SQLite ledger so the query commands
(`summary`, `hotspots`, `overhead`, `compare`) have something to read against.
Three modes:

- **One-shot** — `burn ingest` scans every known session store once and exits.
  Good for backfilling or catching up before a query.
- **Watch** — `burn ingest --watch` keeps a loop running in the foreground and
  picks up new turns as harnesses write them.
- **Hook** — `burn ingest --hook claude --quiet` reads a single Claude Code
  hook payload from stdin and ingests the transcript it points at.

### What is a "hook"?

Claude Code (and similar harnesses) lets users register shell commands that
fire on lifecycle events — turn start, tool use, session end, etc. When an
event fires, Claude pipes a JSON payload describing what just happened into
the registered command's stdin.

`burn ingest --hook claude` is burn registering itself as one of those
commands. Instead of polling the transcript file looking for new turns, burn
gets pushed each turn the moment it lands. Lower latency, no missed turns, no
FS-watching contortions.

### Why both watch and hook?

- **Hook** is the preferred path when the harness supports it: push-based,
  exact, no polling.
- **Watch** is the fallback for harnesses without hooks, or when the user
  hasn't wired one up. It polls the session store directories on an interval.
- **One-shot** is for backfill, cron, or "just give me numbers now."

For most solo users, `burn ingest --watch` in a background terminal is
simplest. The hook path matters most when you're building a launcher that
spawns Claude Code programmatically and wants per-invocation control without
mutating global config.

### How is the hook installed?

Burn does not ship an auto-installer. There are two integration paths:

**Per-invocation (orchestrators / launchers).** If you control the Claude
Code spawn, pass `--settings <path>` to Claude with a JSON file that
registers `burn ingest --hook claude --quiet` against the hook events you
care about. This is the recommended path because it avoids touching global
config and stays scoped to the invocation.

**Global (manual).** Edit `~/.claude/settings.json` (or the project-level
`.claude/settings.json`) and add `burn ingest --hook claude --quiet` to the
hook events you want to wire. Burn's hook policy is to always exit 0 so it
can never break the surrounding Claude Code session, and payloads missing
`session_id` or `transcript_path` are ignored, so wiring it onto extra
events is safe.

If you don't want to deal with hook config at all, run `burn ingest --watch`
instead — same data, slightly higher latency.
