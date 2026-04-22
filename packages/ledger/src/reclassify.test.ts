import { strict as assert } from 'node:assert';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, beforeEach, describe, it } from 'node:test';

import type { ContentRecord, TurnRecord } from '@relayburn/reader';

import { appendContent } from './content.js';
import { __resetIndexCacheForTesting } from './index-sidecar.js';
import { ledgerPath } from './paths.js';
import { reclassifyLedger } from './reclassify.js';
import { queryAll } from './reader.js';
import { appendTurns } from './writer.js';

function fakeTurn(overrides: Partial<TurnRecord> = {}): TurnRecord {
  return {
    v: 1,
    source: 'codex',
    sessionId: 'sess-reclassify-1',
    messageId: 'msg-1',
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model: 'gpt-5.4',
    usage: {
      input: 100,
      output: 50,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    ...overrides,
  };
}

describe('reclassifyLedger', () => {
  let tmpDir: string;
  const originalHome = process.env['RELAYBURN_HOME'];

  before(async () => {
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-reclassify-'));
  });

  beforeEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-reclassify-'));
    process.env['RELAYBURN_HOME'] = tmpDir;
    __resetIndexCacheForTesting();
  });

  after(async () => {
    if (originalHome !== undefined) process.env['RELAYBURN_HOME'] = originalHome;
    else delete process.env['RELAYBURN_HOME'];
    await rm(tmpDir, { recursive: true, force: true });
  });

  it('is a no-op when there is no ledger file yet', async () => {
    const report = await reclassifyLedger();
    assert.equal(report.scanned, 0);
    assert.equal(report.reclassified, 0);
  });

  it('fills activity on previously unclassified turns using tool signal alone', async () => {
    await appendTurns([
      fakeTurn({
        messageId: 'msg-coding',
        toolCalls: [{ id: 'c1', name: 'apply_patch', argsHash: 'h', target: '/a.ts' }],
      }),
      fakeTurn({
        messageId: 'msg-testing',
        turnIndex: 1,
        ts: '2026-04-20T00:00:01.000Z',
        toolCalls: [{ id: 'c2', name: 'exec_command', argsHash: 'h', target: 'pytest -q' }],
      }),
      fakeTurn({
        messageId: 'msg-exploration',
        turnIndex: 2,
        ts: '2026-04-20T00:00:02.000Z',
        toolCalls: [{ id: 'c3', name: 'read_file', argsHash: 'h', target: '/b.ts' }],
      }),
    ]);

    const report = await reclassifyLedger();
    assert.equal(report.scanned, 3);
    assert.equal(report.reclassified, 3);
    assert.equal(report.skipped, 0);

    const turns = await queryAll();
    const byId = new Map(turns.map((t) => [t.messageId, t]));
    assert.equal(byId.get('msg-coding')!.activity, 'coding');
    assert.equal(byId.get('msg-coding')!.hasEdits, true);
    assert.equal(byId.get('msg-testing')!.activity, 'testing');
    assert.equal(byId.get('msg-exploration')!.activity, 'exploration');
  });

  it('skips turns that already have activity unless --force', async () => {
    await appendTurns([
      fakeTurn({
        messageId: 'msg-already',
        activity: 'refactoring',
        hasEdits: true,
        retries: 0,
        toolCalls: [{ id: 'c1', name: 'apply_patch', argsHash: 'h', target: '/a.ts' }],
      }),
    ]);

    const defaultRun = await reclassifyLedger();
    assert.equal(defaultRun.scanned, 1);
    assert.equal(defaultRun.reclassified, 0);
    assert.equal(defaultRun.skipped, 1);

    const turnsAfter = await queryAll();
    assert.equal(turnsAfter[0]!.activity, 'refactoring');

    // Force run: would downgrade to 'coding' because there is no keyword
    // signal in the content sidecar for this turn. That's the point of the
    // default being non-destructive.
    const forcedRun = await reclassifyLedger({ force: true });
    assert.equal(forcedRun.reclassified, 1);
    const turnsForced = await queryAll();
    assert.equal(turnsForced[0]!.activity, 'coding');
  });

  it('uses content sidecar signals when available (user text + tool errors)', async () => {
    const sessionId = 'abcdef12-3456-7890-abcd-ef1234567890';
    await appendTurns([
      fakeTurn({
        source: 'claude-code',
        sessionId,
        messageId: 'msg-asst-1',
        ts: '2026-04-20T00:00:05.000Z',
        toolCalls: [
          { id: 'tool-1', name: 'Bash', argsHash: 'h', target: 'pytest -q' },
        ],
      }),
    ]);

    const content: ContentRecord[] = [
      {
        v: 1,
        source: 'claude-code',
        sessionId,
        messageId: 'msg-user-1',
        ts: '2026-04-20T00:00:04.000Z',
        role: 'user',
        kind: 'text',
        text: 'please fix the failing build',
      },
      {
        v: 1,
        source: 'claude-code',
        sessionId,
        messageId: 'msg-next-user-1',
        ts: '2026-04-20T00:00:06.000Z',
        role: 'tool_result',
        kind: 'tool_result',
        toolResult: { toolUseId: 'tool-1', content: 'FAIL', isError: true },
      },
    ];
    await appendContent(content);

    const report = await reclassifyLedger();
    assert.equal(report.reclassified, 1);

    const turns = await queryAll();
    // pytest-by-bash would normally be 'testing', but the failed tool_result
    // flips it to 'debugging' via the classifier's hasFailedTool branch.
    assert.equal(turns[0]!.activity, 'debugging');
  });

  it('preserves stamp lines and blank lines verbatim', async () => {
    // Seed a turn + stamp
    await appendTurns([
      fakeTurn({
        messageId: 'msg-pres',
        toolCalls: [{ id: 'c1', name: 'apply_patch', argsHash: 'h', target: '/a.ts' }],
      }),
    ]);
    const { stamp } = await import('./writer.js');
    await stamp({ sessionId: 'sess-reclassify-1' }, { workflowId: 'wf-1' });

    const before = await readFile(ledgerPath(), 'utf8');
    const stampLinesBefore = before.split('\n').filter((l) => l.includes('"kind":"stamp"'));
    assert.equal(stampLinesBefore.length, 1);

    await reclassifyLedger();

    const after = await readFile(ledgerPath(), 'utf8');
    const stampLinesAfter = after.split('\n').filter((l) => l.includes('"kind":"stamp"'));
    assert.equal(stampLinesAfter.length, 1);
    assert.equal(stampLinesAfter[0], stampLinesBefore[0]);
  });
});
