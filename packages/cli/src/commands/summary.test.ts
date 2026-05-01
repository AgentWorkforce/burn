import { strict as assert } from 'node:assert';
import { mkdtemp, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, afterEach, beforeEach, describe, it } from 'node:test';

import {
  appendRelationships,
  appendUserTurns,
  appendTurns,
  archivePath,
  stamp,
  __resetIndexCacheForTesting,
} from '@relayburn/ledger';
import type {
  Fidelity,
  SessionRelationshipRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import { runSummary } from './summary.js';

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
      input: 1000,
      output: 500,
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

function fakeUserTurn(overrides: Partial<UserTurnRecord> = {}): UserTurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    userUuid: 'u-1',
    ts: '2026-04-20T00:00:30.000Z',
    precedingMessageId: 'msg-1',
    followingMessageId: 'msg-2',
    blocks: [],
    ...overrides,
  };
}

function fakeRelationship(
  overrides: Partial<SessionRelationshipRecord> = {},
): SessionRelationshipRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    relationshipType: 'root',
    ts: '2026-04-20T00:00:00.000Z',
    ...overrides,
  };
}

interface CapturedOutput {
  stdout: string;
  stderr: string;
  code: number;
}

async function captureSummary(
  flags: Record<string, string | true> = {},
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
    code = await runSummary({ flags, tags: {}, positional: [], passthrough: [] });
  } finally {
    process.stdout.write = origStdout;
    process.stderr.write = origStderr;
  }
  return { stdout, stderr, code };
}

describe('burn summary archive integration (#82)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalArchive = process.env['RELAYBURN_ARCHIVE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-summary-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-summary-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    delete process.env['RELAYBURN_ARCHIVE'];
    __resetIndexCacheForTesting();
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalArchive !== undefined) process.env['RELAYBURN_ARCHIVE'] = originalArchive;
    else delete process.env['RELAYBURN_ARCHIVE'];
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  it('--json output is identical between archive and ledger paths (parity)', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-A', messageId: 'pa-1' }),
      fakeTurn({
        sessionId: 's-A',
        messageId: 'pa-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
      fakeTurn({
        sessionId: 's-B',
        messageId: 'pa-3',
        ts: '2026-04-20T00:02:00.000Z',
        model: 'claude-haiku-4-5',
        project: '/tmp/other',
      }),
    ]);
    await stamp({ sessionId: 's-A' }, { workflowId: 'wf-parity' });

    // Default path: builds the archive, then queries SQL.
    const archiveOut = await captureSummary({ json: true });
    assert.equal(archiveOut.code, 0);

    // Fallback path: streams the ledger.
    const ledgerOut = await captureSummary({ json: true, 'no-archive': true });
    assert.equal(ledgerOut.code, 0);

    interface SummaryPayload {
      turns: number;
      totalCost: { total: number };
      byModel: Array<{ model: string; turns: number; usage: Record<string, number>; cost: { total: number } }>;
      fidelity: unknown;
    }
    const archive = JSON.parse(archiveOut.stdout) as SummaryPayload;
    const ledger = JSON.parse(ledgerOut.stdout) as SummaryPayload;
    assert.equal(archive.turns, ledger.turns);
    assert.equal(archive.turns, 3);
    assert.deepEqual(
      archive.byModel.map((r) => ({ model: r.model, turns: r.turns, usage: r.usage, cost: r.cost })),
      ledger.byModel.map((r) => ({ model: r.model, turns: r.turns, usage: r.usage, cost: r.cost })),
    );
    assert.deepEqual(archive.totalCost, ledger.totalCost);
    assert.deepEqual(archive.fidelity, ledger.fidelity);
  });

  it('default path auto-builds archive.sqlite on first run', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-AB', messageId: 'ab-1' })]);
    // Pre-condition: no archive on disk.
    await assert.rejects(stat(archivePath()), /ENOENT/);

    const out = await captureSummary({ json: true });
    assert.equal(out.code, 0);

    // Post-condition: `loadTurns` ran `buildArchive()` and the file exists.
    const st = await stat(archivePath());
    assert.equal(st.isFile(), true);
  });

  it('--no-archive flag does NOT build the archive (fallback path)', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-NA', messageId: 'na-1' })]);
    await assert.rejects(stat(archivePath()), /ENOENT/);

    const out = await captureSummary({ json: true, 'no-archive': true });
    assert.equal(out.code, 0);

    // The archive should still be missing — we hit the legacy `queryAll` path.
    await assert.rejects(stat(archivePath()), /ENOENT/);
  });

  it('--agent includes sessions linked by relationship records', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-parent', messageId: 'parent-1' }),
      fakeTurn({
        sessionId: 's-child',
        messageId: 'child-1',
        ts: '2026-04-20T00:01:00.000Z',
        usage: {
          input: 2000,
          output: 500,
          reasoning: 0,
          cacheRead: 1000,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
    ]);
    await appendRelationships([
      {
        v: 1,
        source: 'spawn-env',
        sessionId: 's-child',
        relatedSessionId: 'ag-parent',
        relationshipType: 'subagent',
        agentId: 'ag-child',
      },
    ]);

    const out = await captureSummary({
      json: true,
      agent: 'ag-parent',
      'no-archive': true,
    });
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as { turns: number };
    assert.equal(parsed.turns, 1);
  });

  it('RELAYBURN_ARCHIVE=0 env disables the archive path (fallback)', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-ENV', messageId: 'env-1' })]);
    await assert.rejects(stat(archivePath()), /ENOENT/);

    process.env['RELAYBURN_ARCHIVE'] = '0';
    try {
      const out = await captureSummary({ json: true });
      assert.equal(out.code, 0);
    } finally {
      delete process.env['RELAYBURN_ARCHIVE'];
    }
    // Same fallback behavior — no archive built.
    await assert.rejects(stat(archivePath()), /ENOENT/);
  });

  it('text output matches between archive and ledger paths (parity)', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-T', messageId: 'tx-1' }),
      fakeTurn({
        sessionId: 's-T',
        messageId: 'tx-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);

    const archiveOut = await captureSummary({});
    assert.equal(archiveOut.code, 0);
    const ledgerOut = await captureSummary({ 'no-archive': true });
    assert.equal(ledgerOut.code, 0);

    // The "ingested N new sessions (+M turns)" preamble depends on the live
    // ingest pass which is a no-op here (no ~/.claude or ~/.codex sessions in
    // the temp HOME), but stripping the preamble keeps the test resilient if
    // that contract ever changes. Compare the body — model table + total
    // cost.
    const stripPreamble = (s: string): string => {
      const idx = s.indexOf('turns analyzed:');
      return idx >= 0 ? s.slice(idx) : s;
    };
    assert.equal(stripPreamble(archiveOut.stdout), stripPreamble(ledgerOut.stdout));
  });
});

