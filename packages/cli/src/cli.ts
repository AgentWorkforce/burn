#!/usr/bin/env node
import { parseArgs } from './args.js';
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
  burn state reset   [--force] [--reingest] [--json]

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
  burn overhead trim --json
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
  switch (cmd) {
    case 'summary':
      return (await import('./commands/summary.js')).runSummary(args);
    case 'hotspots':
      return (await import('./commands/hotspots.js')).runHotspots(args);
    case 'budget':
      return (await import('./commands/budget.js')).runBudget(args);
    case 'overhead':
      return (await import('./commands/overhead.js')).runOverhead(args);
    case 'compare':
      return (await import('./commands/compare.js')).runCompare(args);
    case 'run':
      return (await import('./commands/run.js')).runWrapper(args);
    case 'ingest':
      return (await import('./commands/ingest.js')).runIngest(args);
    case 'mcp-server':
      return (await import('./commands/mcp-server.js')).runMcpServer(args);
    case 'state':
      return (await import('./commands/state.js')).runState(args);
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
