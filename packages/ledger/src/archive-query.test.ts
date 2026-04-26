import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, beforeEach, describe, it } from 'node:test';

import type { TurnRecord } from '@relayburn/reader';

import { __resetIndexCacheForTesting } from './index-sidecar.js';
import { appendTurns, stamp } from './writer.js';
import { queryAll, type EnrichedTurn } from './reader.js';
import { buildArchive } from './archive.js';
import { archiveAvailable, queryAllFromArchive } from './archive-query.js';

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

// Strip the synthesized `fidelity.coverage` block before comparing — the
// archive doesn't persist the full coverage shape (#110), only the class /
// granularity / tokens-present / cost-present projection. Class equality is the
// load-bearing parity guarantee `summarizeFidelity` cares about; coverage may
// differ shape-for-shape between the streamed `queryAll` and the synthesized
// archive row when the source populated coverage flags asymmetrically.
function normalizeForCompare(t: EnrichedTurn): EnrichedTurn {
  const out = { ...t } as EnrichedTurn;
  if (out.fidelity) {
    out.fidelity = {
      class: out.fidelity.class,
      granularity: out.fidelity.granularity,
      coverage: out.fidelity.coverage,
    };
  }
  return out;
}

describe('archive-query', () => {
  let tmpDir: string;
  const originalHome = process.env['RELAYBURN_HOME'];

  before(async () => {
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-archive-query-test-'));
  });

  beforeEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-archive-query-test-'));
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

  it('archiveAvailable() is false before the first build, true after', async () => {
    assert.equal(await archiveAvailable(), false);
    await appendTurns([fakeTurn({ sessionId: 's-a', messageId: 'm-a' })]);
    await buildArchive();
    assert.equal(await archiveAvailable(), true);
  });

  it('returns the same turn count as queryAll for an unfiltered query', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-A', messageId: 'm-1' }),
      fakeTurn({
        sessionId: 's-A',
        messageId: 'm-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
      fakeTurn({
        sessionId: 's-B',
        messageId: 'm-3',
        ts: '2026-04-20T00:02:00.000Z',
        project: '/tmp/other',
      }),
    ]);
    await buildArchive();

    const fromLedger = await queryAll();
    const fromArchive = await queryAllFromArchive();
    assert.equal(fromArchive.length, fromLedger.length);
    assert.equal(fromArchive.length, 3);
  });

  it('parity: per-turn shape matches queryAll for project + workflow filters', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-P',
        messageId: 'mp-1',
        toolCalls: [
          { id: 'tu-1', name: 'Read', target: '/tmp/foo.ts', argsHash: 'a1' },
          { id: 'tu-2', name: 'Edit', target: '/tmp/foo.ts', argsHash: 'a2', isError: false },
        ],
      }),
      fakeTurn({
        sessionId: 's-P',
        messageId: 'mp-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
      fakeTurn({
        sessionId: 's-Q',
        messageId: 'mq-1',
        ts: '2026-04-20T00:02:00.000Z',
        project: '/tmp/other',
      }),
    ]);
    await stamp({ sessionId: 's-P' }, { workflowId: 'wf-42', persona: 'eng' });
    await buildArchive();

    // Project filter — should match both s-P turns and exclude s-Q.
    {
      const ledger = (await queryAll({ project: '/tmp/project' })).map(normalizeForCompare);
      const archive = (await queryAllFromArchive({ project: '/tmp/project' })).map(
        normalizeForCompare,
      );
      assert.equal(archive.length, ledger.length);
      assert.equal(archive.length, 2);
      // Same set of message ids, regardless of sort order.
      const archiveIds = new Set(archive.map((t) => t.messageId));
      const ledgerIds = new Set(ledger.map((t) => t.messageId));
      assert.deepEqual([...archiveIds].sort(), [...ledgerIds].sort());

      // Cross-check that the first row's enrichment + tool calls survived the
      // archive round-trip.
      const a = archive.find((t) => t.messageId === 'mp-1')!;
      const l = ledger.find((t) => t.messageId === 'mp-1')!;
      assert.equal(a.enrichment['workflowId'], l.enrichment['workflowId']);
      assert.equal(a.enrichment['workflowId'], 'wf-42');
      assert.equal(a.enrichment['persona'], 'eng');
      assert.equal(a.toolCalls.length, 2);
      assert.equal(a.toolCalls.length, l.toolCalls.length);
      assert.equal(a.toolCalls[0]!.name, 'Read');
      assert.equal(a.toolCalls[1]!.isError, false);
      assert.equal(a.usage.input, l.usage.input);
      assert.equal(a.usage.output, l.usage.output);
    }

    // Workflow filter — only the stamped session should land.
    {
      const ledger = (await queryAll({ enrichment: { workflowId: 'wf-42' } })).map(
        normalizeForCompare,
      );
      const archive = (
        await queryAllFromArchive({ enrichment: { workflowId: 'wf-42' } })
      ).map(normalizeForCompare);
      assert.equal(archive.length, 2);
      assert.equal(archive.length, ledger.length);
      for (const t of archive) {
        assert.equal(t.sessionId, 's-P');
      }
    }
  });

  it('honors since/until window filters the same way queryAll does', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-W',
        messageId: 'mw-1',
        ts: '2026-04-20T00:00:00.000Z',
      }),
      fakeTurn({
        sessionId: 's-W',
        messageId: 'mw-2',
        turnIndex: 1,
        ts: '2026-04-21T00:00:00.000Z',
      }),
      fakeTurn({
        sessionId: 's-W',
        messageId: 'mw-3',
        turnIndex: 2,
        ts: '2026-04-22T00:00:00.000Z',
      }),
    ]);
    await buildArchive();

    const since = '2026-04-21T00:00:00.000Z';
    const ledger = await queryAll({ since });
    const archive = await queryAllFromArchive({ since });
    assert.equal(archive.length, ledger.length);
    assert.equal(archive.length, 2);
    for (const t of archive) {
      assert.ok(t.ts >= since, `turn ts ${t.ts} should be >= since`);
    }
  });

  it('hydrates subagent block when subagent fields are populated', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-S',
        messageId: 'ms-1',
        subagent: {
          isSidechain: true,
          agentId: 'agent-A',
          parentAgentId: 'agent-root',
          parentToolUseId: 'tu-spawn',
          subagentType: 'investigator',
        },
      }),
      // Plain turn — no subagent block expected on the way back out.
      fakeTurn({
        sessionId: 's-S',
        messageId: 'ms-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    await buildArchive();
    const archive = await queryAllFromArchive({ sessionId: 's-S' });
    assert.equal(archive.length, 2);
    const sub = archive.find((t) => t.messageId === 'ms-1')!;
    assert.ok(sub.subagent, 'subagent block should be present');
    assert.equal(sub.subagent.isSidechain, true);
    assert.equal(sub.subagent.agentId, 'agent-A');
    assert.equal(sub.subagent.parentAgentId, 'agent-root');
    assert.equal(sub.subagent.parentToolUseId, 'tu-spawn');
    assert.equal(sub.subagent.subagentType, 'investigator');

    const plain = archive.find((t) => t.messageId === 'ms-2')!;
    assert.equal(plain.subagent, undefined);
  });

  it('preserves fidelity class so summarizeFidelity buckets match the streaming reader', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-F',
        messageId: 'mf-1',
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
        sessionId: 's-F',
        messageId: 'mf-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        fidelity: {
          granularity: 'cost-only',
          coverage: {
            hasInputTokens: false,
            hasOutputTokens: false,
            hasReasoningTokens: false,
            hasCacheReadTokens: false,
            hasCacheCreateTokens: false,
            hasToolCalls: false,
            hasToolResultEvents: false,
            hasSessionRelationships: false,
            hasRawContent: false,
          },
          class: 'cost-only',
        },
      }),
      // Older row with no fidelity at all → archive row has all NULLs and the
      // synthesizer must round-trip that back to `fidelity = undefined`.
      fakeTurn({
        sessionId: 's-F',
        messageId: 'mf-3',
        turnIndex: 2,
        ts: '2026-04-20T00:02:00.000Z',
      }),
    ]);
    await buildArchive();
    const archive = await queryAllFromArchive({ sessionId: 's-F' });
    const byId = new Map(archive.map((t) => [t.messageId, t]));
    assert.equal(byId.get('mf-1')!.fidelity?.class, 'full');
    assert.equal(byId.get('mf-2')!.fidelity?.class, 'cost-only');
    assert.equal(byId.get('mf-2')!.fidelity?.granularity, 'cost-only');
    assert.equal(byId.get('mf-3')!.fidelity, undefined);
  });

  it('returns an empty array when no rows match (no SQL parameter errors on empty filters)', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-E', messageId: 'me-1' })]);
    await buildArchive();
    const out = await queryAllFromArchive({ sessionId: 'does-not-exist' });
    assert.deepEqual(out, []);
  });
});