describe('burn summary --by-relationship (#114)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalArchive = process.env['RELAYBURN_ARCHIVE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-summary-rel-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-summary-rel-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    process.env['RELAYBURN_ARCHIVE'] = '0';
    __resetIndexCacheForTesting();
  });

  afterEach(async () => {
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalArchive !== undefined) process.env['RELAYBURN_ARCHIVE'] = originalArchive;
    else delete process.env['RELAYBURN_ARCHIVE'];
  });

  it('renders a relationship table for all four relationship types', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 'rel-root', messageId: 'rel-root-1' }),
      fakeTurn({
        sessionId: 'rel-root',
        messageId: 'rel-sub-1',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        subagent: {
          isSidechain: true,
          agentId: 'agent-explore',
          parentAgentId: 'rel-root',
          subagentType: 'Explore',
        },
      }),
      fakeTurn({
        sessionId: 'rel-cont',
        messageId: 'rel-cont-1',
        ts: '2026-04-20T00:02:00.000Z',
      }),
      fakeTurn({
        sessionId: 'rel-fork',
        messageId: 'rel-fork-1',
        ts: '2026-04-20T00:03:00.000Z',
      }),
    ]);
    await appendRelationships([
      fakeRelationship({ sessionId: 'rel-root', relationshipType: 'root' }),
      fakeRelationship({
        sessionId: 'rel-root',
        relationshipType: 'subagent',
        agentId: 'agent-explore',
        relatedSessionId: 'rel-root',
        subagentType: 'Explore',
      }),
      fakeRelationship({
        sessionId: 'rel-cont',
        relationshipType: 'continuation',
        relatedSessionId: 'rel-parent',
      }),
      fakeRelationship({
        sessionId: 'rel-fork',
        relationshipType: 'fork',
        relatedSessionId: 'rel-source',
        sourceSessionId: 'rel-source',
      }),
    ]);

    const out = await captureSummary({ 'by-relationship': true });
    assert.equal(out.code, 0);
    assert.match(
      out.stdout,
      /relationshipType\s+sessionCount\s+turnCount\s+total\s+median\s+p95\s+mean/,
    );
    assert.match(out.stdout, /root\s+1\s+1\s+\$/);
    assert.match(out.stdout, /continuation\s+1\s+1\s+\$/);
    assert.match(out.stdout, /fork\s+1\s+1\s+\$/);
    assert.match(out.stdout, /subagent\s+1\s+1\s+\$/);
  });

  it('--json emits a stable relationships block', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 'rel-json-root', messageId: 'rel-json-root-1' }),
      fakeTurn({
        sessionId: 'rel-json-cont',
        messageId: 'rel-json-cont-1',
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    await appendRelationships([
      fakeRelationship({ sessionId: 'rel-json-root', relationshipType: 'root' }),
      fakeRelationship({
        sessionId: 'rel-json-cont',
        relationshipType: 'continuation',
        relatedSessionId: 'rel-json-parent',
      }),
    ]);

    const out = await captureSummary({ 'by-relationship': true, json: true });
    assert.equal(out.code, 0);
    interface Payload {
      relationships: Array<{
        relationshipType: string;
        count: number;
        sessionCount: number;
        turnCount: number;
        totalCost: number;
        medianCost: number;
        p95Cost: number;
        meanCost: number;
      }>;
    }
    const payload = JSON.parse(out.stdout) as Payload;
    assert.deepEqual(
      payload.relationships.map((r) => r.relationshipType),
      ['root', 'continuation'],
    );
    assert.equal(payload.relationships[0]!.count, 1);
    assert.equal(payload.relationships[0]!.sessionCount, 1);
    assert.equal(payload.relationships[0]!.turnCount, 1);
    assert.equal(typeof payload.relationships[0]!.totalCost, 'number');
    assert.equal(typeof payload.relationships[0]!.medianCost, 'number');
    assert.equal(typeof payload.relationships[0]!.p95Cost, 'number');
    assert.equal(typeof payload.relationships[0]!.meanCost, 'number');
  });

  it('can aggregate a subagent-only slice', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'rel-subonly',
        messageId: 'rel-subonly-1',
        subagent: {
          isSidechain: true,
          agentId: 'agent-review',
          parentAgentId: 'rel-subonly',
          subagentType: 'code-reviewer',
        },
      }),
    ]);
    await appendRelationships([
      fakeRelationship({
        source: 'native-claude',
        sessionId: 'rel-subonly',
        relationshipType: 'subagent',
        agentId: 'agent-review',
        relatedSessionId: 'rel-subonly',
        subagentType: 'code-reviewer',
      }),
    ]);

    const out = await captureSummary({ 'by-relationship': true, json: true });
    assert.equal(out.code, 0);
    const payload = JSON.parse(out.stdout) as {
      relationships: Array<{
        relationshipType: string;
        count: number;
        sessionCount: number;
        turnCount: number;
        totalCost: number;
      }>;
    };
    assert.equal(payload.relationships.length, 1);
    assert.equal(payload.relationships[0]!.relationshipType, 'subagent');
    assert.equal(payload.relationships[0]!.count, 1);
    assert.equal(payload.relationships[0]!.sessionCount, 1);
    assert.equal(payload.relationships[0]!.turnCount, 1);
    assert.equal(typeof payload.relationships[0]!.totalCost, 'number');
  });

  it('joins spawn-env child-session relationships to turns without subagent metadata', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'rel-spawn-child',
        messageId: 'rel-spawn-child-1',
        source: 'codex',
      }),
    ]);
    await appendRelationships([
      fakeRelationship({
        source: 'spawn-env',
        sessionId: 'rel-spawn-child',
        relationshipType: 'subagent',
        relatedSessionId: 'rel-spawn-parent',
        agentId: 'agent-spawn-child',
        subagentType: 'worker',
      }),
    ]);

    const out = await captureSummary({ 'by-relationship': 'subagent', json: true });
    assert.equal(out.code, 0);
    const payload = JSON.parse(out.stdout) as {
      relationships: Array<{ relationshipType: string; turnCount: number }>;
      subagentTypes: Array<{
        subagentType: string;
        invocations: number;
        turns: number;
        totalCost: number;
        medianCost: number;
        p95Cost: number;
        meanCost: number;
      }>;
    };
    assert.equal(payload.relationships[0]!.relationshipType, 'subagent');
    assert.equal(payload.relationships[0]!.turnCount, 1);
    assert.deepEqual(payload.subagentTypes, [
      {
        subagentType: 'worker',
        invocations: 1,
        turns: 1,
        totalCost: payload.subagentTypes[0]!.totalCost,
        medianCost: payload.subagentTypes[0]!.medianCost,
        p95Cost: payload.subagentTypes[0]!.p95Cost,
        meanCost: payload.subagentTypes[0]!.meanCost,
      },
    ]);
  });

  it('--by-relationship=subagent renders the subagent type breakdown', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'rel-types',
        messageId: 'rel-types-1',
        subagent: {
          isSidechain: true,
          agentId: 'agent-explore-1',
          parentAgentId: 'rel-types',
          subagentType: 'Explore',
        },
      }),
      fakeTurn({
        sessionId: 'rel-types',
        messageId: 'rel-types-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        subagent: {
          isSidechain: true,
          agentId: 'agent-explore-2',
          parentAgentId: 'rel-types',
          subagentType: 'Explore',
        },
      }),
      fakeTurn({
        sessionId: 'rel-types',
        messageId: 'rel-types-3',
        turnIndex: 2,
        ts: '2026-04-20T00:02:00.000Z',
        subagent: {
          isSidechain: true,
          agentId: 'agent-review-1',
          parentAgentId: 'rel-types',
          subagentType: 'code-reviewer',
        },
      }),
    ]);
    await appendRelationships([
      fakeRelationship({
        source: 'native-claude',
        sessionId: 'rel-types',
        relationshipType: 'subagent',
        agentId: 'agent-explore-1',
        relatedSessionId: 'rel-types',
        subagentType: 'Explore',
      }),
      fakeRelationship({
        source: 'native-claude',
        sessionId: 'rel-types',
        relationshipType: 'subagent',
        agentId: 'agent-explore-2',
        relatedSessionId: 'rel-types',
        subagentType: 'Explore',
      }),
      fakeRelationship({
        source: 'native-claude',
        sessionId: 'rel-types',
        relationshipType: 'subagent',
        agentId: 'agent-review-1',
        relatedSessionId: 'rel-types',
        subagentType: 'code-reviewer',
      }),
    ]);

    const out = await captureSummary({ 'by-relationship': 'subagent' });
    assert.equal(out.code, 0);
    assert.match(
      out.stdout,
      /subagentType\s+invocations\s+turns\s+total\s+median\s+p95\s+mean/,
    );
    assert.match(out.stdout, /Explore\s+2\s+2\s+\$/);
    assert.match(out.stdout, /code-reviewer\s+1\s+1\s+\$/);
  });

  it('prints a clear message when no relationship rows match the slice', async () => {
    await appendTurns([fakeTurn({ sessionId: 'rel-empty', messageId: 'rel-empty-1' })]);
    const out = await captureSummary({ 'by-relationship': true });
    assert.equal(out.code, 0);
    assert.match(out.stdout, /no SessionRelationshipRecord rows found for the matched slice/);
  });
});

