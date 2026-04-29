import { strict as assert } from 'node:assert';
import { mkdtemp, rm, utimes } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { afterEach, beforeEach, describe, it } from 'node:test';

import { appendContent, appendToolResultEvents, appendTurns, stamp } from '@relayburn/ledger';
import type { ToolResultEventRecord, TurnRecord } from '@relayburn/reader';

import { runState } from './state.js';

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
    code = await runState({ flags, tags: {}, positional, passthrough: [] });
  } finally {
    process.stdout.write = origStdout;
    process.stderr.write = origStderr;
  }
  return { stdout, stderr, code };
}

describe('burn state CLI', () => {
  let tmp: string;
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];
  const originalForce = process.env['RELAYBURN_PRUNE_FORCE'];

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-state-cli-'));
    process.env['RELAYBURN_HOME'] = tmp;
    delete process.env['RELAYBURN_CONTENT_STORE'];
    delete process.env['RELAYBURN_PRUNE_FORCE'];
  });

  afterEach(async () => {
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalStore !== undefined) process.env['RELAYBURN_CONTENT_STORE'] = originalStore;
    else delete process.env['RELAYBURN_CONTENT_STORE'];
    if (originalForce !== undefined) process.env['RELAYBURN_PRUNE_FORCE'] = originalForce;
    else delete process.env['RELAYBURN_PRUNE_FORCE'];
    await rm(tmp, { recursive: true, force: true });
  });

  it('state reports archive "not built yet" before the first build', async () => {
    const out = await captureRun({}, []);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /not built yet/);
    assert.match(out.stdout, /index:/);
    assert.match(out.stdout, /content:/);
    assert.match(out.stdout, /classifier:/);
    assert.match(out.stdout, /archive:/);
  });

  it('state status --json includes all derived artifacts', async () => {
    const out = await captureRun({ json: true }, ['status']);
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as {
      archive: unknown;
      index: unknown;
      content: unknown;
      classifier: unknown;
    };
    assert.ok(parsed.archive);
    assert.ok(parsed.index);
    assert.ok(parsed.content);
    assert.ok(parsed.classifier);
  });

  it('state rebuild archive materializes the ledger and status reflects row counts', async () => {
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

    const buildOut = await captureRun({}, ['rebuild', 'archive']);
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

  it('state rebuild archive --full is idempotent against the same ledger', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-rb', messageId: 'rb-1' })]);
    await captureRun({}, ['rebuild', 'archive']);
    const before = await captureRun({ json: true }, ['status']);
    const beforeStatus = JSON.parse(before.stdout) as {
      archive: { rowCounts: { turns: number } };
    };

    const rebuildOut = await captureRun({ full: true }, ['rebuild', 'archive']);
    assert.equal(rebuildOut.code, 0);
    assert.match(rebuildOut.stdout, /rebuilt archive/);

    const after = await captureRun({ json: true }, ['status']);
    const afterStatus = JSON.parse(after.stdout) as {
      archive: { rowCounts: { turns: number } };
    };
    assert.equal(afterStatus.archive.rowCounts.turns, beforeStatus.archive.rowCounts.turns);
  });

  it('state rebuild archive --vacuum on a missing archive prints a hint and exits 0', async () => {
    const out = await captureRun({ vacuum: true }, ['rebuild', 'archive']);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /no archive/);
    assert.match(out.stdout, /burn state rebuild archive/);
  });

  it('state rebuild archive vacuum --json returns the archive vacuum result', async () => {
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
    await captureRun({}, ['rebuild', 'archive']);

    const out = await captureRun({ json: true }, ['rebuild', 'archive', 'vacuum']);
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

  it('state status --json surfaces tool_result_events row count', async () => {
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
    await captureRun({}, ['rebuild', 'archive']);

    const statusOut = await captureRun({ json: true }, ['status']);
    assert.equal(statusOut.code, 0);
    const parsed = JSON.parse(statusOut.stdout) as {
      archive: { rowCounts: { toolResultEvents: number } };
    };
    assert.equal(parsed.archive.rowCounts.toolResultEvents, 1);

    const human = await captureRun({}, ['status']);
    assert.match(human.stdout, /tool_result_events:\s+1/);
  });

  it('state status --json surfaces a fidelity histogram on turns (#110)', async () => {
    // Use distinctive ts/usage so the writer's content-fingerprint dedup
    // doesn't collide with prior CLI tests in the same module (the in-memory
    // index cache is module-scoped - see #110 / writer.ts).
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
    await captureRun({}, ['rebuild', 'archive']);
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

  it('state rebuild with no target prints help and exits non-zero', async () => {
    const out = await captureRun({}, ['rebuild']);
    assert.equal(out.code, 2);
    assert.match(out.stderr, /missing target/);
    assert.match(out.stderr, /burn state rebuild/);
  });

  it('state rebuild index refreshes ledger indexes', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-index', messageId: 'index-1' })]);
    const out = await captureRun({}, ['rebuild', 'index']);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /rebuilt ledger index/);
  });

  it('state rebuild with unknown target exits non-zero', async () => {
    const out = await captureRun({}, ['rebuild', 'nope']);
    assert.notEqual(out.code, 0);
    assert.match(out.stderr, /unknown target/);
  });

  it('state with unknown subcommand exits non-zero', async () => {
    const out = await captureRun({}, ['nope']);
    assert.notEqual(out.code, 0);
    assert.match(out.stderr, /unknown subcommand/);
  });

  it('state prune returns non-zero and prints help on invalid --days instead of throwing', async () => {
    const out = await captureRun({ days: 'not-a-number' }, ['prune']);
    assert.equal(out.code, 2);
    assert.match(out.stderr, /invalid --days value/);
  });

  it('state prune accepts numeric --days and exits 0', async () => {
    const out = await captureRun({ days: '30' }, ['prune']);
    assert.equal(out.code, 0);
  });

  it('state prune accepts forever and exits 0', async () => {
    const out = await captureRun({ days: 'forever' }, ['prune']);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /retention=forever/);
  });

  it('state prune --force bypasses the recoverable check and deletes per retention', async () => {
    await appendContent([
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-force',
        messageId: 'm',
        ts: '2026-04-20T00:00:00.000Z',
        role: 'assistant',
        kind: 'text',
        text: 'hi',
      },
    ]);
    const sidecar = path.join(tmp, 'content', 's-force.jsonl');
    const longAgo = new Date(Date.now() - 120 * 24 * 60 * 60 * 1000);
    await utimes(sidecar, longAgo, longAgo);

    const out = await captureRun({ days: '90', force: true }, ['prune']);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /pruned 1 content file/);
    assert.doesNotMatch(out.stdout, /kept .* recoverable/);
  });

  it('RELAYBURN_PRUNE_FORCE=1 env var bypasses the recoverable check', async () => {
    await appendContent([
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-env-force',
        messageId: 'm',
        ts: '2026-04-20T00:00:00.000Z',
        role: 'assistant',
        kind: 'text',
        text: 'hi',
      },
    ]);
    const sidecar = path.join(tmp, 'content', 's-env-force.jsonl');
    const longAgo = new Date(Date.now() - 120 * 24 * 60 * 60 * 1000);
    await utimes(sidecar, longAgo, longAgo);

    process.env['RELAYBURN_PRUNE_FORCE'] = '1';
    const out = await captureRun({ days: '90' }, ['prune']);
    assert.equal(out.code, 0);
    assert.match(out.stdout, /pruned 1 content file/);
    assert.doesNotMatch(out.stdout, /kept .* recoverable/);
  });
});
