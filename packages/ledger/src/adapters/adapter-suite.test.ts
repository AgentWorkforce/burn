// Parameterized contract tests run against every StorageAdapter
// implementation. Phase 2 of #139 adds SqliteAdapter, so the same scenarios
// — append/dedup/query/lock/content semantics — must hold for both. Each
// `runAdapterSuite(name, makeAdapter)` call registers a `describe(name)`
// block; adding a new adapter (Postgres, HttpAdapter) is one line at the
// bottom of this file.

import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, afterEach, before, beforeEach, describe, it } from 'node:test';

import type {
  CompactionEvent,
  ContentRecord,
  SessionRelationshipRecord,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import { __resetIndexCacheForTesting } from '../index-sidecar.js';
import type { PruneOptions } from '../content.js';
import type { StampLine } from '../schema.js';
import type { StorageAdapter } from './adapter.js';
import { FileAdapter } from './file-adapter.js';
import { SqliteAdapter } from './sqlite-adapter.js';

interface AdapterContext {
  adapter: StorageAdapter;
  // Some scenarios (file-adapter content-fingerprint dedup, file-adapter
  // index cache) need to clear shared module-level state between tests.
  resetSharedState?: () => void;
}

type MakeAdapter = (tmpDir: string) => Promise<AdapterContext>;

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

function fakeUserTurn(overrides: Partial<UserTurnRecord> = {}): UserTurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    userUuid: 'user-1',
    ts: '2026-04-20T00:00:00.500Z',
    precedingMessageId: 'msg-1',
    followingMessageId: 'msg-2',
    blocks: [{ kind: 'tool_result', toolUseId: 'tu-1', byteLen: 4000, approxTokens: 1000 }],
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
    relatedSessionId: 's-parent',
    relationshipType: 'continuation',
    ts: '2026-04-20T00:00:00.000Z',
    ...overrides,
  };
}

function fakeToolResultEvent(
  overrides: Partial<ToolResultEventRecord> = {},
): ToolResultEventRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    messageId: 'msg-1',
    toolUseId: 'tu-1',
    eventIndex: 0,
    status: 'completed',
    eventSource: 'tool_result',
    ts: '2026-04-20T00:00:00.250Z',
    ...overrides,
  };
}

function fakeCompaction(overrides: Partial<CompactionEvent> = {}): CompactionEvent {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    ts: '2026-04-20T00:00:00.000Z',
    precedingMessageId: 'msg-1',
    tokensBeforeCompact: 12345,
    ...overrides,
  };
}

function fakeContent(overrides: Partial<ContentRecord> = {}): ContentRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    messageId: 'msg-1',
    ts: '2026-04-20T00:00:00.000Z',
    role: 'assistant',
    kind: 'text',
    text: 'hello',
    ...overrides,
  };
}

async function collect<T>(it: AsyncIterable<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const v of it) out.push(v);
  return out;
}

