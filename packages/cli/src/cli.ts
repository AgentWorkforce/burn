#!/usr/bin/env node
import { parseArgs } from './args.js';
import { runCompare } from './commands/compare.js';
import { runContent, opportunisticPrune } from './commands/content.js';
import { runOverhead } from './commands/overhead.js';
import { runIngest } from './commands/ingest.js';
import { runLimits } from './commands/limits.js';
import { runMcpServer } from './commands/mcp-server.js';
import { runPlans } from './commands/plans.js';
import { runRebuild } from './commands/rebuild.js';
import { runWrapper } from './commands/run.js';
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
  burn limits        [--watch [5s]] [--json] [--no-api] [--no-forecast]
  burn plans         [add|remove|set-reset-day] …  (run \`burn plans help\` for full usage)
  burn overhead      [trim] [--project <path>] [--since 7d] [--kind <k>] [--top <n>] [--json]
  burn compare       <model_a,model_b[,...]> [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--min-sample <n>] [--json|--csv]
  burn run <${HARNESS_LIST}>  [--tag k=v ...] [-- <harness args>]
  burn ingest        [--watch|--hook <name>] [--interval <ms>] [--quiet]
  burn mcp-server    [--session-id <uuid>]          (stdio MCP server for in-session self-query)
  burn content prune [--days <n>] [--force]
  burn rebuild       index | classify | content | archive [--full|--vacuum] | all | status [--json]

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
  burn limits
  burn limits --watch
  burn limits --no-api
  burn plans
  burn plans add --provider claude --preset max
  burn plans set-reset-day claude-max 15
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
  burn content prune --days 30
  burn rebuild status
  burn rebuild archive
  burn rebuild archive --full
  burn rebuild archive vacuum
  burn rebuild classify

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
  if (cmd !== 'content') {
    await opportunisticPrune();
  }
  switch (cmd) {
    case 'summary':
      return runSummary(args);
    case 'hotspots':
      return runHotspots(args);
    case 'limits':
      return runLimits(args);
    case 'plans':
      return runPlans(args);
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
    case 'content':
      return runContent(args);
    case 'rebuild':
      return runRebuild(args);
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
