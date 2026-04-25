import {
  createCurrentBlockTool,
  createSessionCostTool,
  startStdioServer,
} from '@relayburn/mcp';

import { ingestAll } from '../ingest.js';
import type { ParsedArgs } from '../args.js';

const MCP_HELP = `burn mcp-server — stdio MCP server exposing read-only ledger queries

Usage:
  burn mcp-server [--session-id <uuid>]

Registers tools for in-session self-query by an agent that was spawned with
this server attached via Claude Code's --mcp-config (see buildMcpConfig in
@relayburn/mcp). Tools default to the session id baked into the command line.

Tools:
  burn__sessionCost   { sessionId? } → total USD / tokens / turns / models
  burn__currentBlock  { sessionId? } → 5-hour OAuth window % + local burn rate forecast
`;

export async function runMcpServer(args: ParsedArgs): Promise<number> {
  if (args.positional[0] === 'help' || args.flags['help'] === true) {
    process.stdout.write(MCP_HELP);
    return 0;
  }
  const defaultSessionId =
    typeof args.flags['session-id'] === 'string' ? args.flags['session-id'] : undefined;

  // Sweep new turns into the ledger before serving so the first tool call
  // doesn't return zeros for a session that's already had activity. Cheap on
  // a steady-state ledger thanks to the cursor index.
  try {
    await ingestAll();
  } catch (err) {
    // Don't fail server startup on a partial ingest — let the tools handle
    // missing data gracefully. Log to stderr (which Claude Code surfaces in
    // the MCP server log) so it's visible if something is genuinely broken.
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[burn mcp-server] initial ingest failed: ${msg}\n`);
  }

  const tools = [
    createSessionCostTool({ defaultSessionId }),
    createCurrentBlockTool(),
  ];

  const server = startStdioServer({
    name: '@relayburn/mcp',
    version: getServerVersion(),
    tools,
    onLog: (msg) => process.stderr.write(`[burn mcp-server] ${msg}\n`),
  });

  await server.done;
  return 0;
}

function getServerVersion(): string {
  // Hardcoded to track the package's published version. Updated alongside
  // the CHANGELOG when a release ships. Keeping it inline avoids a runtime
  // package.json read which would complicate publish-time bundling.
  return '0.11.0';
}
