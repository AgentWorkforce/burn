#!/usr/bin/env node
import { parseArgs } from './args.js';
import { runArchive } from './commands/archive.js';
import { runByTool } from './commands/by-tool.js';
import { runClaudeWrapper } from './commands/claude.js';
import { runCodexWrapper } from './commands/codex.js';
import { runCompare } from './commands/compare.js';
import { runContent, opportunisticPrune } from './commands/content.js';
import { runContext } from './commands/context.js';
import { runDiagnose } from './commands/diagnose.js';
import { runIngest } from './commands/ingest.js';
import { runLimits } from './commands/limits.js';
import { runMcpServer } from './commands/mcp-server.js';
import { runOpencodeWrapper } from './commands/opencode.js';
import { runPlans } from './commands/plans.js';
import { runRebuild } from './commands/rebuild.js';
import { runRebuildIndex } from './commands/rebuild-index.js';
import { runSummary } from './commands/summary.js';
import { runWaste } from './commands/waste.js';

const HELP = `burn — token usage & cost attribution for agent CLIs

Usage:
  burn summary       [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--provider <p>] [--quality]
                     [--by-provider] [--subagent-tree <session-id>] [--by-subagent-type]
  burn by-tool       [--since 7d] [--project <path>] [--session <id>] [--provider <p>]
  burn waste         [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--provider <p>] [--all] [--json]
                     [--patterns[=retries,failures,compaction,reverts]]
  burn diagnose      <session-id> [--json]
  burn limits        [--watch [5s]] [--json] [--no-api] [--no-forecast]
  burn plans         [add|remove|set-reset-day] …  (run \`burn plans help\` for full usage)
  burn context       [advise] [--project <path>] [--since 7d] [--kind <k>] [--top <n>] [--json]
  burn compare       [--models a,b] [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--min-sample <n>] [--json|--csv]
  burn claude        [--tag k=v ...] [-- <claude args>]
  burn codex         [--tag k=v ...] [-- <codex args>]
  burn opencode      [--tag k=v ...] [-- <opencode args>]
  burn ingest        --runtime claude [--quiet]     (reads hook payload on stdin)
  burn mcp-server    [--session-id <uuid>]          (stdio MCP server for in-session self-query)
  burn content prune [--days <n>] [--force]
  burn archive       build | rebuild | status [--json]
  burn rebuild         --index | --reclassify [--force]
  burn rebuild-index   (alias for 'burn rebuild --index')

Examples:
  burn summary --since 24h
  burn summary --by-provider --provider synthetic
  burn summary --subagent-tree <session-id>
  burn summary --by-subagent-type --since 7d
  burn by-tool --since 7d
  burn waste --since 7d
  burn waste --patterns --since 7d
  burn diagnose <session-id>
  burn limits
  burn limits --watch
  burn limits --no-api
  burn plans
  burn plans add --provider claude --preset max
  burn plans set-reset-day claude-max 15
  burn context --since 30d
  burn context --kind claude-md
  burn context advise --top 3
  burn compare --since 30d --models claude-sonnet-4-6,claude-haiku-4-5
  burn claude   --tag workflow=refactor -- --resume
  burn codex    --tag workflow=refactor
  burn opencode --tag workflow=refactor
  burn content prune --days 30
  burn archive status
  burn archive build
  burn archive rebuild
  burn rebuild --reclassify

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
    case 'by-tool':
      return runByTool(args);
    case 'waste':
      return runWaste(args);
    case 'diagnose':
      return runDiagnose(args);
    case 'limits':
      return runLimits(args);
    case 'plans':
      return runPlans(args);
    case 'context':
      return runContext(args);
    case 'compare':
      return runCompare(args);
    case 'claude':
      return runClaudeWrapper(args);
    case 'codex':
      return runCodexWrapper(args);
    case 'opencode':
      return runOpencodeWrapper(args);
    case 'ingest':
      return runIngest(args);
    case 'mcp-server':
      return runMcpServer(args);
    case 'content':
      return runContent(args);
    case 'archive':
      return runArchive(args);
    case 'rebuild':
      return runRebuild(args);
    case 'rebuild-index':
      return runRebuildIndex();
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
