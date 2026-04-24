#!/usr/bin/env node
import { parseArgs } from './args.js';
import { runByTool } from './commands/by-tool.js';
import { runClaudeWrapper } from './commands/claude.js';
import { runCodexWrapper } from './commands/codex.js';
import { runCompare } from './commands/compare.js';
import { runContent, opportunisticPrune } from './commands/content.js';
import { runContext } from './commands/context.js';
import { runDiagnose } from './commands/diagnose.js';
import { runIngest } from './commands/ingest.js';
import { runOpencodeWrapper } from './commands/opencode.js';
import { runRebuild } from './commands/rebuild.js';
import { runRebuildIndex } from './commands/rebuild-index.js';
import { runSummary } from './commands/summary.js';
import { runWaste } from './commands/waste.js';

const HELP = `burn — token usage & cost attribution for agent CLIs

Usage:
  burn summary       [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--quality]
  burn by-tool       [--since 7d] [--project <path>] [--session <id>]
  burn waste         [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--all] [--json]
                     [--patterns[=retries,failures,compaction,reverts]]
  burn diagnose      <session-id> [--json]
  burn context       [advise] [--project <path>] [--since 7d] [--kind <k>] [--top <n>] [--json]
  burn compare       [--models a,b] [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>] [--min-sample <n>] [--json|--csv]
  burn claude        [--tag k=v ...] [-- <claude args>]
  burn codex         [--tag k=v ...] [-- <codex args>]
  burn opencode      [--tag k=v ...] [-- <opencode args>]
  burn ingest        --runtime claude [--quiet]     (reads hook payload on stdin)
  burn content prune [--days <n>]
  burn rebuild         --index | --reclassify [--force]
  burn rebuild-index   (alias for 'burn rebuild --index')

Examples:
  burn summary --since 24h
  burn by-tool --since 7d
  burn waste --since 7d
  burn waste --patterns --since 7d
  burn diagnose <session-id>
  burn context --since 30d
  burn context --kind claude-md
  burn context advise --top 3
  burn compare --since 30d --models claude-sonnet-4-6,claude-haiku-4-5
  burn claude   --tag workflow=refactor -- --resume
  burn codex    --tag workflow=refactor
  burn opencode --tag workflow=refactor
  burn content prune --days 30
  burn rebuild --reclassify
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
    case 'content':
      return runContent(args);
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
