import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import { appendToolResultEvents, appendTurns, stamp } from '@relayburn/ledger';
import type { ToolResultEventRecord, TurnRecord } from '@relayburn/reader';

import { runRebuild } from './rebuild.js';

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
    code = await runRebuild({ flags, tags: {}, positional, passthrough: [] });
  } finally {
    process.stdout.write = origStdout;
    process.stderr.write = origStderr;
  }
  return { stdout, stderr, code };
}

describe('burn rebuild archive/status CLI', () => {
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

  it('rebuild status reports archive "not built yet" before the first build', async () => {
    const out = await captureRun({}, ['status']);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /not built yet/);
    assert.match(out.stdout, /index:/);
    assert.match(out.stdout, /content:/);
    assert.match(out.stdout, /classifier:/);
    assert.match(out.stdout, /archive:/);
  });

  it('rebuild archive materializes the ledger and status reflects row counts', async () => {
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

    const buildOut = await captureRun({}, ['archive']);
    assert.equal(buildOut.code, 0);
    assert.match(buildOut.stdout, /archive build complete/);
    assert.match(buildOut.stdout, /2 turns applied/);

    const statusOut = await captureRun({ json: true }, ['status']);
    assert.equal(statusOut.code, 0);
    const parsed = JSON.parse(statusOut.stdout) as {
      archive: {
        exists: boolean;
        upToDate: boolean;
        rowCounts: { sessions: number; turns: number };
      };
      index: unknown;
      content: unknown;
      classifier: { turns: number; classified: number; missing: number };
    };
    assert.equal(parsed.archive.exists, true);
    assert.equal(parsed.archive.upToDate, true);
    assert.equal(parsed.archive.rowCounts.turns, 2);
    assert.equal(parsed.archive.rowCounts.sessions, 1);
    assert.ok(parsed.index);
    assert.ok(parsed.content);
    assert.equal(parsed.classifier.turns, 2);
    assert.equal(parsed.classifier.classified, 0);
    assert.equal(parsed.classifier.missing, 2);
  });

  it('rebuild archive --full is idempotent against the same ledger', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-rb', messageId: 'rb-1' })]);
    await captureRun({}, ['archive']);
    const before = await captureRun({ json: true }, ['status']);
    const beforeStatus = JSON.parse(before.stdout) as {
      archive: { rowCounts: { turns: number } };
    };

    const rebuildOut = await captureRun({ full: true }, ['archive']);
    assert.equal(rebuildOut.code, 0);
    assert.match(rebuildOut.stdout, /rebuilt archive/);

    const after = await captureRun({ json: true }, ['status']);
    const afterStatus = JSON.parse(after.stdout) as {
      archive: { rowCounts: { turns: number } };
    };
    assert.equal(afterStatus.archive.rowCounts.turns, beforeStatus.archive.rowCounts.turns);
  });

  it('rebuild archive --vacuum on a missing archive prints a hint and exits 0', async () => {
    const out = await captureRun({ vacuum: true }, ['archive']);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /no archive/);
    assert.match(out.stdout, /burn rebuild archive/);
  });

  it('rebuild archive vacuum --json returns the archive vacuum result', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-vac-cli',
        messageId: 'vac-cli-1',
        ts: '2026-04-23T00:00:00.000Z',
        usage: {
          input: 123,
          output: 45,
          reasoning: 0,
          cacheRead: 678,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
    ]);
    await captureRun({}, ['archive']);

    const out = await captureRun({ json: true }, ['archive', 'vacuum']);
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as {
      existed: boolean;
      beforeBytes: number;
      afterBytes: number;
      reclaimedBytes: number;
      archivePath: string;
    };
    assert.equal(parsed.existed, true);
    assert.equal(typeof parsed.beforeBytes, 'number');
    assert.equal(typeof parsed.afterBytes, 'number');
    assert.equal(typeof parsed.reclaimedBytes, 'number');
    assert.match(parsed.archivePath, /archive\.sqlite$/);
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
    await captureRun({}, ['archive']);

    const statusOut = await captureRun({ json: true }, ['status']);
    assert.equal(statusOut.code, 0);
    const parsed = JSON.parse(statusOut.stdout) as {
      archive: { rowCounts: { toolResultEvents: number } };
    };
    assert.equal(parsed.archive.rowCounts.toolResultEvents, 1);

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
    await captureRun({}, ['archive']);
    const statusOut = await captureRun({ json: true }, ['status']);
    assert.equal(statusOut.code, 0);
    const parsed = JSON.parse(statusOut.stdout) as {
      archive: {
        fidelityHistogram: Record<string, number>;
        rowCounts: { turns: number };
      };
    };
    assert.equal(parsed.archive.rowCounts.turns, 2);
    assert.equal(parsed.archive.fidelityHistogram['full'], 1);
    assert.equal(parsed.archive.fidelityHistogram['unknown'], 1);
  });

  it('rebuild with no target prints help and exits 0', async () => {
    const out = await captureRun({}, []);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /burn rebuild/);
  });

  it('rebuild with unknown target exits non-zero', async () => {
    const out = await captureRun({}, ['nope']);
    assert.notEqual(out.code, 0);
    assert.match(out.stderr, /unknown target/);
  });
});
