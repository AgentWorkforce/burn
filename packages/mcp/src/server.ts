import { createInterface } from 'node:readline';
import { Readable, Writable } from 'node:stream';

import type { ToolDefinition } from './types.js';

// Bare-bones MCP (Model Context Protocol) server over stdio.
//
// MCP wraps JSON-RPC 2.0 with a small set of conventional methods. The three
// we need for a read-only, tools-only server are `initialize`, `tools/list`,
// and `tools/call`. We also accept the `notifications/initialized` and
// `notifications/cancelled` notifications (one-way, no response) so well-
// behaved clients don't see unexpected errors.
//
// Not using `@modelcontextprotocol/sdk` here for the same reason the rest of
// the repo avoids runtime deps — the on-wire shape is tiny and freezing a
// specific SDK version buys us nothing. If the protocol evolves, this module
// is localized enough to update in one place.

// Matches the latest protocol revision at the time this shipped. MCP clients
// negotiate version; we echo the client's version back when we can handle it
// (treat as a superset declaration), else fall back to this baseline.
const PROTOCOL_VERSION = '2025-03-26';

export interface StartStdioServerOptions {
  name: string;
  version: string;
  tools: ToolDefinition[];
  // Default I/O is stdio. Tests plug in fake streams.
  input?: Readable;
  output?: Writable;
  // Called for every log-worthy event. Defaults to a no-op so a running
  // server doesn't spam stderr and risk being interpreted by the client as
  // protocol noise. The CLI wrapper can pass a stderr-writer when desired.
  onLog?: (msg: string) => void;
}

interface JsonRpcRequest {
  jsonrpc: '2.0';
  id: number | string | null;
  method: string;
  params?: unknown;
}

interface JsonRpcNotification {
  jsonrpc: '2.0';
  method: string;
  params?: unknown;
}

interface JsonRpcSuccess {
  jsonrpc: '2.0';
  id: number | string | null;
  result: unknown;
}

interface JsonRpcError {
  jsonrpc: '2.0';
  id: number | string | null;
  error: { code: number; message: string; data?: unknown };
}

export interface RunningServer {
  // Resolves when stdin closes (client disconnected). Reject only on fatal
  // wiring errors (e.g. stream setup failure). Protocol-level errors are
  // delivered to the client inline and do not reject.
  done: Promise<void>;
}

export function startStdioServer(opts: StartStdioServerOptions): RunningServer {
  const input = opts.input ?? process.stdin;
  const output = opts.output ?? process.stdout;
  const log = opts.onLog ?? (() => {});

  const tools = new Map<string, ToolDefinition>();
  for (const t of opts.tools) tools.set(t.name, t);

  const rl = createInterface({ input, crlfDelay: Infinity });

  function send(payload: JsonRpcSuccess | JsonRpcError | JsonRpcNotification): void {
    const line = JSON.stringify(payload) + '\n';
    output.write(line);
  }

  function respondError(
    id: number | string | null,
    code: number,
    message: string,
    data?: unknown,
  ): void {
    const err: JsonRpcError['error'] = { code, message };
    if (data !== undefined) err.data = data;
    send({ jsonrpc: '2.0', id, error: err });
  }

  async function handleRequest(req: JsonRpcRequest): Promise<void> {
    const { id, method, params } = req;
    try {
      switch (method) {
        case 'initialize': {
          const p = (params ?? {}) as { protocolVersion?: string };
          const clientVersion = typeof p.protocolVersion === 'string' ? p.protocolVersion : undefined;
          send({
            jsonrpc: '2.0',
            id,
            result: {
              // Echo the client version if it's a known shape; otherwise
              // fall back to our compiled-in baseline. MCP clients tolerate
              // server-declared versions as long as they semver-adjacent.
              protocolVersion: clientVersion ?? PROTOCOL_VERSION,
              capabilities: { tools: { listChanged: false } },
              serverInfo: { name: opts.name, version: opts.version },
            },
          });
          return;
        }
        case 'ping': {
          send({ jsonrpc: '2.0', id, result: {} });
          return;
        }
        case 'tools/list': {
          send({
            jsonrpc: '2.0',
            id,
            result: {
              tools: opts.tools.map((t) => ({
                name: t.name,
                description: t.description,
                inputSchema: t.inputSchema,
              })),
            },
          });
          return;
        }
        case 'tools/call': {
          const p = (params ?? {}) as { name?: string; arguments?: unknown };
          if (!p.name) {
            respondError(id, -32602, 'tools/call requires a name');
            return;
          }
          const tool = tools.get(p.name);
          if (!tool) {
            respondError(id, -32601, `unknown tool: ${p.name}`);
            return;
          }
          try {
            const result = await tool.handler(
              (p.arguments ?? {}) as Record<string, unknown>,
            );
            send({
              jsonrpc: '2.0',
              id,
              result: {
                content: [
                  {
                    type: 'text',
                    text: JSON.stringify(result),
                  },
                ],
                structuredContent: result as Record<string, unknown>,
              },
            });
          } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            // Tool errors come back as a non-throwing result with isError
            // true so clients can display the failure without tearing the
            // session. This mirrors how the MCP spec recommends surfacing
            // per-tool failures — reserve JSON-RPC errors for protocol
            // problems, not "I looked but found nothing" / "bad args".
            send({
              jsonrpc: '2.0',
              id,
              result: {
                content: [{ type: 'text', text: msg }],
                isError: true,
              },
            });
          }
          return;
        }
        default:
          respondError(id, -32601, `method not found: ${method}`);
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      log(`server error handling ${method}: ${msg}`);
      respondError(id, -32603, `internal error: ${msg}`);
    }
  }

  rl.on('line', (line) => {
    const trimmed = line.trim();
    if (!trimmed) return;
    let parsed: unknown;
    try {
      parsed = JSON.parse(trimmed);
    } catch (err) {
      log(`parse error: ${(err as Error).message}`);
      respondError(null, -32700, 'parse error');
      return;
    }
    if (!parsed || typeof parsed !== 'object') {
      respondError(null, -32600, 'invalid request');
      return;
    }
    const obj = parsed as Record<string, unknown>;
    // Notifications have no id; don't respond. We don't act on any currently,
    // but swallowing them keeps the client happy.
    if (!('id' in obj)) return;
    void handleRequest(obj as unknown as JsonRpcRequest);
  });

  const done = new Promise<void>((resolve) => {
    rl.on('close', () => resolve());
  });
  return { done };
}
