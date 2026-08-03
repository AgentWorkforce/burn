import { strict as assert } from 'node:assert';
import { PassThrough } from 'node:stream';
import { describe, it } from 'node:test';

import { startStdioServer } from './server.js';
import { createCompareTool } from './tools/compare.js';
import { createFingerprintTool } from './tools/fingerprint.js';
import { createHotspotsTool } from './tools/hotspots.js';
import { createOverheadTrimTool } from './tools/overhead-trim.js';
import { createOverheadTool } from './tools/overhead.js';
import { createSessionCostTool } from './tools/session-cost.js';
import { createSummaryTool } from './tools/summary.js';

interface JsonRpcResponse {
  jsonrpc: '2.0';
  id: number | string | null;
  result?: {
    content?: { type: string; text: string }[];
    structuredContent?: unknown;
    tools?: { name: string; inputSchema: unknown }[];
    isError?: boolean;
  };
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
        if (trimmed) lines.push(JSON.parse(trimmed) as JsonRpcResponse);
      }
    });
    stream.on('end', () => resolve(lines));
  });
}

function send(stream: PassThrough, msg: unknown): void {
  stream.write(JSON.stringify(msg) + '\n');
}

describe('end-to-end: read tool catalog over stdio', () => {
  it('lists and invokes every tool, returning fixture results as structured content', async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const responses = collectResponses(output);
    const tools = [
      createSessionCostTool({
        defaultSessionId: 'S',
        sessionCost: async (opts) => ({ sessionId: opts.session ?? null, totalUSD: 3, totalTokens: 100, turnCount: 1, models: ['a'] }),
      }),
      createFingerprintTool({ fingerprint: async () => ({ fingerprint: '1:2:3' }) }),
      createSummaryTool({
        defaultSessionId: 'S',
        summary: async () => ({ totalTokens: 100, totalCost: 3, turnCount: 1, byTool: [], byModel: [] }),
      }),
      createHotspotsTool({
        defaultSessionId: 'S',
        hotspots: async () => ({ kind: 'findings', findings: [], summary: { fixture: true } }),
      }),
      createOverheadTool({
        overhead: async () => ({ project: '/fixture', files: [], perFile: [], grandTotal: 0 }),
      }),
      createOverheadTrimTool({
        overheadTrim: async () => ({
          project: '/fixture', since: '24h', recommendations: [],
          summary: { filesAnalyzed: 0, filesWithRecommendations: 0, totalRecommendations: 0, totalProjectedSavingsPerSession: 0, totalProjectedSavingsAcrossWindow: 0 },
        }),
      }),
      createCompareTool({
        compare: async (opts) => ({
          analyzedTurns: 0, minSample: 1, models: opts.models, categories: [], totals: {}, cells: [],
          fidelity: {
            minimum: 'partial',
            excluded: { total: 0, aggregateOnly: 0, costOnly: 0, partial: 0, usageOnly: 0 },
            summary: { total: 0, byClass: { full: 0, 'usage-only': 0, 'aggregate-only': 0, 'cost-only': 0, partial: 0 }, unknown: 0, missingCoverage: {} },
          },
        }),
      }),
    ];
    const server = startStdioServer({ name: '@relayburn/mcp', version: '0.0.1', tools, input, output });

    send(input, { jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-03-26' } });
    send(input, { jsonrpc: '2.0', id: 2, method: 'tools/list' });
    const calls: Array<[number, string, Record<string, unknown>]> = [
      [3, 'burn__sessionCost', {}],
      [4, 'burn__fingerprint', {}],
      [5, 'burn__summary', {}],
      [6, 'burn__hotspots', { groupBy: 'findings' }],
      [7, 'burn__overhead', {}],
      [8, 'burn__overheadTrim', { top: 1 }],
      [9, 'burn__compare', { models: ['a', 'b'] }],
    ];
    for (const [id, name, args] of calls) {
      send(input, { jsonrpc: '2.0', id, method: 'tools/call', params: { name, arguments: args } });
    }
    input.end();
    await server.done;
    await new Promise<void>((resolve) => setImmediate(resolve));
    output.end();

    const all = await responses;
    const listed = all.find((r) => r.id === 2)?.result?.tools ?? [];
    assert.deepEqual(listed.map((tool) => tool.name), tools.map((tool) => tool.name));
    for (const [id] of calls) {
      const response = all.find((r) => r.id === id);
      assert.ok(response?.result?.structuredContent, `call ${id} returned structured content`);
      assert.equal(response.result.isError, undefined);
    }
  });

  it('enforces advertised constraints over real stdio calls', async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const responses = collectResponses(output);
    const tools = [
      createSummaryTool({ defaultSessionId: undefined }),
      createHotspotsTool({ defaultSessionId: undefined }),
      createOverheadTrimTool(),
      createCompareTool(),
    ];
    const server = startStdioServer({ name: '@relayburn/mcp', version: '0.0.1', tools, input, output });
    const invalid = [
      [10, 'burn__hotspots', { groupBy: 'bogus' }],
      [11, 'burn__overheadTrim', { top: -1 }],
      [12, 'burn__compare', { models: ['only-one'] }],
      [13, 'burn__summary', { unknown: true }],
    ] as const;
    for (const [id, name, args] of invalid) {
      send(input, { jsonrpc: '2.0', id, method: 'tools/call', params: { name, arguments: args } });
    }
    input.end();
    await server.done;
    await new Promise<void>((resolve) => setImmediate(resolve));
    output.end();

    const all = await responses;
    for (const [id] of invalid) {
      const result = all.find((r) => r.id === id)?.result;
      assert.equal(result?.isError, true, `invalid call ${id} returned an MCP tool error`);
      assert.ok(result?.content?.[0]?.text);
      assert.equal(result?.structuredContent, undefined);
    }
  });
});
