import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import { appendToolResultEvents, appendTurns, stamp } from '@relayburn/ledger';
import type { ToolResultEventRecord, TurnRecord } from '@relayburn/reader';

import { runArchive } from './archive.js';

function fakeTurn(overrides: Partial<TurnRecord> = {}): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    messageId: 'msg-1',
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    usage: {
      input: 100,
      output: 50,
      reasoning: 0,
      cacheRead: 1000,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    project: '/tmp/project',
    ...overrides,
  };
}

interface CapturedOutput {
  stdout: string;
  stderr: string;
  code: number;
}

async function captureRun(
  flags: Record<string, string | true>,
  positional: string[] = [],
): Promise<CapturedOutput> {
  const origStdout = process.stdout.write.bind(process.stdout);
  const origStderr = process.stderr.write.bind(process.stderr);
  let stdout = '';
  let stderr = '';
  process.stdout.write = ((chunk: string | Uint8Array): boolean => {
    stdout += typeof chunk === 'string' ? chunk : chunk.toString();
    return true;
  }) as typeof process.stdout.write;
  process.stderr.write = ((chunk: string | Uint8Array): boolean => {
    stderr += typeof chunk === 'string' ? chunk : chunk.toString();
    return true;
  }) as typeof process.stderr.write;
  let code: number;
  try {
    code = await runArchive({ flags, tags: {}, positional, passthrough: [] });
  } finally {
    process.stdout.write = origStdout;
    process.stderr.write = origStderr;
  }
  return { stdout, stderr, code };
}

describe('burn archive CLI', () => {
  let tmp: string;
  const originalRelay = process.env['RELAYBURN_HOME'];

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-archive-cli-'));
    process.env['RELAYBURN_HOME'] = tmp;
  });

  after(async () => {
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    await rm(tmp, { recursive: true, force: true });
  });

  it('archive status reports "not built yet" before the first build', async () => {
    const out = await captureRun({}, ['status']);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /not built yet/);
  });

  it('archive build materializes the ledger and status reflects row counts', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-cli', messageId: 'cli-1' }),
      fakeTurn({
        sessionId: 's-cli',
        messageId: 'cli-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    await stamp({ sessionId: 's-cli' }, { workflowId: 'wf-cli' });

    const buildOut = await captureRun({}, ['build']);
    assert.equal(buildOut.code, 0);
    assert.match(buildOut.stdout, /archive build complete/);
    assert.match(buildOut.stdout, /2 turns applied/);

    const statusOut = await captureRun({ json: true }, ['status']);
    assert.equal(statusOut.code, 0);
    const parsed = JSON.parse(statusOut.stdout) as {
      exists: boolean;
      upToDate: boolean;
      rowCounts: { sessions: number; turns: number };
    };
    assert.equal(parsed.exists, true);
    assert.equal(parsed.upToDate, true);
    assert.equal(parsed.rowCounts.turns, 2);
    assert.equal(parsed.rowCounts.sessions, 1);
  });

  it('archive rebuild is idempotent against the same ledger', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-rb', messageId: 'rb-1' })]);
    await captureRun({}, ['build']);
    const before = await captureRun({ json: true }, ['status']);
    const beforeStatus = JSON.parse(before.stdout) as {
      rowCounts: { turns: number };
    };

    const rebuildOut = await captureRun({}, ['rebuild']);
    assert.equal(rebuildOut.code, 0);
    assert.match(rebuildOut.stdout, /rebuilt archive/);

    const after = await captureRun({ json: true }, ['status']);
    const afterStatus = JSON.parse(after.stdout) as {
      rowCounts: { turns: number };
    };
    assert.equal(afterStatus.rowCounts.turns, beforeStatus.rowCounts.turns);
  });

  it('archive status --json surfaces tool_result_events row count', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-tre-cli', messageId: 'tre-cli-1' })]);
    const events: ToolResultEventRecord[] = [
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-tre-cli',
        messageId: 'tre-cli-1',
        toolUseId: 'tu-cli-1',
        callIndex: 0,
        eventIndex: 0,
        ts: '2026-04-20T00:00:01.000Z',
        status: 'completed',
        eventSource: 'tool_result',
        contentLength: 7,
        contentHash: 'cli-h1',
      },
    ];
    await appendToolResultEvents(events);
    await captureRun({}, ['build']);

    const statusOut = await captureRun({ json: true }, ['status']);
    assert.equal(statusOut.code, 0);
    const parsed = JSON.parse(statusOut.stdout) as {
      rowCounts: { toolResultEvents: number };
    };
    assert.equal(parsed.rowCounts.toolResultEvents, 1);

    const human = await captureRun({}, ['status']);
    assert.match(human.stdout, /tool_result_events:\s+1/);
  });

  it('archive status --json surfaces a fidelity histogram on turns (#110)', async () => {
    // Use distinctive ts/usage so the writer's content-fingerprint dedup
    // doesn't collide with prior CLI tests in the same module (the in-memory
    // index cache is module-scoped — see #110 / writer.ts).
    await appendTurns([
      fakeTurn({
        sessionId: 's-fid',
        messageId: 'fid-1',
        ts: '2026-04-22T00:00:00.000Z',
        usage: {
          input: 111,
          output: 51,
          reasoning: 0,
          cacheRead: 1001,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
        fidelity: {
          granularity: 'per-turn',
          coverage: {
            hasInputTokens: true,
            hasOutputTokens: true,
            hasReasoningTokens: false,
            hasCacheReadTokens: true,
            hasCacheCreateTokens: true,
            hasToolCalls: true,
            hasToolResultEvents: true,
            hasSessionRelationships: true,
            hasRawContent: true,
          },
          class: 'full',
        },
      }),
      // Older line — no fidelity at all.
      fakeTurn({
        sessionId: 's-fid',
        messageId: 'fid-2',
        turnIndex: 1,
        ts: '2026-04-22T00:01:00.000Z',
        usage: {
          input: 112,
          output: 52,
          reasoning: 0,
          cacheRead: 1002,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
    ]);
    await captureRun({}, ['build']);
    const statusOut = await captureRun({ json: true }, ['status']);
    assert.equal(statusOut.code, 0);
    const parsed = JSON.parse(statusOut.stdout) as {
      fidelityHistogram: Record<string, number>;
      rowCounts: { turns: number };
    };
    assert.equal(parsed.rowCounts.turns, 2);
    assert.equal(parsed.fidelityHistogram['full'], 1);
    assert.equal(parsed.fidelityHistogram['unknown'], 1);
  });

  it('archive with no subcommand prints help and exits 0', async () => {
    const out = await captureRun({}, []);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /burn archive/);
  });

  it('archive with unknown subcommand exits non-zero', async () => {
    const out = await captureRun({}, ['nope']);
    assert.notEqual(out.code, 0);
    assert.match(out.stderr, /unknown subcommand/);
  });
});
