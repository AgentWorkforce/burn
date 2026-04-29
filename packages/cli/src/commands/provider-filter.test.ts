import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { afterEach, beforeEach, describe, it } from 'node:test';

import { appendTurns, queryAll } from '@relayburn/ledger';
import type { TurnRecord } from '@relayburn/reader';

import { runSummary } from './summary.js';
import { runHotspots } from './hotspots.js';
import type { ParsedArgs } from '../args.js';

async function captureStdio<T>(
  fn: () => Promise<T>,
): Promise<{ result: T; stdout: string; stderr: string }> {
  let stdout = '';
  let stderr = '';
  const origOut = process.stdout.write.bind(process.stdout);
  const origErr = process.stderr.write.bind(process.stderr);
  process.stdout.write = ((c: string | Uint8Array) => {
    stdout += typeof c === 'string' ? c : Buffer.from(c).toString('utf8');
    return true;
  }) as typeof process.stdout.write;
  process.stderr.write = ((c: string | Uint8Array) => {
    stderr += typeof c === 'string' ? c : Buffer.from(c).toString('utf8');
    return true;
  }) as typeof process.stderr.write;
  try {
    const result = await fn();
    return { result, stdout, stderr };
  } finally {
    process.stdout.write = origOut;
    process.stderr.write = origErr;
  }
}

function args(flags: Record<string, string | true> = {}): ParsedArgs {
  return { flags, tags: {}, positional: [], passthrough: [] };
}

function turn(overrides: Partial<TurnRecord> = {}): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-provider',
    messageId: 'm-provider',
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    usage: {
      input: 1_000_000,
      output: 1_000_000,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    ...overrides,
  };
}

describe('provider filters', () => {
  let tmpHome: string | undefined;
  let tmpRelay: string | undefined;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-provider-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-provider-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
  });

  afterEach(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (tmpHome) await rm(tmpHome, { recursive: true, force: true });
    if (tmpRelay) await rm(tmpRelay, { recursive: true, force: true });
    tmpHome = undefined;
    tmpRelay = undefined;
  });

  it('groups hf:-routed turns under synthetic with non-zero cost and keeps the ledger model raw', async () => {
    await appendTurns([
      turn({
        sessionId: 's-synth-summary',
        messageId: 'm-synth-summary',
        model: 'hf:deepseek-ai/deepseek-r1',
      }),
    ]);

    const { result, stdout } = await captureStdio(() =>
      runSummary(args({ 'by-provider': true, provider: 'synthetic', json: true })),
    );
    assert.equal(result, 0);

    const parsed = JSON.parse(stdout);
    assert.equal(parsed.turns, 1);
    assert.equal(parsed.byProvider.length, 1);
    assert.equal(parsed.byProvider[0].provider, 'synthetic');
    assert.ok(parsed.byProvider[0].cost.total > 0);

    const raw = await queryAll();
    assert.equal(raw.length, 1);
    assert.equal(raw[0]!.model, 'hf:deepseek-ai/deepseek-r1');
  });

  it('applies --provider synthetic to summary --by-tool', async () => {
    await appendTurns([
      turn({
        sessionId: 's-synth-tool',
        messageId: 'm-synth-tool',
        model: 'hf:deepseek-ai/deepseek-r1',
        toolCalls: [{ id: 'tc-synth', name: 'Read', target: 'synthetic.ts', argsHash: 'abc' }],
      }),
      turn({
        sessionId: 's-anthropic-tool',
        messageId: 'm-anthropic-tool',
        ts: '2026-04-20T00:00:01.000Z',
        toolCalls: [{ id: 'tc-anthropic', name: 'Bash', target: 'npm test', argsHash: 'def' }],
      }),
    ]);

    const { result, stdout } = await captureStdio(() =>
      runSummary(args({ 'by-tool': true, provider: 'synthetic' })),
    );
    assert.equal(result, 0);
    assert.match(stdout, /turns analyzed: 1/);
    assert.match(stdout, /Read/);
    assert.doesNotMatch(stdout, /Bash/);
  });

  it('applies --provider synthetic to hotspots', async () => {
    await appendTurns([
      turn({
        sessionId: 's-synth-hotspots',
        messageId: 'm-synth-hotspots',
        model: 'hf:deepseek-ai/deepseek-r1',
        toolCalls: [{ id: 'tc-synth-hotspots', name: 'Bash', target: 'pnpm test', argsHash: 'aaa' }],
      }),
      turn({
        sessionId: 's-anthropic-hotspots',
        messageId: 'm-anthropic-hotspots',
        ts: '2026-04-20T00:00:02.000Z',
        toolCalls: [{ id: 'tc-anthropic-hotspots', name: 'Read', target: 'anthropic.ts', argsHash: 'bbb' }],
      }),
    ]);

    const { result, stdout } = await captureStdio(() =>
      runHotspots(args({ provider: 'synthetic', json: true })),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.equal(parsed.turnsAnalyzed, 1);
    assert.equal(parsed.sessions.length, 1);
    assert.equal(parsed.sessions[0].sessionId, 's-synth-hotspots');
    assert.ok(parsed.grandTotal > 0);
  });
});