describe('burn summary --subagent-tree relationships (#109)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalArchive = process.env['RELAYBURN_ARCHIVE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-summary-tree-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-summary-tree-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    process.env['RELAYBURN_ARCHIVE'] = '0';
    __resetIndexCacheForTesting();
  });

  afterEach(async () => {
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalArchive !== undefined) process.env['RELAYBURN_ARCHIVE'] = originalArchive;
    else delete process.env['RELAYBURN_ARCHIVE'];
  });

  it('--json includes relationshipType and renders child-session subagents', async () => {
    await appendTurns([
      fakeTurn({
        source: 'codex',
        sessionId: 'tree-parent',
        messageId: 'tree-parent-1',
        model: 'gpt-5.1-codex',
      }),
      fakeTurn({
        source: 'codex',
        sessionId: 'tree-child-session',
        messageId: 'tree-child-1',
        model: 'gpt-5.1-codex',
      }),
    ]);
    await appendRelationships([
      fakeRelationship({
        source: 'codex',
        sessionId: 'tree-parent',
        relationshipType: 'root',
      }),
      fakeRelationship({
        source: 'codex',
        sessionId: 'tree-child-session',
        relationshipType: 'subagent',
        relatedSessionId: 'tree-parent',
        agentId: 'tree-child-agent',
        subagentType: 'worker',
      }),
    ]);

    const out = await captureSummary({
      'subagent-tree': 'tree-parent',
      json: true,
      'no-archive': true,
    });
    assert.equal(out.code, 0);
    const payload = JSON.parse(out.stdout) as {
      relationshipType: string;
      selfTurns: number;
      cumulativeTurns: number;
      children: Array<{
        nodeId: string;
        label: string;
        relationshipType: string;
        selfTurns: number;
      }>;
    };
    assert.equal(payload.relationshipType, 'root');
    assert.equal(payload.selfTurns, 1);
    assert.equal(payload.cumulativeTurns, 2);
    assert.equal(payload.children[0]!.nodeId, 'tree-child-session');
    assert.equal(payload.children[0]!.label, 'worker');
    assert.equal(payload.children[0]!.relationshipType, 'subagent');
    assert.equal(payload.children[0]!.selfTurns, 1);

    const childOut = await captureSummary({
      'subagent-tree': 'tree-child-session',
      json: true,
      'no-archive': true,
    });
    assert.equal(childOut.code, 0);
    const childPayload = JSON.parse(childOut.stdout) as {
      nodeId: string;
      label: string;
      relationshipType: string;
      selfTurns: number;
    };
    assert.equal(childPayload.nodeId, 'tree-child-session');
    assert.equal(childPayload.label, 'worker');
    assert.equal(childPayload.relationshipType, 'subagent');
    assert.equal(childPayload.selfTurns, 1);
  });

  it('falls back to TurnRecord.subagent when no relationship rows exist', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 'tree-legacy', messageId: 'tree-legacy-1' }),
      fakeTurn({
        sessionId: 'tree-legacy',
        messageId: 'tree-legacy-sub-1',
        turnIndex: 1,
        subagent: {
          isSidechain: true,
          agentId: 'legacy-agent',
          parentAgentId: 'tree-legacy',
          subagentType: 'Explore',
        },
      }),
    ]);

    const out = await captureSummary({
      'subagent-tree': 'tree-legacy',
      json: true,
      'no-archive': true,
    });
    assert.equal(out.code, 0);
    const payload = JSON.parse(out.stdout) as {
      children: Array<{ label: string; relationshipType: string; selfTurns: number }>;
    };
    assert.equal(payload.children[0]!.label, 'Explore');
    assert.equal(payload.children[0]!.relationshipType, 'subagent');
    assert.equal(payload.children[0]!.selfTurns, 1);
  });

  it('annotates non-subagent relationship nodes in text output', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 'tree-root', messageId: 'tree-root-1' }),
      fakeTurn({
        sessionId: 'tree-fork',
        messageId: 'tree-fork-1',
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    await appendRelationships([
      fakeRelationship({ sessionId: 'tree-root', relationshipType: 'root' }),
      fakeRelationship({
        sessionId: 'tree-fork',
        relationshipType: 'fork',
        relatedSessionId: 'tree-root',
      }),
    ]);

    const out = await captureSummary({
      'subagent-tree': 'tree-root',
      'no-archive': true,
    });
    assert.equal(out.code, 0);
    assert.match(out.stdout, /tree-fork \[fork\]/);
  });
});

