import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, beforeEach, describe, it } from 'node:test';

import type { TurnRecord } from '@relayburn/reader';

import { __resetIndexCacheForTesting } from './index-sidecar.js';
import { appendTurns, stamp } from './writer.js';
import { buildArchive } from './archive.js';
import { queryTurnsFromArchive } from './archive-query.js';
import { queryAll } from './reader.js';

function fakeTurn(overrides: Partial<TurnRecord> = {}): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    messageId: 'm-1',
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

describe('queryTurnsFromArchive', () => {
  let tmpDir: string;
  const originalHome = process.env['RELAYBURN_HOME'];

  before(async () => {
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-archive-query-'));
  });

  beforeEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-archive-query-'));
    process.env['RELAYBURN_HOME'] = tmpDir;
    __resetIndexCacheForTesting();
  });

  after(async () => {
    if (originalHome !== undefined) {
      process.env['RELAYBURN_HOME'] = originalHome;
    } else {
      delete process.env['RELAYBURN_HOME'];
    }
    await rm(tmpDir, { recursive: true, force: true });
  });

  it('returns empty array when archive has no rows', async () => {
    await buildArchive();
    const out = await queryTurnsFromArchive({});
    assert.deepEqual(out, []);
  });

  it('returns turns with the same usage and shape queryAll would', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-A', messageId: 'm-A1' }),
      fakeTurn({
        sessionId: 's-A',
        messageId: 'm-A2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        usage: {
          input: 10,
          output: 20,
          reasoning: 5,
          cacheRead: 0,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
    ]);
    await buildArchive();

    const fromArchive = await queryTurnsFromArchive({ sessionId: 's-A' });
    const fromLedger = await queryAll({ sessionId: 's-A' });

    assert.equal(fromArchive.length, 2);
    assert.equal(fromArchive.length, fromLedger.length);

    // Sort both by messageId so the comparison is deterministic across the
    // ts/turn_index ASC ordering vs. ledger emit order.
    const sortByMessageId = (a: { messageId: string }, b: { messageId: string }) =>
      a.messageId.localeCompare(b.messageId);
    fromArchive.sort(sortByMessageId);
    fromLedger.sort(sortByMessageId);

    for (let i = 0; i < fromArchive.length; i++) {
      const a = fromArchive[i]!;
      const b = fromLedger[i]!;
      assert.equal(a.sessionId, b.sessionId);
      assert.equal(a.messageId, b.messageId);
      assert.equal(a.model, b.model);
      assert.equal(a.usage.input, b.usage.input);
      assert.equal(a.usage.output, b.usage.output);
      assert.equal(a.usage.reasoning, b.usage.reasoning);
      assert.equal(a.usage.cacheRead, b.usage.cacheRead);
      assert.equal(a.usage.cacheCreate5m, b.usage.cacheCreate5m);
      assert.equal(a.usage.cacheCreate1h, b.usage.cacheCreate1h);
    }
  });

  it('honors the since filter (turns older than the cutoff are dropped)', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-T',
        messageId: 'old',
        ts: '2026-04-20T00:00:00.000Z',
      }),
      fakeTurn({
        sessionId: 's-T',
        messageId: 'new',
        turnIndex: 1,
        ts: '2026-04-21T00:00:00.000Z',
      }),
    ]);
    await buildArchive();
    const out = await queryTurnsFromArchive({ since: '2026-04-20T12:00:00.000Z' });
    assert.equal(out.length, 1);
    assert.equal(out[0]!.messageId, 'new');
  });

  it('honors source filter (matches the ledger reader)', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-cc', messageId: 'cc-1', source: 'claude-code' }),
      fakeTurn({ sessionId: 's-cx', messageId: 'cx-1', source: 'codex' }),
    ]);
    await buildArchive();
    const cc = await queryTurnsFromArchive({ source: 'claude-code' });
    const cx = await queryTurnsFromArchive({ source: 'codex' });
    assert.equal(cc.length, 1);
    assert.equal(cc[0]!.source, 'claude-code');
    assert.equal(cx.length, 1);
    assert.equal(cx[0]!.source, 'codex');
  });

  it('exposes folded enrichment columns on every turn (workflowId, persona, tier)', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-E', messageId: 'me-1' })]);
    await stamp(
      { sessionId: 's-E' },
      { workflowId: 'wf-7', persona: 'eng', tier: 'best' },
    );
    await buildArchive();
    const out = await queryTurnsFromArchive({ sessionId: 's-E' });
    assert.equal(out.length, 1);
    assert.equal(out[0]!.enrichment['workflowId'], 'wf-7');
    assert.equal(out[0]!.enrichment['persona'], 'eng');
    assert.equal(out[0]!.enrichment['tier'], 'best');
  });

  it('reconstructs tool_calls onto the EnrichedTurn rows', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-tc',
        messageId: 'mtc-1',
        toolCalls: [
          { id: 'tu-1', name: 'Read', target: '/tmp/foo.ts', argsHash: 'a1' },
          { id: 'tu-2', name: 'Edit', target: '/tmp/foo.ts', argsHash: 'a2', isError: false },
        ],
      }),
    ]);
    await buildArchive();
    const out = await queryTurnsFromArchive({ sessionId: 's-tc' });
    assert.equal(out.length, 1);
    const calls = out[0]!.toolCalls;
    assert.equal(calls.length, 2);
    assert.deepEqual(
      calls.map((c) => c.name),
      ['Read', 'Edit'],
    );
    assert.equal(calls[1]!.isError, false);
  });

  it('throws if the archive cannot be opened (caller responsible for fallback)', async () => {
    // Point RELAYBURN_HOME at a path the FS cannot create the dir under (a
    // non-existent file used as a parent). openArchive should reject and
    // queryTurnsFromArchive surface that.
    process.env['RELAYBURN_HOME'] = '/dev/null/nope';
    await assert.rejects(() => queryTurnsFromArchive({}));
  });
});