function runAdapterSuite(name: string, makeAdapter: MakeAdapter): void {
  describe(`StorageAdapter contract: ${name}`, () => {
    let tmpDir: string;
    let ctx: AdapterContext;
    const originalHome = process.env['RELAYBURN_HOME'];

    before(async () => {
      tmpDir = await mkdtemp(path.join(tmpdir(), `relayburn-adapter-${name}-`));
    });

    beforeEach(async () => {
      await rm(tmpDir, { recursive: true, force: true });
      tmpDir = await mkdtemp(path.join(tmpdir(), `relayburn-adapter-${name}-`));
      // FileAdapter still keys off RELAYBURN_HOME for its on-disk layout;
      // SqliteAdapter takes the path as a constructor arg, but pointing
      // RELAYBURN_HOME at the same dir keeps `withLock` (and any side-paths
      // either adapter happens to read) consistent.
      process.env['RELAYBURN_HOME'] = tmpDir;
      __resetIndexCacheForTesting();
      ctx = await makeAdapter(tmpDir);
      await ctx.adapter.init();
    });

    afterEach(async () => {
      await ctx.adapter.close();
      ctx.resetSharedState?.();
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
      await ctx.adapter.appendTurns([
        fakeTurn(),
        fakeTurn({ messageId: 'msg-2', turnIndex: 1, ts: '2026-04-20T00:00:01.000Z' }),
      ]);
      const got = await collect(ctx.adapter.queryTurns({}));
      assert.equal(got.length, 2);
      const ids = got.map((t) => t.messageId).sort();
      assert.deepEqual(ids, ['msg-1', 'msg-2']);
    });

    it('dedupes turns appended with the same id_hash inputs', async () => {
      await ctx.adapter.appendTurns([fakeTurn()]);
      await ctx.adapter.appendTurns([fakeTurn()]);
      const got = await collect(ctx.adapter.queryTurns({}));
      assert.equal(got.length, 1);
    });

    it('dedupes compactions / relationships / tool result events / user turns', async () => {
      await ctx.adapter.appendCompactions([fakeCompaction()]);
      await ctx.adapter.appendCompactions([fakeCompaction()]);
      await ctx.adapter.appendRelationships([fakeRelationship()]);
      await ctx.adapter.appendRelationships([fakeRelationship()]);
      await ctx.adapter.appendToolResultEvents([fakeToolResultEvent()]);
      await ctx.adapter.appendToolResultEvents([fakeToolResultEvent()]);
      await ctx.adapter.appendUserTurns([fakeUserTurn()]);
      await ctx.adapter.appendUserTurns([fakeUserTurn()]);

      assert.equal((await collect(ctx.adapter.queryCompactions({}))).length, 1);
      assert.equal((await collect(ctx.adapter.queryRelationships({}))).length, 1);
      assert.equal((await collect(ctx.adapter.queryToolResultEvents({}))).length, 1);
      assert.equal((await collect(ctx.adapter.queryUserTurns({}))).length, 1);
    });

    it('filters queryTurns by sessionId / source', async () => {
      await ctx.adapter.appendTurns([
        fakeTurn({ sessionId: 's-A', messageId: 'm-A1' }),
        // FileAdapter content-fingerprint dedup keys on (ts, model, usage,
        // first-tool argsHash). Bump ts so the second turn doesn't get
        // collapsed against the first one.
        fakeTurn({
          sessionId: 's-B',
          messageId: 'm-B1',
          source: 'codex',
          ts: '2026-04-20T00:00:01.000Z',
        }),
      ]);
      const a = await collect(ctx.adapter.queryTurns({ sessionId: 's-A' }));
      const codex = await collect(ctx.adapter.queryTurns({ source: 'codex' }));
      assert.equal(a.length, 1);
      assert.equal(a[0]!.sessionId, 's-A');
      assert.equal(codex.length, 1);
      assert.equal(codex[0]!.source, 'codex');
    });

    it('folds a sessionId stamp onto matching turns', async () => {
      await ctx.adapter.appendTurns([
        fakeTurn({ sessionId: 's-A', messageId: 'm-A1' }),
        // Distinct ts so FileAdapter content-fingerprint dedup doesn't
        // collapse the second turn.
        fakeTurn({
          sessionId: 's-B',
          messageId: 'm-B1',
          ts: '2026-04-20T00:00:01.000Z',
        }),
      ]);
      const stamp: StampLine = {
        v: 1,
        kind: 'stamp',
        ts: new Date().toISOString(),
        selector: { sessionId: 's-A' },
        enrichment: { workflowId: 'wf-1', agentId: 'ag-42' },
      };
      await ctx.adapter.appendStamp(stamp);
      const got = await collect(ctx.adapter.queryTurns({}));
      const a = got.find((t) => t.sessionId === 's-A')!;
      const b = got.find((t) => t.sessionId === 's-B')!;
      assert.equal(a.enrichment['workflowId'], 'wf-1');
      assert.equal(a.enrichment['agentId'], 'ag-42');
      assert.deepEqual(b.enrichment, {});
    });

    it('synthesizes a spawn-env subagent relationship from a parentAgentId stamp', async () => {
      const stamp: StampLine = {
        v: 1,
        kind: 'stamp',
        ts: '2026-04-20T00:00:00.000Z',
        selector: { sessionId: 's-child' },
        enrichment: { parentAgentId: 'ag-parent', agentId: 'ag-child' },
      };
      await ctx.adapter.appendStamp(stamp);
      const rels = await collect(ctx.adapter.queryRelationships({}));
      assert.equal(rels.length, 1);
      assert.equal(rels[0]!.source, 'spawn-env');
      assert.equal(rels[0]!.sessionId, 's-child');
      assert.equal(rels[0]!.relatedSessionId, 'ag-parent');
      assert.equal(rels[0]!.relationshipType, 'subagent');
      assert.equal(rels[0]!.agentId, 'ag-child');
    });

    it('round-trips content with per-session listing', async () => {
      await ctx.adapter.appendContent([
        fakeContent({ sessionId: 's-A', messageId: 'm-A1' }),
        fakeContent({ sessionId: 's-A', messageId: 'm-A2', text: 'second' }),
        fakeContent({ sessionId: 's-B', messageId: 'm-B1' }),
      ]);
      const ids = (await ctx.adapter.listContentSessionIds()).sort();
      assert.deepEqual(ids, ['s-A', 's-B']);

      const a = await collect(ctx.adapter.readContent({ sessionId: 's-A' }));
      assert.equal(a.length, 2);

      const onlyA1 = await collect(
        ctx.adapter.readContent({ sessionId: 's-A', messageId: 'm-A1' }),
      );
      assert.equal(onlyA1.length, 1);
      assert.equal(onlyA1[0]!.messageId, 'm-A1');
    });

    it('dedupes identical content records within a session', async () => {
      await ctx.adapter.appendContent([fakeContent({ sessionId: 's-A' })]);
      await ctx.adapter.appendContent([fakeContent({ sessionId: 's-A' })]);
      const a = await collect(ctx.adapter.readContent({ sessionId: 's-A' }));
      assert.equal(a.length, 1);
    });

    it('pruneContent deletes session content older than the cutoff', async () => {
      await ctx.adapter.appendContent([fakeContent({ sessionId: 's-A' })]);
      // Wait so the freshly-written records are unambiguously past the
      // cutoff when we set olderThanMs to a value below their age.
      await new Promise((r) => setTimeout(r, 25));
      const opts: PruneOptions = { olderThanMs: 10 };
      const result = await ctx.adapter.pruneContent(opts);
      assert.equal(result.filesDeleted, 1);
      assert(result.bytesFreed >= 0);
      const remaining = await collect(ctx.adapter.readContent({ sessionId: 's-A' }));
      assert.equal(remaining.length, 0);
    });

    it('pruneContent honors isRecoverable predicate', async () => {
      await ctx.adapter.appendContent([fakeContent({ sessionId: 's-keep' })]);
      await ctx.adapter.appendContent([fakeContent({ sessionId: 's-drop' })]);
      await new Promise((r) => setTimeout(r, 25));
      const result = await ctx.adapter.pruneContent({
        olderThanMs: 10,
        isRecoverable: (sid) => sid === 's-keep',
      });
      assert.equal(result.filesDeleted, 1);
      assert.equal(result.skippedRecoverable, 1);
      const kept = await collect(ctx.adapter.readContent({ sessionId: 's-keep' }));
      assert.equal(kept.length, 1);
      const dropped = await collect(ctx.adapter.readContent({ sessionId: 's-drop' }));
      assert.equal(dropped.length, 0);
    });

    it('withLock is re-entrant within the same async context', async () => {
      // The outer lock holds the named row; the nested call must short-
      // circuit on AsyncLocalStorage instead of trying to re-acquire and
      // deadlocking on its own row.
      const result = await ctx.adapter.withLock('demo', async () => {
        return await ctx.adapter.withLock('demo', async () => 42);
      });
      assert.equal(result, 42);
    });

    it('withLock serializes concurrent acquirers of the same name', async () => {
      let inside = 0;
      let maxInside = 0;
      const work = async () => {
        await ctx.adapter.withLock('serialize', async () => {
          inside++;
          if (inside > maxInside) maxInside = inside;
          await new Promise((r) => setTimeout(r, 5));
          inside--;
        });
      };
      await Promise.all([work(), work(), work()]);
      assert.equal(maxInside, 1, 'expected only one holder at a time');
    });
  });
}

