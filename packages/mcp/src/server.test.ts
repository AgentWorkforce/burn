import { strict as assert } from 'node:assert';
import { PassThrough } from 'node:stream';
import { describe, it } from 'node:test';

import { startStdioServer } from './server.js';
import type { ToolDefinition } from './types.js';

interface JsonRpcResponse {
  jsonrpc: '2.0';
  id: number | string | null;
  result?: unknown;
  error?: { code: number; message: string };
}

function collectResponses(stream: PassThrough): Promise<JsonRpcResponse[]> {
  const lines: JsonRpcResponse[] = [];
  let buf = '';
  return new Promise((resolve) => {
    stream.on('data', (chunk: Buffer | string) => {
      buf += typeof chunk === 'string' ? chunk : chunk.toString('utf8');
      const parts = buf.split('\n');
      buf = parts.pop() ?? '';
      for (const p of parts) {
        const trimmed = p.trim();
        if (!trimmed) continue;
        lines.push(JSON.parse(trimmed) as JsonRpcResponse);
      }
    });
    stream.on('end', () => resolve(lines));
  });
}

function send(stream: PassThrough, msg: unknown): void {
  stream.write(JSON.stringify(msg) + '\n');
}

const ECHO_TOOL: ToolDefinition = {
  name: 'echo',
  description: 'echoes its input',
  inputSchema: { type: 'object', additionalProperties: true },
  handler: (args) => args,
};

describe('startStdioServer', () => {
  it('responds to initialize with serverInfo and capabilities', async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const responses = collectResponses(output);
    const server = startStdioServer({
      name: 'test',
      version: '0.0.1',
      tools: [ECHO_TOOL],
      input,
      output,
    });
    send(input, {
      jsonrpc: '2.0',
      id: 1,
      method: 'initialize',
      params: { protocolVersion: '2025-03-26' },
    });
    input.end();
    await server.done;
    output.end();
    const all = await responses;
    assert.equal(all.length, 1);
    const resp = all[0]!;
    assert.equal(resp.id, 1);
    const r = resp.result as {
      protocolVersion: string;
      capabilities: { tools: unknown };
      serverInfo: { name: string; version: string };
    };
    assert.equal(r.protocolVersion, '2025-03-26');
    assert.equal(r.serverInfo.name, 'test');
    assert.equal(r.serverInfo.version, '0.0.1');
    assert.ok(r.capabilities.tools);
  });

  it('lists registered tools via tools/list', async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const responses = collectResponses(output);
    const server = startStdioServer({
      name: 'test',
      version: '0.0.1',
      tools: [ECHO_TOOL],
      input,
      output,
    });
    send(input, { jsonrpc: '2.0', id: 1, method: 'tools/list' });
    input.end();
    await server.done;
    output.end();
    const all = await responses;
    const r = all[0]!.result as { tools: { name: string }[] };
    assert.equal(r.tools.length, 1);
    assert.equal(r.tools[0]!.name, 'echo');
  });

  it('dispatches tools/call to the tool handler and returns text content', async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const responses = collectResponses(output);
    const server = startStdioServer({
      name: 'test',
      version: '0.0.1',
      tools: [ECHO_TOOL],
      input,
      output,
    });
    send(input, {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: { name: 'echo', arguments: { hello: 'world' } },
    });
    input.end();
    await server.done;
    output.end();
    const all = await responses;
    const r = all[0]!.result as {
      content: { type: string; text: string }[];
      structuredContent?: Record<string, unknown>;
    };
    assert.equal(r.content[0]!.type, 'text');
    assert.deepEqual(JSON.parse(r.content[0]!.text), { hello: 'world' });
    assert.deepEqual(r.structuredContent, { hello: 'world' });
  });

  it('returns isError content (not a JSON-RPC error) when a tool throws', async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const responses = collectResponses(output);
    const failingTool: ToolDefinition = {
      name: 'fail',
      description: '',
      inputSchema: { type: 'object' },
      handler: () => {
        throw new Error('boom');
      },
    };
    const server = startStdioServer({
      name: 'test',
      version: '0.0.1',
      tools: [failingTool],
      input,
      output,
    });
    send(input, {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: { name: 'fail', arguments: {} },
    });
    input.end();
    await server.done;
    output.end();
    const all = await responses;
    const r = all[0]!.result as {
      content: { type: string; text: string }[];
      isError?: boolean;
    };
    assert.equal(r.isError, true);
    assert.match(r.content[0]!.text, /boom/);
  });

  it('returns method not found for unknown methods', async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const responses = collectResponses(output);
    const server = startStdioServer({
      name: 'test',
      version: '0.0.1',
      tools: [ECHO_TOOL],
      input,
      output,
    });
    send(input, { jsonrpc: '2.0', id: 1, method: 'nope/whatever' });
    input.end();
    await server.done;
    output.end();
    const all = await responses;
    const err = all[0]!.error;
    assert.ok(err);
    assert.equal(err.code, -32601);
  });

  it('ignores notifications (messages with no id) without crashing', async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const responses = collectResponses(output);
    const server = startStdioServer({
      name: 'test',
      version: '0.0.1',
      tools: [ECHO_TOOL],
      input,
      output,
    });
    send(input, { jsonrpc: '2.0', method: 'notifications/initialized' });
    send(input, { jsonrpc: '2.0', id: 7, method: 'tools/list' });
    input.end();
    await server.done;
    output.end();
    const all = await responses;
    // Only the tools/list response should appear; the notification is dropped.
    assert.equal(all.length, 1);
    assert.equal(all[0]!.id, 7);
  });
});
