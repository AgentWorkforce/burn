import { strict as assert } from 'node:assert';
import { mkdtemp, readFile, rm, stat, unlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, beforeEach, describe, it } from 'node:test';

import type {
  SessionRelationshipRecord,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import {
  appendRelationships,
  appendToolResultEvents,
  appendTurns,
  appendUserTurns,
  stamp,
} from './writer.js';
import {
  queryAll,
  queryRelationships,
  queryToolResultEvents,
  queryUserTurns,
} from './reader.js';
import { ledgerContentIndexPath, ledgerIndexPath, ledgerPath } from './paths.js';
import { __resetIndexCacheForTesting, rebuildIndex } from './index-sidecar.js';

function fakeTurn(overrides: Partial<TurnRecord> = {}): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    messageId: 'msg-1',
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    usage: { input: 100, output: 50, reasoning: 0, cacheRead: 1000, cacheCreate5m: 0, cacheCreate1h: 0 },
    toolCalls: [],
    project: '/tmp/project',
    ...overrides,
  };
}

function fakeUserTurn(overrides: Partial<UserTurnRecord> = {}): UserTurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    userUuid: 'user-1',
    ts: '2026-04-20T00:00:00.500Z',
    precedingMessageId: 'msg-1',
    followingMessageId: 'msg-2',
    blocks: [
      {
        kind: 'tool_result',
        toolUseId: 'tu-1',
        byteLen: 4000,
        approxTokens: 1000,
      },
    ],
    ...overrides,
  };
}

