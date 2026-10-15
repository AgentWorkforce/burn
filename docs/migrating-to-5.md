# Migrating to burn 5

burn 5 reads every session through
[relayhistory](https://github.com/AgentWorkforce/relayhistory)'s session store
instead of parsing harness logs itself. Commands, flags, ledger location and
report shapes are unchanged; what changes is where sessions come from and a
few numbers that were wrong before.

## What changes for users

- **Subagent spend is billed.** Every turn of a Claude Code subagent
  (`<session>/subagents/agent-<id>.jsonl`), nested subagents included, is a
  turn of the session that spawned it, marked with the subagent's `agentId`.
  4.x never read those transcripts, so totals for sessions that delegated
  work go up.
- **OpenCode's SQLite store is read.** Current OpenCode releases keep sessions
  in `~/.local/share/opencode/opencode.db`; burn reads it (and the older
  `storage/` JSON tree).
- **GitHub Copilot CLI sessions are read.** Copilot's session journals
  (`~/.copilot/session-state/<id>/events.jsonl`) are ingested retroactively as
  `copilot-cli` turns, with no exporter to enable.
- **Archived Codex threads are read.** Rollouts Codex moved to
  `~/.codex/archived_sessions/` are ingested like live ones.
- **Codex usage comes from each response's raw token snapshot**, and a Codex
  subagent thread is a session of its own.
- **Every Codex task is billed.** A task commits at `task_complete`, at
  `turn_aborted` (stop reason `aborted`), or when the next `task_started`
  begins. A task still open at the end of a rollout that reported spend is billed
  once nothing has been written to the rollout for two minutes, as `<turn id>@<line>`; if the
  session continues it after all, the rest is billed as a further turn of that
  task. 4.x billed only tasks a later `task_complete` followed, so spend on
  interrupted and unfinished tasks was missing.
- Two deliberate record changes, also in the sourcing snapshots: a failed
  OpenCode tool call sets `isError`, and a Claude assistant turn still being
  written (no `stop_reason` yet) is not billed until it settles.

## First run

The first `burn ingest` (or any command run with `--ingest`):

1. Opens the relayhistory store at `$AI_HIST_DB`, else
   `~/.local/share/ai-hist/ai-history.db`, creating it when it does not
   exist. The `ai-hist` CLI does not need to be installed; when it is, burn and
   it share the store.
2. Syncs the store. A new store reads every session on the machine once, so
   the first sync takes as long as the history is large; later syncs read only
   what changed.
3. Reconciles the ledger: an existing 4.x ledger has no relayhistory
   watermark, so every session in the store is rebuilt and appended. Turn
   identities match the 4.x readers', so turns already in the ledger are not
   duplicated; only turns 4.x never counted (subagent turns, archived Codex
   threads, OpenCode SQLite sessions) are added. The ledger then records the store's change-feed
   watermark, and later ingests read only sessions that changed.

How long it takes, measured on one machine with about 10 GB of harness history
(7.2 GB of Codex rollouts, 3.9 GB of OpenCode data, 0.6 GB of Claude
transcripts):

| Run | Time |
|---|---|
| First run, new store (sync + rebuild) | 16–17 minutes |
| First run, store already synced by `ai-hist` | about 7 minutes |
| `burn ingest`, nothing new | under 0.3 seconds |
| `burn ingest`, one session gained a turn | about 5 seconds |

The run after a new store's first sync takes longer (about 20 seconds here) and
may still append a few Claude turns: relayhistory holds a transcript's last
message back until a later sweep shows nothing follows it. An ingest after new
activity spends its time in the store's sweep of every harness;
`burn ingest --watch` reads only the files that changed between its periodic
full sweeps, and `burn ingest --hook claude` reads only the transcript the hook
names.

## Commands

- `burn ingest` = sync relayhistory, then append what its change feed reports.
- `burn ingest --watch` follows relayhistory's live-capture loop
  (filesystem events with a polling backstop; `--no-fsevents` and
  `--interval` keep their meaning).
- `burn ingest --hook claude --quiet` is unchanged: it indexes just the
  transcript the hook names.
- `burn state rebuild` clears the ledger's watermark with the derived rows, so
  the next ingest rebuilds every session from the relayhistory store; the
  harness logs are not re-read.
- `burn state reset` likewise; the store itself is never modified by burn
  beyond relayhistory's own sync.

## Environment

| Variable | In burn 5 |
|---|---|
| `AI_HIST_DB` | The relayhistory store burn reads. |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `OPENCODE_DB`, `COPILOT_HOME` | Harness store locations, resolved by relayhistory. |
| `BURN_CLAUDE_PROJECTS_DIR` | Removed. Point `CLAUDE_CONFIG_DIR` (or `HOME`) at the Claude install instead. |
| `RELAYBURN_HOME`, `RELAYBURN_*` ledger settings | Unchanged. |

## Embedders

- Rust: `IngestOptions.roots` / `IngestRoots` are replaced by
  `IngestOptions.store: HistoryStoreOptions { db_path, home }`. An explicit
  `home` is the whole provider layout (`<home>/.claude`, `<home>/.codex`,
  `<home>/.local/share/opencode`). `start_watch_loop` and its options are
  replaced by `watch_ingest(ledger, &opts, WatchIngestOptions, on_tick)`;
  `ingest_claude_session`, `ingest_codex_sessions`,
  `ingest_opencode_sessions`, `default_session_roots`,
  `count_subagents_under`, `discover_subagents` and `pair_subagents_to_main`
  are gone. `RawIngestOptions` is `IngestOptions`.
- Node (`@relayburn/sdk`): `ingest()` takes `{ ledgerHome, storeDbPath, home }`;
  the unused `sessionId` / `harness` fields are gone.