// ---------------------------------------------------------------------
// Adapter registrations.
// ---------------------------------------------------------------------

runAdapterSuite('file', async (tmpDir) => {
  // FileAdapter still resolves paths from RELAYBURN_HOME; the beforeEach
  // already pointed it at tmpDir. Reset the in-memory dedup cache between
  // tests so a previous test's hashes don't bleed into this one.
  void tmpDir;
  return {
    adapter: new FileAdapter(),
    resetSharedState: () => __resetIndexCacheForTesting(),
  };
});

runAdapterSuite('sqlite', async (tmpDir) => {
  const dbPath = path.join(tmpDir, 'burn.sqlite');
  return {
    adapter: new SqliteAdapter({ dbPath, staleMs: 200 }),
  };
});

// SqliteAdapter-specific: concurrent writers against the same file should
// converge on a single deduplicated row set with no SQLITE_BUSY surfacing.
// This exercises the verification scenario called out in #141.
describe('SqliteAdapter concurrent writers', () => {
  let tmpDir: string;
  beforeEach(async () => {
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-sqlite-concurrent-'));
  });
  afterEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
  });

  it('two SqliteAdapters on the same DB file converge to one row per turn', async () => {
    const dbPath = path.join(tmpDir, 'burn.sqlite');
    const a = new SqliteAdapter({ dbPath, staleMs: 200 });
    const b = new SqliteAdapter({ dbPath, staleMs: 200 });
    await a.init();
    await b.init();
    try {
      const turns = Array.from({ length: 20 }, (_, i) =>
        fakeTurn({
          messageId: `msg-${i}`,
          turnIndex: i,
          ts: new Date(1745539200000 + i * 1000).toISOString(),
        }),
      );
      // Same input from both writers — the adapters compete on every row.
      // INSERT OR IGNORE collapses the duplicates without raising.
      await Promise.all([a.appendTurns(turns), b.appendTurns(turns)]);
      const got = await collect(a.queryTurns({}));
      assert.equal(got.length, turns.length);
    } finally {
      await a.close();
      await b.close();
    }
  });
});
