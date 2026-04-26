import { strict as assert } from 'node:assert';
import { PassThrough } from 'node:stream';
import { describe, it } from 'node:test';

import type { PricingTable } from '@relayburn/analyze';
import type { EnrichedTurn } from '@relayburn/ledger';

import { startStdioServer } from './server.js';
import { createSessionCostTool } from './tools/session-cost.js';

interface JsonRpcResponse {
  jsonrpc: '2.0';
  id: number | string | null;
  result?: { content: { type: string; text: string }[]; structuredContent?: unknown };
  error?: { code: number; message: string };
}

const PRICING: PricingTable = {
  'claude-sonnet-4-5': {
    input: 3,
    output: 15,
    cacheRead: 0.3,
    cacheWrite: 3.75,
    reasoningMode: 'same_as_output',
  },
};

function turn(): EnrichedTurn {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 'S',
    messageId: 'm1',
    turnIndex: 0,
    ts: '2026-04-24T10:00:00.000Z',
    model: 'claude-sonnet-4-5',
    usage: {
      input: 1_000_000,
      output: 0,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    enrichment: {},
  };
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

describe('end-to-end: spawn server, call burn__sessionCost, verify cost', () => {
  it('returns the same total a direct cost computation would', async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const responses = collectResponses(output);
    const sessionCost = createSessionCostTool({
      defaultSessionId: 'S',
      queryTurns: async () => [turn()],
      loadPricing: async () => PRICING,
    });
    const server = startStdioServer({
      name: '@relayburn/mcp',
      version: '0.0.1',
      tools: [sessionCost],
      input,
      output,
    });

    // Real client handshake order: initialize → tools/list → tools/call.
    send(input, {
      jsonrpc: '2.0',
      id: 1,
      method: 'initialize',
      params: { protocolVersion: '2025-03-26' },
    });
    send(input, { jsonrpc: '2.0', id: 2, method: 'tools/list' });
    send(input, {
      jsonrpc: '2.0',
      id: 3,
      method: 'tools/call',
      params: { name: 'burn__sessionCost', arguments: {} },
    });
    input.end();
    await server.done;
    output.end();

    const all = await responses;
    assert.equal(all.length, 3);
    const callResp = all.find((r) => r.id === 3)!;
    assert.ok(callResp.result, 'tool call returned a result');
    const text = callResp.result.content[0]!.text;
    const parsed = JSON.parse(text) as { totalUSD: number; turnCount: number; sessionId: string };
    // 1M input @ $3/M = $3, matching what `burn summary --session S` would
    // report for the same turns and pricing.
    assert.equal(parsed.totalUSD, 3);
    assert.equal(parsed.turnCount, 1);
    assert.equal(parsed.sessionId, 'S');
  });
});