describe('burn summary per-cell fidelity (#136)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalArchive = process.env['RELAYBURN_ARCHIVE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-summary-cellfid-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-summary-cellfid-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    // Force the legacy ledger-walk path so the per-cell counters reflect the
    // exact turns we appended; archive-backed pricing/coverage is exercised
    // independently in the parity test above.
    process.env['RELAYBURN_ARCHIVE'] = '0';
    // The ledger's index-sidecar cache is module-level. Earlier suites
    // populate it against their own tmpRelay; without resetting it we'd
    // dedup against stale content fingerprints (same default ts + usage as
    // the parity test → silent skip in `appendTurns`).
    __resetIndexCacheForTesting();
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalArchive !== undefined) process.env['RELAYBURN_ARCHIVE'] = originalArchive;
    else delete process.env['RELAYBURN_ARCHIVE'];
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  function fullFidelity(): Fidelity {
    return {
      granularity: 'per-turn',
      coverage: {
        hasInputTokens: true,
        hasOutputTokens: true,
        hasReasoningTokens: true,
        hasCacheReadTokens: true,
        hasCacheCreateTokens: true,
        hasToolCalls: true,
        hasToolResultEvents: true,
        hasSessionRelationships: true,
        hasRawContent: true,
      },
      class: 'full',
    };
  }

  function partialMissingOutput(): Fidelity {
    const f = fullFidelity();
    return {
      ...f,
      coverage: { ...f.coverage, hasOutputTokens: false },
      class: 'partial',
    };
  }

  function aggregateNoOutputOrReasoning(): Fidelity {
    const f = fullFidelity();
    return {
      ...f,
      granularity: 'per-session-aggregate',
      coverage: {
        ...f.coverage,
        hasOutputTokens: false,
        hasReasoningTokens: false,
      },
      class: 'aggregate-only',
    };
  }

  it('renders no marker and no footer when every turn is full fidelity', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 'fc-1', messageId: 'fc-1', fidelity: fullFidelity() }),
      fakeTurn({
        sessionId: 'fc-1',
        messageId: 'fc-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        fidelity: fullFidelity(),
      }),
    ]);
    const out = await captureSummary({});
    assert.equal(out.code, 0);
    // No `*` partial marker on any cell, no footer line.
    assert.equal(out.stdout.includes('*'), false, 'no partial marker should appear');
    assert.equal(
      out.stdout.includes('partial coverage:'),
      false,
      'no partial-coverage footer for all-full slice',
    );
  });

  it('renders `—` (never `0`) for a field every turn omitted', async () => {
    // Two turns, both with output omitted from upstream. Pricing layer would
    // happily report `output: 0`, but the per-cell counter says "knew about
    // 0 of 2 turns" → render the dash sentinel instead of `0`.
    await appendTurns([
      fakeTurn({
        sessionId: 'mz-1',
        messageId: 'mz-1',
        usage: {
          input: 1000,
          output: 0,
          reasoning: 0,
          cacheRead: 1000,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
        fidelity: {
          granularity: 'per-turn',
          coverage: {
            hasInputTokens: true,
            hasOutputTokens: false,
            hasReasoningTokens: false,
            hasCacheReadTokens: true,
            hasCacheCreateTokens: false,
            hasToolCalls: false,
            hasToolResultEvents: false,
            hasSessionRelationships: false,
            hasRawContent: false,
          },
          class: 'partial',
        },
      }),
      fakeTurn({
        sessionId: 'mz-1',
        messageId: 'mz-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        usage: {
          input: 2000,
          output: 0,
          reasoning: 0,
          cacheRead: 1500,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
        fidelity: {
          granularity: 'per-turn',
          coverage: {
            hasInputTokens: true,
            hasOutputTokens: false,
            hasReasoningTokens: false,
            hasCacheReadTokens: true,
            hasCacheCreateTokens: false,
            hasToolCalls: false,
            hasToolResultEvents: false,
            hasSessionRelationships: false,
            hasRawContent: false,
          },
          class: 'partial',
        },
      }),
    ]);
    const out = await captureSummary({});
    assert.equal(out.code, 0);
    // Find the model row and assert the output column rendered as `—`,
    // never literal `0`.
    const modelLine = out.stdout
      .split('\n')
      .find((l) => l.includes('claude-sonnet-4-6'));
    assert.ok(modelLine, 'expected a model row in summary output');
    // Expect a `—` somewhere on the row (output and reasoning + cacheCreate
    // are all fully missing in this fixture).
    assert.ok(modelLine!.includes('—'), `expected a — in row: ${modelLine}`);
  });

  it('marks mixed cells with `*` and prints a single footer note', async () => {
    // One full-fidelity turn + one partial (missing output) for the same
    // model. The output column should carry the value with `*` and the
    // footer should appear exactly once.
    await appendTurns([
      fakeTurn({
        sessionId: 'mx-1',
        messageId: 'mx-1',
        fidelity: fullFidelity(),
      }),
      fakeTurn({
        sessionId: 'mx-1',
        messageId: 'mx-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        fidelity: partialMissingOutput(),
      }),
    ]);
    const out = await captureSummary({});
    assert.equal(out.code, 0);
    assert.ok(
      out.stdout.includes('*'),
      'expected a * partial marker on at least one cell',
    );
    const footerMatches = out.stdout.match(/\* partial coverage:/g) ?? [];
    assert.equal(footerMatches.length, 1, 'expected exactly one partial-coverage footer');
    // Denominator should be 2 (we appended 2 turns).
    assert.match(out.stdout, /partial coverage: \d+ of 2 turns/);
  });

  it('--json emits a fidelity block with summary + perCell', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'js-1',
        messageId: 'js-1',
        fidelity: fullFidelity(),
      }),
      fakeTurn({
        sessionId: 'js-1',
        messageId: 'js-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        fidelity: aggregateNoOutputOrReasoning(),
      }),
    ]);
    const out = await captureSummary({ json: true });
    assert.equal(out.code, 0);
    interface Payload {
      fidelity: {
        summary: { total: number; missingCoverage: Record<string, number> };
        perCell: {
          groupBy: string;
          cells: Array<{
            label: string;
            partial: boolean;
            fields: Record<string, { known: number; missing: number }>;
          }>;
        };
      };
    }
    const payload = JSON.parse(out.stdout) as Payload;
    // Summary shape: 2 turns, 1 missing output, 1 missing reasoning.
    assert.equal(payload.fidelity.summary.total, 2);
    assert.equal(payload.fidelity.summary.missingCoverage['hasOutputTokens'], 1);
    assert.equal(payload.fidelity.summary.missingCoverage['hasReasoningTokens'], 1);
    // perCell shape: one row keyed by model, partial=true, output known=1/missing=1.
    assert.equal(payload.fidelity.perCell.groupBy, 'model');
    assert.equal(payload.fidelity.perCell.cells.length, 1);
    const cell = payload.fidelity.perCell.cells[0]!;
    assert.equal(cell.partial, true);
    assert.deepEqual(cell.fields['output'], { known: 1, missing: 1 });
    assert.deepEqual(cell.fields['input'], { known: 2, missing: 0 });
  });

  it('treats records with no fidelity field as best-effort full (no partial marker)', async () => {
    // Pre-#41 records (no `fidelity` at all). They should be counted as
    // `known` for every field — so no partial marker, no footer.
    await appendTurns([
      fakeTurn({ sessionId: 'pf-1', messageId: 'pf-1' }),
      fakeTurn({
        sessionId: 'pf-1',
        messageId: 'pf-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    const out = await captureSummary({});
    assert.equal(out.code, 0);
    assert.equal(out.stdout.includes('*'), false);
    assert.equal(out.stdout.includes('partial coverage:'), false);
  });

  it('--by-tool emits attributedCost rows and the explanatory footer', async () => {
    // Two-turn session: turn 0 emits a Read tool_use; turn 1 ingests its
    // result. The input cost of turn 1 should attribute to Read.
    await appendTurns([
      fakeTurn({
        sessionId: 'bt-1',
        messageId: 'bt-1',
        toolCalls: [{ id: 'tc-1', name: 'Read', target: 'a.ts', argsHash: 'aa' }],
      }),
      fakeTurn({
        sessionId: 'bt-1',
        messageId: 'bt-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    const out = await captureSummary({ 'by-tool': true });
    assert.equal(out.code, 0);
    assert.match(out.stdout, /turns analyzed: 2/);
    assert.match(out.stdout, /Read/);
    assert.match(out.stdout, /attributedCost/);
    assert.match(out.stdout, /user-turn byte size/);
    assert.match(out.stdout, /unattributed cost/);
  });

  it('--by-tool --json emits { byTool, unattributed } with fidelity and even-split fallback', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'btj-1',
        messageId: 'btj-1',
        toolCalls: [{ id: 'tc-j', name: 'Edit', target: 'b.ts', argsHash: 'bb' }],
      }),
      fakeTurn({
        sessionId: 'btj-1',
        messageId: 'btj-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    const out = await captureSummary({ 'by-tool': true, json: true });
    assert.equal(out.code, 0);
    interface Payload {
      ingest: { ingestedSessions: number; appendedTurns: number };
      turns: number;
      byTool: Array<{
        tool: string;
        calls: number;
        attributedCost: number;
        attributionMethod: 'sized' | 'even-split' | 'unattributed';
      }>;
      unattributed: number;
      fidelity: { summary: unknown };
    }
    const payload = JSON.parse(out.stdout) as Payload;
    assert.equal(payload.turns, 2);
    assert.ok(Array.isArray(payload.byTool));
    const edit = payload.byTool.find((r) => r.tool === 'Edit');
    assert.ok(edit);
    assert.equal(edit!.attributionMethod, 'even-split');
    assert.equal(typeof payload.unattributed, 'number');
    assert.ok(payload.fidelity.summary, 'fidelity.summary block expected');
  });

  it('--by-tool --json uses user-turn byteLen for proportional attribution', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'bt-sized',
        messageId: 'bt-sized-1',
        toolCalls: [
          { id: 'tc-big', name: 'Read', target: 'large.log', argsHash: 'read-big' },
          { id: 'tc-small', name: 'Bash', target: 'true', argsHash: 'bash-small' },
        ],
      }),
      fakeTurn({
        sessionId: 'bt-sized',
        messageId: 'bt-sized-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    await appendUserTurns([
      fakeUserTurn({
        sessionId: 'bt-sized',
        userUuid: 'bt-sized-u-1',
        precedingMessageId: 'bt-sized-1',
        followingMessageId: 'bt-sized-2',
        blocks: [
          { kind: 'tool_result', toolUseId: 'tc-big', byteLen: 9000, approxTokens: 2250 },
          {
            kind: 'tool_result',
            toolUseId: 'tc-small',
            byteLen: 1000,
            approxTokens: 250,
            isError: true,
          },
          { kind: 'text', byteLen: 50_000, approxTokens: 12_500 },
        ],
      }),
    ]);

    const out = await captureSummary({ 'by-tool': true, json: true });
    assert.equal(out.code, 0);
    interface Payload {
      byTool: Array<{
        tool: string;
        attributedCost: number;
        attributionMethod: 'sized' | 'even-split' | 'unattributed';
      }>;
    }
    const payload = JSON.parse(out.stdout) as Payload;
    const read = payload.byTool.find((r) => r.tool === 'Read');
    const bash = payload.byTool.find((r) => r.tool === 'Bash');
    assert.ok(read);
    assert.ok(bash);
    assert.equal(read!.attributionMethod, 'sized');
    assert.equal(bash!.attributionMethod, 'sized');
    assert.ok(
      read!.attributedCost > bash!.attributedCost * 8,
      `Read should dominate Bash by byteLen: read=${read!.attributedCost} bash=${bash!.attributedCost}`,
    );
  });

  it('--by-tool combined with --by-provider exits non-zero with a clear error', async () => {
    const out = await captureSummary({ 'by-tool': true, 'by-provider': true });
    assert.equal(out.code, 2);
    assert.match(
      out.stderr,
      /--by-tool cannot be combined with --by-provider\/--by-subagent-type\/--by-relationship\/--subagent-tree/,
    );
  });

  it('--by-provider combined with subagent modes exits non-zero with a clear error', async () => {
    const byType = await captureSummary({ 'by-provider': true, 'by-subagent-type': true });
    assert.equal(byType.code, 2);
    assert.match(
      byType.stderr,
      /--by-provider cannot be combined with --by-subagent-type\/--by-relationship\/--subagent-tree/,
    );

    const tree = await captureSummary({ 'by-provider': true, 'subagent-tree': 's-1' });
    assert.equal(tree.code, 2);
    assert.match(
      tree.stderr,
      /--by-provider cannot be combined with --by-subagent-type\/--by-relationship\/--subagent-tree/,
    );
  });

  it('footer N sums per-field missing across rows (multi-model regression)', async () => {
    // Devin-review regression: with two model rows that each have some
    // turns missing output, the footer denominator should report the
    // cross-row sum of `missing` for the worst-covered field — not the
    // per-(row, field) max. Two rows × 2 missing-output turns each → 4.
    const partialOutput = (model: string, sessionId: string, msg: string, ts: string) =>
      fakeTurn({
        sessionId,
        messageId: msg,
        ts,
        model,
        fidelity: partialMissingOutput(),
      });
    await appendTurns([
      // Model A: 1 full + 2 partial-output
      fakeTurn({
        sessionId: 'mr-A',
        messageId: 'mr-A-1',
        model: 'claude-sonnet-4-6',
        fidelity: fullFidelity(),
      }),
      partialOutput('claude-sonnet-4-6', 'mr-A', 'mr-A-2', '2026-04-20T00:01:00.000Z'),
      partialOutput('claude-sonnet-4-6', 'mr-A', 'mr-A-3', '2026-04-20T00:02:00.000Z'),
      // Model B: 1 full + 2 partial-output
      fakeTurn({
        sessionId: 'mr-B',
        messageId: 'mr-B-1',
        ts: '2026-04-20T00:03:00.000Z',
        model: 'claude-haiku-4-5',
        fidelity: fullFidelity(),
      }),
      partialOutput('claude-haiku-4-5', 'mr-B', 'mr-B-2', '2026-04-20T00:04:00.000Z'),
      partialOutput('claude-haiku-4-5', 'mr-B', 'mr-B-3', '2026-04-20T00:05:00.000Z'),
    ]);
    const out = await captureSummary({});
    assert.equal(out.code, 0);
    // 4 turns missing output across 6 total. The pre-fix bug took the
    // per-(row, field) max (2 instead of 4); this assertion would have
    // failed against that arithmetic.
    assert.match(out.stdout, /partial coverage: 4 of 6 turns/);
  });
});

describe('burn summary replacement-tool savings (#219)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalArchive = process.env['RELAYBURN_ARCHIVE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-summary-savings-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-summary-savings-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    process.env['RELAYBURN_ARCHIVE'] = '0';
    __resetIndexCacheForTesting();
  });

  afterEach(async () => {
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalArchive !== undefined) process.env['RELAYBURN_ARCHIVE'] = originalArchive;
    else delete process.env['RELAYBURN_ARCHIVE'];
  });

  it('default JSON exposes a replacementSavings block when annotations are present', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-rs',
        messageId: 'rs-1',
        toolCalls: [
          {
            id: 'tu-1',
            name: 'relaywash__Search',
            argsHash: 'h',
            replacedTools: ['Glob', 'Grep', 'Read'],
            collapsedCalls: 9,
          },
        ],
      }),
    ]);
    const out = await captureSummary({ json: true });
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as {
      replacementSavings?: {
        calls: number;
        collapsedCalls: number;
        estimatedTokensSaved: number;
        byTool: Array<{ tool: string; calls: number }>;
      };
    };
    assert.ok(parsed.replacementSavings, 'replacementSavings block emitted');
    assert.equal(parsed.replacementSavings!.calls, 1);
    assert.equal(parsed.replacementSavings!.collapsedCalls, 9);
    assert.ok(parsed.replacementSavings!.estimatedTokensSaved > 0);
    assert.equal(parsed.replacementSavings!.byTool[0]!.tool, 'relaywash__Search');
  });

  it('omits the replacementSavings block when no turn carries annotations', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-no-rs', messageId: 'no-rs-1' })]);
    const out = await captureSummary({ json: true });
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as { replacementSavings?: unknown };
    assert.equal(parsed.replacementSavings, undefined);
  });

  it('--by-tool --json attaches a savings field to rows that carry annotations', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-bt',
        messageId: 'bt-1',
        toolCalls: [
          {
            id: 'tu-bt-1',
            name: 'relaywash__Search',
            argsHash: 'h',
            replacedTools: ['Read'],
            collapsedCalls: 4,
          },
          { id: 'tu-bt-2', name: 'Bash', argsHash: 'h2' },
        ],
      }),
      fakeTurn({
        sessionId: 's-bt',
        messageId: 'bt-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    const out = await captureSummary({ json: true, 'by-tool': true });
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as {
      byTool: Array<{
        tool: string;
        savings?: { calls: number; collapsedCalls: number; estimatedTokensSaved: number };
      }>;
      replacementSavings?: { estimatedTokensSaved: number };
    };
    const search = parsed.byTool.find((r) => r.tool === 'relaywash__Search');
    const bash = parsed.byTool.find((r) => r.tool === 'Bash');
    assert.ok(search);
    assert.ok(bash);
    assert.ok(search!.savings, 'savings present on annotated tool row');
    assert.equal(search!.savings!.collapsedCalls, 4);
    assert.equal(bash!.savings, undefined);
    assert.ok(parsed.replacementSavings!.estimatedTokensSaved > 0);
  });
});