describe('ledger', () => {
  let tmpDir: string;
  const originalHome = process.env['RELAYBURN_HOME'];

  before(async () => {
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-test-'));
  });

  beforeEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
    await mkdtemp(path.join(tmpdir(), 'relayburn-test-')).then((d) => {
      tmpDir = d;
    });
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

  it('round-trips turns through append + query', async () => {
    await appendTurns([fakeTurn(), fakeTurn({ messageId: 'msg-2', turnIndex: 1 })]);
    const got = await queryAll();
    assert.equal(got.length, 2);
    assert.equal(got[0]!.messageId, 'msg-1');
    assert.deepEqual(got[0]!.enrichment, {});
  });

  it('stores ledger at RELAYBURN_HOME/ledger.jsonl', async () => {
    await appendTurns([fakeTurn()]);
    const expected = path.join(tmpDir, 'ledger.jsonl');
    assert.equal(ledgerPath(), expected);
  });

  it('folds a sessionId stamp onto matching turns', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-A', messageId: 'm-A1' }),
      fakeTurn({ sessionId: 's-B', messageId: 'm-B1' }),
    ]);
    await stamp({ sessionId: 's-A' }, { workflowId: 'wf-1', agentId: 'ag-42' });
    const got = await queryAll();
    const a = got.find((t) => t.sessionId === 's-A')!;
    const b = got.find((t) => t.sessionId === 's-B')!;
    assert.equal(a.enrichment['workflowId'], 'wf-1');
    assert.equal(a.enrichment['agentId'], 'ag-42');
    assert.deepEqual(b.enrichment, {});
  });

  it('emits a spawn-env relationship when a session stamp carries parentAgentId', async () => {
    await stamp(
      { sessionId: 's-child' },
      { agentId: 'ag-child', parentAgentId: 'ag-parent', harness: 'codex' },
    );
    const got = await queryRelationships();
    assert.equal(got.length, 1);
    const rel = got[0]!;
    assert.equal(rel.source, 'spawn-env');
    assert.equal(rel.sessionId, 's-child');
    assert.equal(rel.relatedSessionId, 'ag-parent');
    assert.equal(rel.relationshipType, 'subagent');
    assert.equal(rel.agentId, 'ag-child');
    assert.equal(typeof rel.ts, 'string');
  });

  it('applies messageId stamp only to that one turn', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-A', messageId: 'm1' }),
      fakeTurn({ sessionId: 's-A', messageId: 'm2', turnIndex: 1 }),
    ]);
    await stamp({ messageId: 'm2' }, { stepId: 'step-2' });
    const got = await queryAll();
    assert.equal(got.find((t) => t.messageId === 'm1')!.enrichment['stepId'], undefined);
    assert.equal(got.find((t) => t.messageId === 'm2')!.enrichment['stepId'], 'step-2');
  });

  it('later stamps override earlier stamps per key (last-write-wins)', async () => {
    await appendTurns([fakeTurn()]);
    await stamp({ sessionId: 's-1' }, { tier: 'best' });
    await new Promise((r) => setTimeout(r, 10));
    await stamp({ sessionId: 's-1' }, { tier: 'fast' });
    const got = await queryAll();
    assert.equal(got[0]!.enrichment['tier'], 'fast');
  });

  it('stamp range filters by ts', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-1', messageId: 'm1', ts: '2026-04-20T00:00:00.000Z' }),
      fakeTurn({ sessionId: 's-1', messageId: 'm2', ts: '2026-04-20T05:00:00.000Z', turnIndex: 1 }),
      fakeTurn({ sessionId: 's-1', messageId: 'm3', ts: '2026-04-20T10:00:00.000Z', turnIndex: 2 }),
    ]);
    await stamp(
      {
        sessionId: 's-1',
        range: { fromTs: '2026-04-20T03:00:00.000Z', toTs: '2026-04-20T06:00:00.000Z' },
      },
      { workflowId: 'wf-mid' },
    );
    const got = await queryAll();
    assert.equal(got.find((t) => t.messageId === 'm1')!.enrichment['workflowId'], undefined);
    assert.equal(got.find((t) => t.messageId === 'm2')!.enrichment['workflowId'], 'wf-mid');
    assert.equal(got.find((t) => t.messageId === 'm3')!.enrichment['workflowId'], undefined);
  });

  it('query filters by since, project, sessionId, and enrichment', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-A', messageId: 'm1', ts: '2026-04-19T00:00:00.000Z', project: '/a' }),
      fakeTurn({ sessionId: 's-B', messageId: 'm2', ts: '2026-04-20T12:00:00.000Z', project: '/b', turnIndex: 1 }),
    ]);
    await stamp({ sessionId: 's-B' }, { persona: 'posthog' });

    const sinceFiltered = await queryAll({ since: '2026-04-20T00:00:00.000Z' });
    assert.equal(sinceFiltered.length, 1);
    assert.equal(sinceFiltered[0]!.messageId, 'm2');

    const projectFiltered = await queryAll({ project: '/a' });
    assert.equal(projectFiltered.length, 1);
    assert.equal(projectFiltered[0]!.sessionId, 's-A');

    const enrichmentFiltered = await queryAll({ enrichment: { persona: 'posthog' } });
    assert.equal(enrichmentFiltered.length, 1);
    assert.equal(enrichmentFiltered[0]!.sessionId, 's-B');
  });

  it('stamp before the turn is still applied (out-of-order tolerant)', async () => {
    await stamp({ sessionId: 's-future' }, { workflowId: 'wf-early' });
    await appendTurns([fakeTurn({ sessionId: 's-future', messageId: 'm-late' })]);
    const got = await queryAll();
    assert.equal(got.length, 1);
    assert.equal(got[0]!.enrichment['workflowId'], 'wf-early');
  });

  it('empty selector throws', async () => {
    await assert.rejects(() => stamp({}, { x: 'y' }));
  });

  it('dedupes by (source, sessionId, messageId) across repeated appends', async () => {
    const t1 = fakeTurn({ messageId: 'dup-1' });
    const t2 = fakeTurn({ messageId: 'dup-2', turnIndex: 1 });
    await appendTurns([t1, t2]);
    const sizeAfterFirst = (await stat(ledgerPath())).size;

    await appendTurns([t1, t2]);
    const sizeAfterSecond = (await stat(ledgerPath())).size;
    assert.equal(sizeAfterSecond, sizeAfterFirst, 'ledger must not grow on repeated appends');

    const got = await queryAll();
    assert.equal(got.length, 2);
  });

  it('skips a turn whose content fingerprint matches (ID regenerated)', async () => {
    // Same timestamp/model/usage/toolCalls, different messageIds
    const a = fakeTurn({ messageId: 'id-a' });
    const b = fakeTurn({ messageId: 'id-b', turnIndex: 1 });
    await appendTurns([a]);
    await appendTurns([b]);
    const got = await queryAll();
    assert.equal(got.length, 1, 'content fingerprint dedup drops second turn');
  });

  it('query filters by projectKey when q.project matches projectKey', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-X',
        messageId: 'pk-1',
        project: '/Users/me/repo',
        projectKey: 'github.com/org/repo',
      }),
      fakeTurn({
        sessionId: 's-Y',
        messageId: 'pk-2',
        turnIndex: 1,
        ts: '2026-04-20T00:00:01.000Z',
        project: '/tmp/worktree',
        projectKey: 'github.com/org/repo',
      }),
      fakeTurn({
        sessionId: 's-Z',
        messageId: 'pk-3',
        turnIndex: 2,
        ts: '2026-04-20T00:00:02.000Z',
        project: '/tmp/other',
      }),
    ]);
    const byKey = await queryAll({ project: 'github.com/org/repo' });
    assert.equal(byKey.length, 2);
    const byPath = await queryAll({ project: '/Users/me/repo' });
    assert.equal(byPath.length, 1);
  });

  it('round-trips SessionRelationshipRecord through append + query', async () => {
    const root: SessionRelationshipRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-root',
      relationshipType: 'root',
      ts: '2026-04-20T00:00:00.000Z',
    };
    const sub: SessionRelationshipRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-root',
      relationshipType: 'subagent',
      relatedSessionId: 's-root',
      agentId: 'agent-1',
      parentToolUseId: 'tu_outer',
      subagentType: 'Explore',
      ts: '2026-04-20T00:00:01.000Z',
    };
    await appendRelationships([root, sub]);
    const got = await queryRelationships();
    assert.equal(got.length, 2);
    const r = got.find((x) => x.relationshipType === 'root')!;
    const s = got.find((x) => x.relationshipType === 'subagent')!;
    assert.equal(r.sessionId, 's-root');
    assert.equal(s.agentId, 'agent-1');
    assert.equal(s.subagentType, 'Explore');
  });

  it('relationship dedup is keyed on (source, sessionId, type, relatedSessionId, agentId, parentToolUseId)', async () => {
    const sub: SessionRelationshipRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-x',
      relationshipType: 'subagent',
      relatedSessionId: 's-x',
      agentId: 'agent-x',
      parentToolUseId: 'tu_x',
    };
    await appendRelationships([sub]);
    await appendRelationships([sub]);
    const sizeAfter = (await stat(ledgerPath())).size;
    await appendRelationships([sub]);
    assert.equal((await stat(ledgerPath())).size, sizeAfter, 'duplicate relationships must not grow the ledger');
    const got = await queryRelationships();
    assert.equal(got.length, 1);
  });

  it('round-trips ToolResultEventRecord through append + query and dedupes by (sessionId, toolUseId, eventIndex)', async () => {
    const ev1: ToolResultEventRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-tre',
      toolUseId: 'tu_a',
      callIndex: 0,
      eventIndex: 0,
      status: 'completed',
      eventSource: 'tool_result',
      contentLength: 12,
      contentHash: 'abc1234567890def',
    };
    const ev2: ToolResultEventRecord = {
      ...ev1,
      eventIndex: 1,
      status: 'errored',
      isError: true,
    };
    await appendToolResultEvents([ev1, ev2]);
    await appendToolResultEvents([ev1]); // dup
    const got = await queryToolResultEvents();
    assert.equal(got.length, 2);
    const errored = got.find((e) => e.status === 'errored')!;
    assert.equal(errored.isError, true);
    assert.equal(errored.eventIndex, 1);
  });

  it('round-trips UserTurnRecord through append + query and dedupes by (sessionId, userUuid)', async () => {
    const u1 = fakeUserTurn();
    const u2 = fakeUserTurn({
      userUuid: 'user-2',
      ts: '2026-04-20T00:00:01.000Z',
      blocks: [{ kind: 'text', byteLen: 20, approxTokens: 5 }],
    });
    await appendUserTurns([u1, u2]);
    await appendUserTurns([u1]);

    const got = await queryUserTurns();
    assert.equal(got.length, 2);
    assert.equal(got[0]!.userUuid, 'user-1');
    assert.equal(got[0]!.blocks[0]!.toolUseId, 'tu-1');

    const filtered = await queryUserTurns({ sessionId: 's-1', source: 'codex' });
    assert.equal(filtered.length, 0);
  });

  it('queryRelationships filters by source and sessionId (matching child or parent)', async () => {
    await appendRelationships([
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-A',
        relationshipType: 'root',
      },
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-A',
        relatedSessionId: 's-A',
        relationshipType: 'subagent',
        agentId: 'a-1',
      },
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-B',
        relationshipType: 'root',
      },
    ]);
    const filteredA = await queryRelationships({ sessionId: 's-A' });
    // Both the root row and the subagent row match s-A (the root via
    // sessionId, the subagent via relatedSessionId).
    assert.equal(filteredA.length, 2);
    const filteredSrc = await queryRelationships({ source: 'codex' });
    assert.equal(filteredSrc.length, 0);
  });

  it('rebuildIndex recovers after index files are deleted', async () => {
    const t1 = fakeTurn({ messageId: 'r-1', ts: '2026-04-20T00:00:00.000Z' });
    const t2 = fakeTurn({
      messageId: 'r-2',
      turnIndex: 1,
      ts: '2026-04-20T00:01:00.000Z', // distinct content fingerprint
    });
    await appendTurns([t1, t2]);

    // Delete sidecar index files
    await unlink(ledgerIndexPath());
    await unlink(ledgerContentIndexPath());
    __resetIndexCacheForTesting();

    const { ids, content } = await rebuildIndex();
    assert.equal(ids, 2);
    assert.equal(content, 2);

    // After rebuild, re-appending the same turns must not duplicate
    const sizeBefore = (await stat(ledgerPath())).size;
    await appendTurns([t1, t2]);
    const sizeAfter = (await stat(ledgerPath())).size;
    assert.equal(sizeAfter, sizeBefore);

    // Verify index files are populated
    const idsContent = await readFile(ledgerIndexPath(), 'utf8');
    assert.equal(idsContent.trim().split('\n').length, 2);
  });

  it('round-trips UserTurnRecord through append + query and dedupes by (source, sessionId, userUuid)', async () => {
    const u1: UserTurnRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-ut',
      userUuid: 'u-1',
      ts: '2026-04-20T00:00:00.000Z',
      followingMessageId: 'msg-1',
      blocks: [
        { kind: 'text', byteLen: 12, approxTokens: 3 },
      ],
    };
    const u2: UserTurnRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-ut',
      userUuid: 'u-2',
      ts: '2026-04-20T00:00:01.000Z',
      precedingMessageId: 'msg-1',
      followingMessageId: 'msg-2',
      blocks: [
        { kind: 'tool_result', toolUseId: 'tu_a', byteLen: 40, approxTokens: 10 },
        { kind: 'tool_result', toolUseId: 'tu_b', byteLen: 80, approxTokens: 20, isError: true },
      ],
    };
    await appendUserTurns([u1, u2]);
    await appendUserTurns([u1]); // dup: same userUuid
    const got = await queryUserTurns();
    assert.equal(got.length, 2, 'second append must not produce a duplicate');
    const second = got.find((r) => r.userUuid === 'u-2')!;
    assert.equal(second.blocks.length, 2);
    assert.equal(second.blocks[1]!.isError, true);
  });

  it('queryUserTurns filters by sessionId, source, and time range', async () => {
    const a: UserTurnRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-A',
      userUuid: 'u-A1',
      ts: '2026-04-19T00:00:00.000Z',
      blocks: [{ kind: 'text', byteLen: 4, approxTokens: 1 }],
    };
    const b: UserTurnRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-B',
      userUuid: 'u-B1',
      ts: '2026-04-21T00:00:00.000Z',
      blocks: [{ kind: 'text', byteLen: 4, approxTokens: 1 }],
    };
    const c: UserTurnRecord = {
      v: 1,
      source: 'codex',
      sessionId: 's-C',
      userUuid: 'u-C1',
      ts: '2026-04-21T00:00:00.000Z',
      blocks: [{ kind: 'text', byteLen: 4, approxTokens: 1 }],
    };
    await appendUserTurns([a, b, c]);
    assert.equal((await queryUserTurns({ sessionId: 's-A' })).length, 1);
    assert.equal((await queryUserTurns({ source: 'codex' })).length, 1);
    const sinceFiltered = await queryUserTurns({ since: '2026-04-20T00:00:00.000Z' });
    assert.equal(sinceFiltered.length, 2);
  });

  it('user-turn append + later turn append do not collide on the ledger lock or id index', async () => {
    // Sanity: a user-turn line and a turn line written for overlapping
    // sessionIds share the same ledger but live in distinct id slots.
    await appendUserTurns([
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-mix',
        userUuid: 'u-mix-1',
        ts: '2026-04-20T00:00:00.000Z',
        blocks: [{ kind: 'text', byteLen: 8, approxTokens: 2 }],
      },
    ]);
    await appendTurns([fakeTurn({ sessionId: 's-mix', messageId: 'msg-mix-1' })]);
    const turns = await queryAll({ sessionId: 's-mix' });
    const userTurns = await queryUserTurns({ sessionId: 's-mix' });
    assert.equal(turns.length, 1);
    assert.equal(userTurns.length, 1);
  });

  it('rebuildIndex re-indexes user_turn lines', async () => {
    const turn = fakeTurn({ messageId: 'rebuild-ut-1' });
    const u: UserTurnRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-rebuild-ut',
      userUuid: 'u-rebuild-1',
      ts: '2026-04-20T00:00:00.000Z',
      blocks: [{ kind: 'text', byteLen: 16, approxTokens: 4 }],
    };
    await appendTurns([turn]);
    await appendUserTurns([u]);

    await unlink(ledgerIndexPath());
    await unlink(ledgerContentIndexPath());
    __resetIndexCacheForTesting();

    const { ids } = await rebuildIndex();
    // 1 turn + 1 user_turn = 2 ids.
    assert.equal(ids, 2);

    // After rebuild, re-appending the same user turn must not duplicate.
    const sizeBefore = (await stat(ledgerPath())).size;
    await appendUserTurns([u]);
    assert.equal((await stat(ledgerPath())).size, sizeBefore);
  });

  it('rebuildIndex re-indexes auxiliary append-only lines', async () => {
    const turn = fakeTurn({ messageId: 'rebuild-1' });
    const rel: SessionRelationshipRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-rebuild',
      relationshipType: 'subagent',
      agentId: 'a-rebuild',
      relatedSessionId: 's-rebuild',
    };
    const ev: ToolResultEventRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-rebuild',
      toolUseId: 'tu_rebuild',
      eventIndex: 0,
      status: 'completed',
      eventSource: 'tool_result',
    };
    const userTurn = fakeUserTurn({
      sessionId: 's-rebuild',
      userUuid: 'user-rebuild',
    });
    await appendTurns([turn]);
    await appendRelationships([rel]);
    await appendToolResultEvents([ev]);
    await appendUserTurns([userTurn]);

    await unlink(ledgerIndexPath());
    await unlink(ledgerContentIndexPath());
    __resetIndexCacheForTesting();

    const { ids } = await rebuildIndex();
    // 1 turn + 1 relationship + 1 tool_result_event + 1 user_turn = 4 ids.
    assert.equal(ids, 4);

    // After rebuild, re-appending the same auxiliary records must not duplicate.
    const sizeBefore = (await stat(ledgerPath())).size;
    await appendRelationships([rel]);
    await appendToolResultEvents([ev]);
    await appendUserTurns([userTurn]);
    assert.equal((await stat(ledgerPath())).size, sizeBefore);
  });
});
