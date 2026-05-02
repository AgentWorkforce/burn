import { buildArchive } from '@relayburn/ledger';
import {
  createSessionCostTool,
  startStdioServer,
} from '@relayburn/mcp';

import { ingestAll } from '@relayburn/ingest';
import type { ParsedArgs } from '../args.js';
import { withProgress } from '../progress.js';

const MCP_HELP = `burn mcp-server — stdio MCP server exposing read-only ledger queries

Usage:
  burn mcp-server [--session-id <uuid>]

Registers tools for in-session self-query by an agent that was spawned with
this server attached via Claude Code's --mcp-config (see buildMcpConfig in
@relayburn/mcp). Tools default to the session id baked into the command line.

Tools:
  burn__sessionCost   { sessionId? } → total USD / tokens / turns / models
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
    await withProgress('mcp server initial ingest', (task) =>
      ingestAll({
        onProgress: (message) => task.update(`ingest: ${message}`),
        onWarn: (body) => task.warn(body),
      }),
    );
  } catch (err) {
    // Don't fail server startup on a partial ingest — let the tools handle
    // missing data gracefully. Log to stderr (which Claude Code surfaces in
    // the MCP server log) so it's visible if something is genuinely broken.
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[burn mcp-server] initial ingest failed: ${msg}\n`);
  }

  // Apply any ledger tail not yet materialized into the archive so the first
  // tool call hits SQL on the hot path instead of re-walking the ledger.
  // Idempotent and incremental — a no-op when nothing has changed since the
  // last build.
  try {
    await withProgress('mcp server archive warmup', async (task) => {
      const result = await buildArchive();
      task.succeed(
        `mcp archive warmup: ${result.turnsApplied} turn` +
          `${result.turnsApplied === 1 ? '' : 's'} applied`,
      );
    });
  } catch (err) {
    // Tools fall back to `queryAll` if the archive is unavailable; log the
    // build failure so an operator can spot a persistent breakage but don't
    // refuse to serve.
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[burn mcp-server] initial archive build failed: ${msg}\n`);
  }

  const log = (msg: string): void => {
    process.stderr.write(`[burn mcp-server] ${msg}\n`);
  };

  const tools = [
    createSessionCostTool({ defaultSessionId, onLog: log }),
  ];

  const server = startStdioServer({
    name: '@relayburn/mcp',
    version: getServerVersion(),
    tools,
    onLog: log,
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
