#!/usr/bin/env node
import { parseArgs } from './args.js';
import { runBudget } from './commands/budget.js';
import { runCompare } from './commands/compare.js';
import { runOverhead } from './commands/overhead.js';
import { runIngest } from './commands/ingest.js';
import { runMcpServer } from './commands/mcp-server.js';
import { runWrapper } from './commands/run.js';
import { runState, opportunisticPrune } from './commands/state.js';
import { runSummary } from './commands/summary.js';
import { runHotspots } from './commands/hotspots.js';
import { listHarnessNames } from './harnesses/registry.js';

const HARNESS_LIST = listHarnessNames().join('|');

const HELP = `burn — token usage & cost attribution for agent CLIs

Usage:
  burn summary       [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--provider <p>] [--quality]
                     [--by-provider | --by-tool | --by-subagent-type | --by-relationship[=subagent] | --subagent-tree <session-id>] [--no-archive]
                     (mode flags are mutually exclusive; --by-tool emits tool | calls | attributedCost)
  burn hotspots      [--since 7d] [--project <path>] [--workflow <id>] [--provider <p>] [--all] [--json]
                     [--session [id]] [--explain-drift]
                     [--patterns[=retries,failures,compaction,reverts]] [--findings]
  burn budget        [--watch [--interval 5s]] [--json] [--no-api] [--no-forecast] [plans ...]
  burn overhead      [trim] [--project <path>] [--since 7d] [--kind <k>] [--top <n>] [--json]
  burn compare       <model_a,model_b[,...]> [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--min-sample <n>] [--json|--csv]
  burn run <${HARNESS_LIST}>  [--tag k=v ...] [-- <harness args>]
  burn ingest        [--watch|--hook <name>] [--interval <ms>] [--quiet]
  burn mcp-server    [--session-id <uuid>]          (stdio MCP server for in-session self-query)
  burn state         [status] [--json]
  burn state rebuild index | classify | content | archive [--full|--vacuum] | all
  burn state prune   [--days <n>] [--force]

Examples:
  burn summary --since 24h
  burn summary --by-provider --provider synthetic
  burn summary --subagent-tree <session-id>
  burn summary --by-subagent-type --since 7d
  burn summary --by-relationship --since 7d
  burn summary --by-tool --since 7d
  burn hotspots --since 7d
  burn hotspots --patterns --since 7d
  burn hotspots --session --explain-drift
  burn hotspots --session <session-id>
  burn budget
  burn budget --watch
  burn budget --no-api
  burn budget plans
  burn budget plans add --provider claude --preset max
  burn budget plans set-reset-day claude-max 15
  burn overhead --since 30d
  burn overhead --kind claude-md
  burn overhead trim --top 3
  burn compare claude-sonnet-4-6,claude-haiku-4-5 --since 30d
  burn run claude   --tag workflow=refactor -- --resume
  burn run codex    --tag workflow=refactor
  burn run opencode --tag workflow=refactor
  burn ingest
  burn ingest --watch
  burn ingest --watch --opencode-stream
  burn state
  burn state prune --days 30
  burn state rebuild archive
  burn state rebuild archive --full
  burn state rebuild archive vacuum
  burn state rebuild classify

Provider filters are query-time only. Synthetic-routed models are recognized
from hf:*, accounts/fireworks/models/*, and synthetic/* model IDs and are
reported as provider "synthetic" without rewriting ledger rows.
`;

async function main(): Promise<number> {
  const [, , cmd, ...rest] = process.argv;
  if (!cmd || cmd === 'help' || cmd === '--help' || cmd === '-h') {
    process.stdout.write(HELP);
    return 0;
  }
  const args = parseArgs(rest);
  // Opportunistic content-sidecar retention prune on every invocation.
  // Best-effort; never fails the CLI.
  if (!(cmd === 'state' && args.positional[0] === 'prune')) {
    await opportunisticPrune();
  }
  switch (cmd) {
    case 'summary':
      return runSummary(args);
    case 'hotspots':
      return runHotspots(args);
    case 'budget':
      return runBudget(args);
    case 'overhead':
      return runOverhead(args);
    case 'compare':
      return runCompare(args);
    case 'run':
      return runWrapper(args);
    case 'ingest':
      return runIngest(args);
    case 'mcp-server':
      return runMcpServer(args);
    case 'state':
      return runState(args);
    default:
      process.stderr.write(`unknown command: ${cmd}\n\n${HELP}`);
      return 1;
  }
}

main().then(
  (code) => process.exit(code),
  (err) => {
    process.stderr.write(`burn: ${err?.message ?? err}\n`);
    process.exit(1);
  },
);
