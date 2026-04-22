#!/usr/bin/env node
import { parseArgs } from './args.js';
import { runByTool } from './commands/by-tool.js';
import { runClaudeWrapper } from './commands/claude.js';
import { runCodexWrapper } from './commands/codex.js';
import { runOpencodeWrapper } from './commands/opencode.js';
import { runRebuildIndex } from './commands/rebuild-index.js';
import { runSummary } from './commands/summary.js';

const HELP = `burn — token usage & cost attribution for agent CLIs

Usage:
  burn summary       [--since 7d] [--project <path>] [--session <id>] [--workflow <id>] [--agent <id>]
  burn by-tool       [--since 7d] [--project <path>] [--session <id>]
  burn claude        [--tag k=v ...] [-- <claude args>]
  burn codex         [--tag k=v ...] [-- <codex args>]
  burn opencode      [--tag k=v ...] [-- <opencode args>]
  burn rebuild-index

Examples:
  burn summary --since 24h
  burn by-tool --since 7d
  burn claude   --tag workflow=refactor -- --resume
  burn codex    --tag workflow=refactor
  burn opencode --tag workflow=refactor
  burn rebuild-index
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
      return runSummary(args);
    case 'by-tool':
      return runByTool(args);
    case 'claude':
      return runClaudeWrapper(args);
    case 'codex':
      return runCodexWrapper(args);
    case 'opencode':
      return runOpencodeWrapper(args);
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
