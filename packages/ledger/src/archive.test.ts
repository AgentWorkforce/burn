import { strict as assert } from 'node:assert';
import { mkdtemp, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { after, before, beforeEach, describe, it } from 'node:test';

import type { ToolResultEventRecord, TurnRecord } from '@relayburn/reader';

import { __resetIndexCacheForTesting } from './index-sidecar.js';
import {
  appendCompactions,
  appendToolResultEvents,
  appendTurns,
  stamp,
} from './writer.js';
import { archivePath, ledgerPath } from './paths.js';
import {
  ARCHIVE_VERSION,
  buildArchive,
  getArchiveStatus,
  openArchive,
  rebuildArchive,
  vacuumArchive,
} from './archive.js';
import { withLock } from './lock.js';

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

describe('archive', () => {
  let tmpDir: string;
  const originalHome = process.env['RELAYBURN_HOME'];

  before(async () => {
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-archive-test-'));
  });

  beforeEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-archive-test-'));
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

  it('build with no ledger is a no-op and reports zero counts', async () => {
    const result = await buildArchive();
    assert.equal(result.turnsApplied, 0);
    assert.equal(result.sessionsTouched, 0);
    const status = await getArchiveStatus();
    assert.equal(status.exists, true);
    assert.equal(status.rowCounts.turns, 0);
    assert.equal(status.archiveVersion, ARCHIVE_VERSION);
  });

  it('materializes turns and tool_calls from the ledger', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-A',
        messageId: 'm-1',
        toolCalls: [
          { id: 'tu-1', name: 'Read', target: '/tmp/foo.ts', argsHash: 'a1' },
          { id: 'tu-2', name: 'Edit', target: '/tmp/foo.ts', argsHash: 'a2', isError: false },
        ],
      }),
      fakeTurn({
        sessionId: 's-A',
        messageId: 'm-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    const result = await buildArchive();
    assert.equal(result.turnsApplied, 2);
    assert.equal(result.sessionsTouched, 1);

    const db = await openArchive();
    try {
      const rowCount = (db.prepare('SELECT COUNT(*) AS n FROM turns').get() as {
        n: number | bigint;
      }).n;
      assert.equal(Number(rowCount), 2);
      const toolRows = (db.prepare('SELECT COUNT(*) AS n FROM tool_calls').get() as {
        n: number | bigint;
      }).n;
      assert.equal(Number(toolRows), 2);
      const toolNames = (db
        .prepare(
          'SELECT tool_name FROM tool_calls WHERE message_id = ? ORDER BY call_index',
        )
        .all('m-1') as Array<{ tool_name: string }>).map((r) => r.tool_name);
      assert.deepEqual(toolNames, ['Read', 'Edit']);
      const sessionRow = db
        .prepare('SELECT turn_count, started_at, ended_at FROM sessions WHERE session_id = ?')
        .get('s-A') as { turn_count: number | bigint; started_at: string; ended_at: string };
      assert.equal(Number(sessionRow.turn_count), 2);
      assert.equal(sessionRow.started_at, '2026-04-20T00:00:00.000Z');
      assert.equal(sessionRow.ended_at, '2026-04-20T00:01:00.000Z');
    } finally {
      db.close();
    }
  });

  it('folds stamps into materialized enrichment columns', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-X', messageId: 'mx-1' }),
    ]);
    await stamp({ sessionId: 's-X' }, { workflowId: 'wf-42', persona: 'eng', tier: 'best' });
    await buildArchive();
    const db = await openArchive();
    try {
      const row = db
        .prepare('SELECT workflow_id, persona, tier, enrichment_json FROM turns WHERE message_id = ?')
        .get('mx-1') as { workflow_id: string; persona: string; tier: string; enrichment_json: string };
      assert.equal(row.workflow_id, 'wf-42');
      assert.equal(row.persona, 'eng');
      assert.equal(row.tier, 'best');
      const parsed = JSON.parse(row.enrichment_json) as Record<string, string>;
      assert.equal(parsed['workflowId'], 'wf-42');
    } finally {
      db.close();
    }
  });

  it('refolds enrichment when a stamp arrives after the turn was materialized', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-late', messageId: 'ml-1' })]);
    // First build: no stamp yet.
    await buildArchive();
    {
      const db = await openArchive();
      try {
        const row = db
          .prepare('SELECT workflow_id FROM turns WHERE message_id = ?')
          .get('ml-1') as { workflow_id: string | null };
        assert.equal(row.workflow_id, null);
      } finally {
        db.close();
      }
    }
    // Stamp arrives; rebuild only the tail.
    await stamp({ sessionId: 's-late' }, { workflowId: 'wf-99' });
    const result = await buildArchive();
    assert.equal(result.stampsApplied, 1);
    {
      const db = await openArchive();
      try {
        const row = db
          .prepare('SELECT workflow_id FROM turns WHERE message_id = ?')
          .get('ml-1') as { workflow_id: string };
        assert.equal(row.workflow_id, 'wf-99');
      } finally {
        db.close();
      }
    }
  });

  it('is incrementally idempotent: rebuilding the same ledger twice yields stable counts', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-i', messageId: 'mi-1' }),
      fakeTurn({ sessionId: 's-i', messageId: 'mi-2', turnIndex: 1, ts: '2026-04-20T00:00:01.000Z' }),
    ]);
    const r1 = await buildArchive();
    assert.equal(r1.turnsApplied, 2);
    // Calling again with no new ledger writes is a clean no-op.
    const r2 = await buildArchive();
    assert.equal(r2.turnsApplied, 0);
    assert.equal(r2.scannedBytes, 0);
    const status = await getArchiveStatus();
    assert.equal(status.upToDate, true);
    assert.equal(status.rowCounts.turns, 2);
  });

  it('rebuilds from zero deterministically: same ledger yields same row counts', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-r', messageId: 'mr-1' }),
      fakeTurn({ sessionId: 's-r', messageId: 'mr-2', turnIndex: 1, ts: '2026-04-20T00:00:01.000Z' }),
    ]);
    await stamp({ sessionId: 's-r' }, { workflowId: 'wf-r' });

    await buildArchive();
    const before = await getArchiveStatus();

    const result = await rebuildArchive();
    assert.equal(result.rebuiltFromZero, true);

    const after = await getArchiveStatus();
    assert.equal(after.rowCounts.sessions, before.rowCounts.sessions);
    assert.equal(after.rowCounts.turns, before.rowCounts.turns);
    assert.equal(after.rowCounts.toolCalls, before.rowCounts.toolCalls);

    const db = await openArchive();
    try {
      const row = db
        .prepare('SELECT workflow_id FROM turns WHERE message_id = ?')
        .get('mr-1') as { workflow_id: string };
      assert.equal(row.workflow_id, 'wf-r');
    } finally {
      db.close();
    }
  });

  it('handles incremental tail: appended turns after a build show up in the next build', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-tail', messageId: 'mt-1' })]);
    await buildArchive();
    const status1 = await getArchiveStatus();
    assert.equal(status1.rowCounts.turns, 1);

    await appendTurns([
      fakeTurn({
        sessionId: 's-tail',
        messageId: 'mt-2',
        turnIndex: 1,
        ts: '2026-04-20T00:00:01.000Z',
      }),
    ]);
    const result = await buildArchive();
    assert.equal(result.turnsApplied, 1);
    const status2 = await getArchiveStatus();
    assert.equal(status2.rowCounts.turns, 2);
    assert.equal(status2.upToDate, true);
  });

  it('records compaction events into the compactions table', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-c', messageId: 'mc-1' })]);
    await appendCompactions([
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-c',
        ts: '2026-04-20T00:05:00.000Z',
        precedingMessageId: 'mc-1',
        tokensBeforeCompact: 12345,
      },
    ]);
    await buildArchive();
    const db = await openArchive();
    try {
      const rows = db.prepare('SELECT * FROM compactions').all() as Array<{
        session_id: string;
        tokens_before_compact: number | bigint;
      }>;
      assert.equal(rows.length, 1);
      assert.equal(rows[0]!.session_id, 's-c');
      assert.equal(Number(rows[0]!.tokens_before_compact), 12345);
    } finally {
      db.close();
    }
  });

  it('round-trips tool_result_event lines from ledger to archive', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-tre', messageId: 'mtre-1' })]);
    const events: ToolResultEventRecord[] = [
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-tre',
        messageId: 'mtre-1',
        toolUseId: 'tu-A',
        callIndex: 0,
        eventIndex: 0,
        ts: '2026-04-20T00:00:01.000Z',
        status: 'completed',
        eventSource: 'tool_result',
        contentLength: 1234,
        contentHash: 'abc123',
        agentId: 'agent-X',
      },
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-tre',
        messageId: 'mtre-1',
        toolUseId: 'tu-B',
        callIndex: 1,
        eventIndex: 1,
        ts: '2026-04-20T00:00:02.000Z',
        status: 'errored',
        eventSource: 'tool_result',
        contentLength: 0,
        contentHash: 'def456',
        isError: true,
      },
    ];
    await appendToolResultEvents(events);
    const result = await buildArchive();
    assert.equal(result.toolResultEventsApplied, 2);

    const status = await getArchiveStatus();
    assert.equal(status.rowCounts.toolResultEvents, 2);

    const db = await openArchive();
    try {
      const rows = db
        .prepare(
          `SELECT source, session_id, message_id, tool_use_id, call_index,
                  event_index, status, content_length, content_hash, is_error,
                  agent_id, event_source, ts
             FROM tool_result_events ORDER BY event_index`,
        )
        .all() as Array<{
        source: string;
        session_id: string;
        message_id: string;
        tool_use_id: string;
        call_index: number | bigint;
        event_index: number | bigint;
        status: string;
        content_length: number | bigint | null;
        content_hash: string | null;
        is_error: number | bigint | null;
        agent_id: string | null;
        event_source: string;
        ts: string | null;
      }>;
      assert.equal(rows.length, 2);
      const r0 = rows[0]!;
      assert.equal(r0.source, 'claude-code');
      assert.equal(r0.session_id, 's-tre');
      assert.equal(r0.message_id, 'mtre-1');
      assert.equal(r0.tool_use_id, 'tu-A');
      assert.equal(Number(r0.call_index), 0);
      assert.equal(Number(r0.event_index), 0);
      assert.equal(r0.status, 'completed');
      assert.equal(Number(r0.content_length), 1234);
      assert.equal(r0.content_hash, 'abc123');
      assert.equal(r0.agent_id, 'agent-X');
      assert.equal(r0.event_source, 'tool_result');
      assert.equal(r0.ts, '2026-04-20T00:00:01.000Z');
      assert.equal(r0.is_error, null);
      const r1 = rows[1]!;
      assert.equal(r1.tool_use_id, 'tu-B');
      assert.equal(r1.status, 'errored');
      assert.equal(Number(r1.call_index), 1);
      assert.equal(Number(r1.is_error), 1);
    } finally {
      db.close();
    }
  });

  it('rebuilds tool_result_events from zero deterministically (idempotent)', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-rb-tre', messageId: 'mrb-tre-1' })]);
    const events: ToolResultEventRecord[] = Array.from({ length: 5 }, (_, i) => ({
      v: 1,
      source: 'claude-code',
      sessionId: 's-rb-tre',
      messageId: 'mrb-tre-1',
      toolUseId: `tu-${i}`,
      callIndex: i,
      eventIndex: i,
      ts: `2026-04-20T00:00:0${i}.000Z`,
      status: 'completed',
      eventSource: 'tool_result',
      contentLength: 100 * i,
      contentHash: `h${i}`,
    }));
    await appendToolResultEvents(events);
    await buildArchive();
    const before = await getArchiveStatus();
    assert.equal(before.rowCounts.toolResultEvents, 5);

    // Rebuild from zero replays the same lines and yields the same row count.
    const rb = await rebuildArchive();
    assert.equal(rb.rebuiltFromZero, true);
    assert.equal(rb.toolResultEventsApplied, 5);
    const after = await getArchiveStatus();
    assert.equal(after.rowCounts.toolResultEvents, 5);

    // A second build with no new ledger writes is a clean no-op.
    const noop = await buildArchive();
    assert.equal(noop.toolResultEventsApplied, 0);
    const noopStatus = await getArchiveStatus();
    assert.equal(noopStatus.rowCounts.toolResultEvents, 5);
  });

  it('materializes mixed-source tool_result events (claude-code, codex, opencode)', async () => {
    // Synthesize events from each supported source so the archive table
    // doesn't accidentally lock to a single source's column shape.
    const events: ToolResultEventRecord[] = [
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-cc',
        messageId: 'mcc-1',
        toolUseId: 'tu-cc',
        callIndex: 0,
        eventIndex: 0,
        ts: '2026-04-20T00:00:00.000Z',
        status: 'completed',
        eventSource: 'tool_result',
        contentLength: 10,
        contentHash: 'cc',
      },
      {
        v: 1,
        source: 'codex',
        sessionId: 's-cx',
        messageId: 'mcx-1',
        toolUseId: 'call-cx',
        callIndex: 0,
        eventIndex: 0,
        ts: '2026-04-20T00:00:01.000Z',
        status: 'completed',
        eventSource: 'function_call_output',
        contentLength: 20,
        contentHash: 'cx',
      },
      {
        v: 1,
        source: 'opencode',
        sessionId: 's-oc',
        messageId: 'moc-1',
        toolUseId: 'callid-oc',
        callIndex: 0,
        eventIndex: 0,
        ts: '2026-04-20T00:00:02.000Z',
        status: 'completed',
        eventSource: 'tool_result',
        contentLength: 30,
        contentHash: 'oc',
      },
    ];
    await appendToolResultEvents(events);
    await buildArchive();
    const db = await openArchive();
    try {
      const rows = db
        .prepare(
          'SELECT DISTINCT source FROM tool_result_events ORDER BY source',
        )
        .all() as Array<{ source: string }>;
      assert.deepEqual(
        rows.map((r) => r.source),
        ['claude-code', 'codex', 'opencode'],
      );
      const total = (db
        .prepare('SELECT COUNT(*) AS n FROM tool_result_events')
        .get() as { n: number | bigint }).n;
      assert.equal(Number(total), 3);
    } finally {
      db.close();
    }
  });

  it('tool_result_events: incremental tail picks up newly-appended events', async () => {
    await appendToolResultEvents([
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-tail-tre',
        messageId: 'mtt-1',
        toolUseId: 'tu-1',
        callIndex: 0,
        eventIndex: 0,
        ts: '2026-04-20T00:00:00.000Z',
        status: 'completed',
        eventSource: 'tool_result',
        contentLength: 1,
        contentHash: 'h1',
      },
    ]);
    await buildArchive();
    const status1 = await getArchiveStatus();
    assert.equal(status1.rowCounts.toolResultEvents, 1);

    await appendToolResultEvents([
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-tail-tre',
        messageId: 'mtt-1',
        toolUseId: 'tu-2',
        callIndex: 1,
        eventIndex: 1,
        ts: '2026-04-20T00:00:01.000Z',
        status: 'completed',
        eventSource: 'tool_result',
        contentLength: 2,
        contentHash: 'h2',
      },
    ]);
    const tail = await buildArchive();
    assert.equal(tail.toolResultEventsApplied, 1);
    const status2 = await getArchiveStatus();
    assert.equal(status2.rowCounts.toolResultEvents, 2);
    assert.equal(status2.upToDate, true);
  });

  it('tool_result_events: PK is dedup-safe — replaying the same event is a no-op upsert', async () => {
    const ev: ToolResultEventRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-dup',
      messageId: 'mdup-1',
      toolUseId: 'tu-dup',
      callIndex: 0,
      eventIndex: 0,
      ts: '2026-04-20T00:00:00.000Z',
      status: 'completed',
      eventSource: 'tool_result',
      contentLength: 99,
      contentHash: 'h-dup',
    };
    await appendToolResultEvents([ev]);
    await buildArchive();

    // rebuildArchive replays the entire ledger — including the same tool
    // result event line — and must land exactly one row.
    await rebuildArchive();
    const status = await getArchiveStatus();
    assert.equal(status.rowCounts.toolResultEvents, 1);
  });

  it('archive lives at RELAYBURN_HOME/archive.sqlite', async () => {
    await buildArchive();
    const expected = path.join(tmpDir, 'archive.sqlite');
    assert.equal(archivePath(), expected);
    const st = await stat(expected);
    assert.ok(st.isFile());
  });

  it('handles ledger truncation: shrunken ledger triggers rebuild from zero', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-t', messageId: 'mt-A' }),
      fakeTurn({ sessionId: 's-t', messageId: 'mt-B', turnIndex: 1, ts: '2026-04-20T00:00:01.000Z' }),
    ]);
    await buildArchive();

    // Simulate `burn rebuild --reclassify` rewriting the ledger to a smaller
    // size: nuke the file and the dedup index, then write a single fresh
    // turn. Build should detect the cursor is past EOF and rebuild from
    // byte zero.
    await rm(ledgerPath());
    const { unlink } = await import('node:fs/promises');
    await unlink(path.join(tmpDir, 'ledger.idx')).catch(() => undefined);
    await unlink(path.join(tmpDir, 'ledger.content.idx')).catch(() => undefined);
    __resetIndexCacheForTesting();
    await appendTurns([
      fakeTurn({
        sessionId: 's-t',
        messageId: 'mt-C',
        ts: '2026-04-21T00:00:00.000Z',
      }),
    ]);
    await buildArchive();
    const status = await getArchiveStatus();
    assert.equal(status.rowCounts.turns, 1);
    const db = await openArchive();
    try {
      const row = db
        .prepare('SELECT message_id FROM turns WHERE session_id = ?')
        .get('s-t') as { message_id: string };
      assert.equal(row.message_id, 'mt-C');
    } finally {
      db.close();
    }
  });

  it('rebuildArchive populates both last_built_at and last_rebuild_at', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-rb', messageId: 'mrb-1' })]);
    const before = await getArchiveStatus();
    assert.equal(before.lastRebuildAt, null);

    await rebuildArchive();
    const after = await getArchiveStatus();
    assert.ok(after.lastBuiltAt, 'lastBuiltAt should be populated after rebuild');
    assert.ok(after.lastRebuildAt, 'lastRebuildAt should be populated after rebuild');
  });

  it('buildArchive only updates lastBuiltAt, not lastRebuildAt', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-bo', messageId: 'mbo-1' })]);
    await buildArchive();
    const status = await getArchiveStatus();
    assert.ok(status.lastBuiltAt, 'lastBuiltAt should be populated after build');
    assert.equal(status.lastRebuildAt, null, 'lastRebuildAt should remain null after a non-rebuild build');
  });

  it('partial trailing line: ledger cursor advances only past complete lines', async () => {
    // Write a complete turn first.
    await appendTurns([fakeTurn({ sessionId: 's-p', messageId: 'mp-1' })]);
    await buildArchive();
    const after1 = await getArchiveStatus();

    // Append a partial line (no trailing newline) directly to the ledger,
    // simulating a writer that crashed mid-write.
    const { appendFile } = await import('node:fs/promises');
    const partial = '{"v":1,"kind":"turn"';
    await appendFile(ledgerPath(), partial);

    // Build should advance the cursor only past the previous newline; the
    // partial fragment must remain unconsumed so the next build can re-read
    // it once the writer has completed it.
    await buildArchive();
    const after2 = await getArchiveStatus();
    assert.equal(
      after2.ledgerOffsetBytes,
      after1.ledgerOffsetBytes,
      'cursor must not advance past a partial trailing line',
    );
    const ledgerStat = await stat(ledgerPath());
    assert.ok(
      after2.ledgerOffsetBytes < ledgerStat.size,
      'ledger size includes the partial fragment but cursor must stay short of EOF',
    );
  });

  it('persists per-turn attribution_fidelity / tokens_present / cost_present columns and rolls them up onto sessions (#110)', async () => {
    // Mixed-fidelity ledger: a Claude `full` turn (carries fidelity), a
    // synthetic Codex turn that lacks fidelity entirely (older line), and a
    // cost-only turn to exercise the cost_present column. The session's
    // worst-fidelity rollup should be `cost-only` (NULL ignored, cost-only is
    // worst); has_full_attribution should be 0 (some known-fidelity turn is
    // not full).
    await appendTurns([
      fakeTurn({
        sessionId: 's-mix',
        messageId: 'mix-full',
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
        sessionId: 's-mix',
        messageId: 'mix-codex-old',
        turnIndex: 1,
        ts: '2026-04-20T00:00:01.000Z',
        // No fidelity field — simulates a Codex/OpenCode line written before
        // the upstream fidelity work landed (#84/#89).
      }),
      fakeTurn({
        sessionId: 's-mix',
        messageId: 'mix-cost',
        turnIndex: 2,
        ts: '2026-04-20T00:00:02.000Z',
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
    ]);
    await buildArchive();

    const db = await openArchive();
    try {
      const rows = db
        .prepare(
          `SELECT message_id, attribution_fidelity, tokens_present, cost_present
           FROM turns WHERE session_id = ? ORDER BY turn_index`,
        )
        .all('s-mix') as Array<{
        message_id: string;
        attribution_fidelity: string | null;
        tokens_present: number | bigint | null;
        cost_present: number | bigint | null;
      }>;
      assert.equal(rows.length, 3);

      // Claude full turn — fidelity columns populated.
      assert.equal(rows[0]!.message_id, 'mix-full');
      assert.equal(rows[0]!.attribution_fidelity, 'full');
      assert.equal(Number(rows[0]!.tokens_present), 1);
      assert.equal(Number(rows[0]!.cost_present), 0);

      // Older Codex turn — no fidelity → all three columns NULL (unknown,
      // not zero / partial).
      assert.equal(rows[1]!.message_id, 'mix-codex-old');
      assert.equal(rows[1]!.attribution_fidelity, null);
      assert.equal(rows[1]!.tokens_present, null);
      assert.equal(rows[1]!.cost_present, null);

      // Cost-only turn.
      assert.equal(rows[2]!.message_id, 'mix-cost');
      assert.equal(rows[2]!.attribution_fidelity, 'cost-only');
      assert.equal(Number(rows[2]!.tokens_present), 0);
      assert.equal(Number(rows[2]!.cost_present), 1);

      // Session rollup: worst fidelity ignoring NULLs is 'cost-only';
      // has_full_attribution = 0 because at least one known-fidelity turn
      // (mix-cost) is not 'full'.
      const sessionRow = db
        .prepare(
          'SELECT min_fidelity, has_full_attribution FROM sessions WHERE session_id = ?',
        )
        .get('s-mix') as {
        min_fidelity: string | null;
        has_full_attribution: number | bigint | null;
      };
      assert.equal(sessionRow.min_fidelity, 'cost-only');
      assert.equal(Number(sessionRow.has_full_attribution), 0);
    } finally {
      db.close();
    }

    // Fidelity histogram on status: 1 full, 1 cost-only, 1 unknown.
    const status = await getArchiveStatus();
    assert.deepEqual(status.fidelityHistogram, {
      full: 1,
      'cost-only': 1,
      unknown: 1,
    });
  });

  it('session has_full_attribution is 1 only when every fidelity-tagged turn is full, NULL when none carry fidelity (#110)', async () => {
    const fullCoverage = {
      hasInputTokens: true,
      hasOutputTokens: true,
      hasReasoningTokens: false,
      hasCacheReadTokens: true,
      hasCacheCreateTokens: true,
      hasToolCalls: true,
      hasToolResultEvents: true,
      hasSessionRelationships: true,
      hasRawContent: true,
    } as const;

    await appendTurns([
      // s-allfull: two full turns.
      fakeTurn({
        sessionId: 's-allfull',
        messageId: 'af-1',
        fidelity: { granularity: 'per-turn', coverage: { ...fullCoverage }, class: 'full' },
      }),
      fakeTurn({
        sessionId: 's-allfull',
        messageId: 'af-2',
        turnIndex: 1,
        ts: '2026-04-20T00:00:01.000Z',
        fidelity: { granularity: 'per-turn', coverage: { ...fullCoverage }, class: 'full' },
      }),
      // s-nofid: no fidelity on any turn.
      fakeTurn({ sessionId: 's-nofid', messageId: 'nf-1' }),
      fakeTurn({
        sessionId: 's-nofid',
        messageId: 'nf-2',
        turnIndex: 1,
        ts: '2026-04-20T00:00:01.000Z',
      }),
    ]);
    await buildArchive();

    const db = await openArchive();
    try {
      const allfull = db
        .prepare(
          'SELECT min_fidelity, has_full_attribution FROM sessions WHERE session_id = ?',
        )
        .get('s-allfull') as {
        min_fidelity: string | null;
        has_full_attribution: number | bigint | null;
      };
      assert.equal(allfull.min_fidelity, 'full');
      assert.equal(Number(allfull.has_full_attribution), 1);

      const nofid = db
        .prepare(
          'SELECT min_fidelity, has_full_attribution FROM sessions WHERE session_id = ?',
        )
        .get('s-nofid') as {
        min_fidelity: string | null;
        has_full_attribution: number | bigint | null;
      };
      // No turn carries fidelity → both rollups NULL (unknown, not 0/full).
      assert.equal(nofid.min_fidelity, null);
      assert.equal(nofid.has_full_attribution, null);
    } finally {
      db.close();
    }
  });

  it('rebuilding a fidelity-tagged ledger is idempotent: row counts and per-turn columns stable across rebuild (#110)', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 's-idem',
        messageId: 'id-1',
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
        sessionId: 's-idem',
        messageId: 'id-2',
        turnIndex: 1,
        ts: '2026-04-20T00:00:01.000Z',
        fidelity: {
          granularity: 'per-turn',
          coverage: {
            hasInputTokens: true,
            hasOutputTokens: true,
            hasReasoningTokens: false,
            hasCacheReadTokens: false,
            hasCacheCreateTokens: false,
            hasToolCalls: false,
            hasToolResultEvents: false,
            hasSessionRelationships: false,
            hasRawContent: false,
          },
          class: 'usage-only',
        },
      }),
    ]);
    await buildArchive();
    const before = await getArchiveStatus();

    await rebuildArchive();
    const after = await getArchiveStatus();

    assert.deepEqual(after.rowCounts, before.rowCounts);
    assert.deepEqual(after.fidelityHistogram, before.fidelityHistogram);
    assert.deepEqual(after.fidelityHistogram, { full: 1, 'usage-only': 1 });

    const db = await openArchive();
    try {
      const session = db
        .prepare(
          'SELECT min_fidelity, has_full_attribution FROM sessions WHERE session_id = ?',
        )
        .get('s-idem') as {
        min_fidelity: string | null;
        has_full_attribution: number | bigint | null;
      };
      // Worst class is `usage-only` (rank 4 < full=5); not all full → 0.
      assert.equal(session.min_fidelity, 'usage-only');
      assert.equal(Number(session.has_full_attribution), 0);
    } finally {
      db.close();
    }
  });

  it('additive ALTER migration: opening an archive that pre-dates the fidelity columns adds them without dropping data (#110)', async () => {
    // Build a normal archive at the current schema, then drop the new
    // columns to simulate an archive built before #110 landed. Re-open via
    // openArchive() and assert (a) data survives, (b) the columns exist
    // again, and (c) a subsequent build populates them for new turns.
    await appendTurns([
      fakeTurn({ sessionId: 's-mig', messageId: 'mg-old' }),
    ]);
    await buildArchive();

    // Drop the columns by recreating the tables without them. SQLite has no
    // DROP COLUMN on older versions, so we round-trip through a temp table
    // and recreate to mimic an older on-disk shape.
    {
      const db = new DatabaseSync(archivePath());
      try {
        db.exec('BEGIN');
        db.exec(`
          CREATE TABLE turns_old (
            source                TEXT NOT NULL,
            session_id            TEXT NOT NULL,
            message_id            TEXT NOT NULL,
            turn_index            INTEGER NOT NULL,
            ts                    TEXT NOT NULL,
            model                 TEXT NOT NULL,
            project               TEXT,
            project_key           TEXT,
            activity              TEXT,
            stop_reason           TEXT,
            has_edits             INTEGER,
            retries               INTEGER,
            is_sidechain          INTEGER,
            subagent_id           TEXT,
            parent_subagent_id    TEXT,
            parent_tool_use_id    TEXT,
            subagent_type         TEXT,
            input_tokens          INTEGER NOT NULL DEFAULT 0,
            output_tokens         INTEGER NOT NULL DEFAULT 0,
            reasoning_tokens      INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
            cache_create_5m_tokens INTEGER NOT NULL DEFAULT 0,
            cache_create_1h_tokens INTEGER NOT NULL DEFAULT 0,
            workflow_id           TEXT,
            agent_id              TEXT,
            persona               TEXT,
            tier                  TEXT,
            enrichment_json       TEXT,
            PRIMARY KEY (source, session_id, message_id)
          );
          INSERT INTO turns_old SELECT
            source, session_id, message_id, turn_index, ts, model, project, project_key,
            activity, stop_reason, has_edits, retries,
            is_sidechain, subagent_id, parent_subagent_id, parent_tool_use_id, subagent_type,
            input_tokens, output_tokens, reasoning_tokens,
            cache_read_tokens, cache_create_5m_tokens, cache_create_1h_tokens,
            workflow_id, agent_id, persona, tier, enrichment_json
          FROM turns;
          DROP TABLE turns;
          ALTER TABLE turns_old RENAME TO turns;

          CREATE TABLE sessions_old (
            source              TEXT NOT NULL,
            session_id          TEXT NOT NULL,
            project             TEXT,
            project_key         TEXT,
            started_at          TEXT,
            ended_at            TEXT,
            turn_count          INTEGER NOT NULL DEFAULT 0,
            model_set_json      TEXT,
            workflow_id         TEXT,
            agent_id            TEXT,
            parent_agent_id     TEXT,
            has_subagent        INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (source, session_id)
          );
          INSERT INTO sessions_old SELECT
            source, session_id, project, project_key, started_at, ended_at,
            turn_count, model_set_json, workflow_id, agent_id, parent_agent_id, has_subagent
          FROM sessions;
          DROP TABLE sessions;
          ALTER TABLE sessions_old RENAME TO sessions;
        `);
        db.exec('COMMIT');
      } finally {
        db.close();
      }
    }

    // Re-opening must idempotently add the columns back without nuking
    // existing rows. Existing rows have NULL in the new columns (unknown).
    const db = await openArchive();
    try {
      const turnCols = (db.prepare('PRAGMA table_info(turns)').all() as Array<{
        name: string;
      }>).map((r) => r.name);
      assert.ok(turnCols.includes('attribution_fidelity'));
      assert.ok(turnCols.includes('tokens_present'));
      assert.ok(turnCols.includes('cost_present'));

      const sessionCols = (db.prepare('PRAGMA table_info(sessions)').all() as Array<{
        name: string;
      }>).map((r) => r.name);
      assert.ok(sessionCols.includes('min_fidelity'));
      assert.ok(sessionCols.includes('has_full_attribution'));

      const turnRow = db
        .prepare(
          'SELECT message_id, attribution_fidelity FROM turns WHERE message_id = ?',
        )
        .get('mg-old') as { message_id: string; attribution_fidelity: string | null };
      assert.equal(turnRow.message_id, 'mg-old');
      assert.equal(turnRow.attribution_fidelity, null);
    } finally {
      db.close();
    }

    // A fresh build then populates the new columns for new turns without a
    // full rebuild — this is the "ALTER strategy" promise from the issue.
    await appendTurns([
      fakeTurn({
        sessionId: 's-mig',
        messageId: 'mg-new',
        turnIndex: 1,
        ts: '2026-04-20T00:00:01.000Z',
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
    ]);
    await buildArchive();

    const status = await getArchiveStatus();
    assert.equal(status.rowCounts.turns, 2);
    assert.equal(status.fidelityHistogram['full'], 1);
    assert.equal(status.fidelityHistogram['unknown'], 1);
  });

  it('schema version bump: an old archive_version triggers a clean rebuild on open', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-v', messageId: 'mv-1' })]);
    await buildArchive();

    // Tamper with archive_version to simulate an older schema.
    {
      const db = new DatabaseSync(archivePath());
      try {
        db.prepare('UPDATE archive_state SET archive_version = ? WHERE id = 1').run(0);
      } finally {
        db.close();
      }
    }

    // Re-opening must wipe the file and reset the cursor.
    const db = await openArchive();
    try {
      const state = db
        .prepare('SELECT archive_version, ledger_offset_bytes FROM archive_state WHERE id = 1')
        .get() as { archive_version: number | bigint; ledger_offset_bytes: number | bigint };
      assert.equal(Number(state.archive_version), ARCHIVE_VERSION);
      assert.equal(Number(state.ledger_offset_bytes), 0);
      const turnRows = db.prepare('SELECT COUNT(*) AS n FROM turns').get() as { n: number | bigint };
      assert.equal(Number(turnRows.n), 0);
    } finally {
      db.close();
    }

    // Build re-materializes everything.
    await buildArchive();
    const status = await getArchiveStatus();
    assert.equal(status.rowCounts.turns, 1);
  });

  it('vacuum on a missing archive is a no-op and reports existed=false', async () => {
    const result = await vacuumArchive();
    assert.equal(result.existed, false);
    assert.equal(result.beforeBytes, 0);
    assert.equal(result.afterBytes, 0);
    assert.equal(result.reclaimedBytes, 0);
    // Crucially: vacuum must NOT create the archive file as a side effect.
    const status = await getArchiveStatus();
    assert.equal(status.exists, false);
  });

  it('vacuum reduces file size after rebuild churn', async () => {
    // Build up enough rows that VACUUM has something measurable to reclaim.
    // Then rebuild — which deletes + rewrites every row, leaving a large pile
    // of free pages — and vacuum to confirm the file shrinks.
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 200; i++) {
      turns.push(
        fakeTurn({
          sessionId: `s-vac-${i % 5}`,
          messageId: `mv-${i}`,
          turnIndex: i,
          ts: new Date(Date.UTC(2026, 3, 20, 0, 0, i)).toISOString(),
          // Vary usage so the writer's content-fingerprint dedup doesn't
          // collapse them.
          usage: {
            input: 100 + i,
            output: 50 + i,
            reasoning: 0,
            cacheRead: 1000 + i,
            cacheCreate5m: 0,
            cacheCreate1h: 0,
          },
        }),
      );
    }
    await appendTurns(turns);
    await buildArchive();
    // Force churn: rebuild from zero deletes all rows then re-inserts them.
    await rebuildArchive();
    await rebuildArchive();

    const sizeBefore = (await stat(archivePath())).size;
    const result = await vacuumArchive();
    assert.equal(result.existed, true);
    assert.equal(result.beforeBytes, sizeBefore);
    // VACUUM + WAL-truncate should not grow the file.
    assert.ok(
      result.afterBytes <= result.beforeBytes,
      `expected afterBytes <= beforeBytes, got ${result.afterBytes} > ${result.beforeBytes}`,
    );
    assert.equal(result.reclaimedBytes, result.beforeBytes - result.afterBytes);

    // Sanity: row counts survive vacuum unchanged.
    const status = await getArchiveStatus();
    assert.equal(status.rowCounts.turns, turns.length);
  });

  it('vacuum serializes against a concurrent build via the archive lock', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-lk', messageId: 'lk-1' })]);
    await buildArchive();

    // Hold the archive lock externally while invoking vacuum + build. They
    // must both queue behind the holder, then run sequentially without
    // corrupting the archive.
    let releaseHolder: () => void = () => {};
    const holderReleased = new Promise<void>((r) => {
      releaseHolder = r;
    });
    const holder = withLock('archive', async () => {
      await holderReleased;
    });

    // Brief delay so the holder grabs the lock before we issue the contenders.
    await new Promise((r) => setTimeout(r, 20));
    const vacuumPromise = vacuumArchive();
    const buildPromise = buildArchive();
    // Neither should resolve while the holder still has the lock.
    let resolved = 0;
    void vacuumPromise.then(() => resolved++);
    void buildPromise.then(() => resolved++);
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(resolved, 0, 'vacuum/build resolved before holder released the lock');

    releaseHolder();
    await holder;
    const [vac, _build] = await Promise.all([vacuumPromise, buildPromise]);
    assert.equal(vac.existed, true);

    // Archive remains queryable and consistent.
    const status = await getArchiveStatus();
    assert.equal(status.rowCounts.turns, 1);
  });
});
