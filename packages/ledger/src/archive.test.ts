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
} from './archive.js';

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
                  event_index, status, content_length, content_hash, agent_id,
                  event_source, ts
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
      const r1 = rows[1]!;
      assert.equal(r1.tool_use_id, 'tu-B');
      assert.equal(r1.status, 'errored');
      assert.equal(Number(r1.call_index), 1);
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
});
