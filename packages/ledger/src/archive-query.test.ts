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
import {
  archiveAvailable,
  queryAllFromArchive,
  queryTurnsFromArchive,
} from './archive-query.js';

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

  it('round-trips ToolCall.replacedTools and ToolCall.collapsedCalls through the archive (#219)', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-RS',
        messageId: 'mrs-1',
        toolCalls: [
          {
            id: 'tu-rs-1',
            name: 'relaywash__Search',
            argsHash: 'rs1',
            replacedTools: ['Glob', 'Grep', 'Read'],
            collapsedCalls: 9,
          },
          // Vanilla call without annotations — both fields should survive as
          // undefined on the read side.
          { id: 'tu-rs-2', name: 'Bash', argsHash: 'rs2' },
        ],
      }),
    ]);
    await buildArchive();
    const archive = await queryAllFromArchive({ sessionId: 's-RS' });
    assert.equal(archive.length, 1);
    const calls = archive[0]!.toolCalls;
    const search = calls.find((c) => c.name === 'relaywash__Search');
    const bash = calls.find((c) => c.name === 'Bash');
    assert.ok(search);
    assert.ok(bash);
    assert.deepEqual(search!.replacedTools, ['Glob', 'Grep', 'Read']);
    assert.equal(search!.collapsedCalls, 9);
    assert.equal(bash!.replacedTools, undefined);
    assert.equal(bash!.collapsedCalls, undefined);
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
          description: 'find auth-flow regressions',
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
    assert.equal(sub.subagent.description, 'find auth-flow regressions');

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
