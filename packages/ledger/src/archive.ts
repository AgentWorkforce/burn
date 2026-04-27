import { createReadStream } from 'node:fs';
import { mkdir, stat, unlink } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import * as path from 'node:path';
import { DatabaseSync } from 'node:sqlite';

// `node:sqlite` is still flagged ExperimentalWarning in Node 22 even though it
// works without `--experimental-sqlite`. Suppress only that specific warning
// so we don't pollute every CLI invocation that touches the archive.
//
// Node's default warning printer fires from the same `process.emit('warning',
// …)` path that user listeners do, so the only reliable way to drop a
// specific warning is to short-circuit the emit. We patch only this one
// warning by name+message so any other warning still propagates normally.
let warningFilterInstalled = false;
function installSqliteWarningFilter(): void {
  if (warningFilterInstalled) return;
  warningFilterInstalled = true;
  type Emitter = (event: string | symbol, ...rest: unknown[]) => boolean;
  const proc = process as unknown as { emit: Emitter };
  const originalEmit = proc.emit.bind(process);
  proc.emit = function patched(event: string | symbol, ...rest: unknown[]): boolean {
    if (event === 'warning') {
      const payload = rest[0];
      if (
        payload &&
        typeof payload === 'object' &&
        (payload as Error).name === 'ExperimentalWarning' &&
        /SQLite/i.test((payload as Error).message ?? '')
      ) {
        return false;
      }
    }
    return originalEmit(event, ...rest);
  };
}
installSqliteWarningFilter();

import type { TurnRecord } from '@relayburn/reader';

import { withLock } from './lock.js';
import { archivePath, ledgerPath } from './paths.js';
import {
  isCompactionLine,
  isStampLine,
  isToolResultEventLine,
  isTurnLine,
  stampMatches,
  type CompactionLine,
  type Enrichment,
  type StampLine,
  type ToolResultEventLine,
  type TurnLine,
} from './schema.js';

/**
 * On-disk schema version for `archive.sqlite`. Bump when any CREATE TABLE
 * statement below changes shape; the next `buildArchive()` call will detect
 * the mismatch in `archive_state.archive_version` and rebuild from scratch.
 */
export const ARCHIVE_VERSION = 2;

/**
 * SQL statements that materialize the read model. These are intentionally
 * kept declarative and idempotent (`CREATE TABLE IF NOT EXISTS`) so callers
 * can run them on every open without coordinating migrations.
 *
 * Schema reference: see issue #40 for the design discussion.
 *  - sessions: one row per (source, sessionId)
 *  - turns: one row per ingested TurnRecord, with stamps folded in
 *  - tool_calls: one row per ToolCall attached to a turn
 *  - tool_result_events: chronological tool-output / terminal-status events
 *    materialized from `ToolResultEventLine` ledger lines (#101 / #42 / #77).
 *    `content_length` / `content_hash` come straight from the canonical
 *    record; the content sidecar (#33) carries the raw bytes when callers
 *    need them.
 *  - archive_state: incremental build cursor + schema version
 */
const SCHEMA_SQL = `
CREATE TABLE IF NOT EXISTS sessions (
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
  -- Worst (lowest) fidelity class observed across the session's turns.
  -- NULL iff every turn in the session lacks fidelity metadata (older lines
  -- emitted before the upstream parser populated TurnRecord.fidelity).
  -- Vocabulary matches FidelityClass: full | usage-only | partial |
  -- aggregate-only | cost-only.
  min_fidelity        TEXT,
  -- 1 iff every known-fidelity turn in the session is class='full'. 0 iff
  -- any turn is below 'full'. NULL iff the session has zero turns with
  -- fidelity metadata to assert against. See issue #110 / #40.
  has_full_attribution INTEGER,
  PRIMARY KEY (source, session_id)
);

CREATE INDEX IF NOT EXISTS idx_sessions_started_at ON sessions(started_at);
CREATE INDEX IF NOT EXISTS idx_sessions_project_key ON sessions(project_key);
CREATE INDEX IF NOT EXISTS idx_sessions_workflow_id ON sessions(workflow_id);

CREATE TABLE IF NOT EXISTS turns (
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
  -- Free-form subagent description (Claude reader populates this from the
  -- spawn payload). Persisted so 'burn summary --subagent-tree --json'
  -- preserves SubagentTreeNode.description on archive-backed reads.
  subagent_description  TEXT,
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
  -- Coverage / fidelity columns (issue #110, upstream #41 / PR #76). Mirror
  -- TurnRecord.fidelity so SQL consumers can filter / group by them without
  -- re-deriving in memory. NULL on rows from older ledger lines that pre-date
  -- the upstream fidelity work — interpret NULL as "unknown" rather than
  -- guessing.
  --
  -- attribution_fidelity: FidelityClass string —
  --   full | usage-only | partial | aggregate-only | cost-only
  attribution_fidelity  TEXT,
  -- 1 iff the source surfaced any per-turn token count
  -- (input OR output OR reasoning); 0 iff explicitly known to surface none;
  -- NULL iff fidelity metadata is missing entirely.
  tokens_present        INTEGER,
  -- 1 iff this row is cost-only (granularity='cost-only'); 0 otherwise when
  -- fidelity is present; NULL iff fidelity metadata is missing.
  cost_present          INTEGER,
  PRIMARY KEY (source, session_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_turns_ts ON turns(ts);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(source, session_id, turn_index);
CREATE INDEX IF NOT EXISTS idx_turns_model ON turns(model);
CREATE INDEX IF NOT EXISTS idx_turns_activity ON turns(activity);
CREATE INDEX IF NOT EXISTS idx_turns_project_key ON turns(project_key);
CREATE INDEX IF NOT EXISTS idx_turns_workflow ON turns(workflow_id);
-- idx_turns_attribution_fidelity is created in applyAdditiveMigrations()
-- (which runs after the column is ensured) — see issue #110.

CREATE TABLE IF NOT EXISTS tool_calls (
  source         TEXT NOT NULL,
  session_id     TEXT NOT NULL,
  message_id     TEXT NOT NULL,
  call_index     INTEGER NOT NULL,
  tool_use_id    TEXT,
  tool_name      TEXT NOT NULL,
  target         TEXT,
  args_hash      TEXT,
  is_error       INTEGER,
  PRIMARY KEY (source, session_id, message_id, call_index)
);

CREATE INDEX IF NOT EXISTS idx_tool_calls_name ON tool_calls(tool_name);
CREATE INDEX IF NOT EXISTS idx_tool_calls_use_id ON tool_calls(tool_use_id);

-- Materialized from ToolResultEventLine (execution-graph #42 / #77) -- one
-- row per chronological tool_result / terminal-status / progress event keyed
-- on (source, session_id, message_id, tool_use_id, event_index). The content
-- sidecar (#33) holds the raw bytes; this table carries metadata only
-- (content_length, content_hash) so analyses can group / dedupe without
-- loading sidecar JSONL.
CREATE TABLE IF NOT EXISTS tool_result_events (
  source              TEXT NOT NULL,
  session_id          TEXT NOT NULL,
  message_id          TEXT NOT NULL,
  tool_use_id         TEXT NOT NULL,
  call_index          INTEGER NOT NULL,
  event_index         INTEGER NOT NULL,
  status              TEXT,
  content_length      INTEGER,
  content_hash        TEXT,
  is_error            INTEGER,
  subagent_session_id TEXT,
  agent_id            TEXT,
  event_source        TEXT,
  ts                  TEXT,
  PRIMARY KEY (source, session_id, message_id, tool_use_id, event_index)
);

CREATE INDEX IF NOT EXISTS idx_tool_result_events_use_id ON tool_result_events(tool_use_id);
CREATE INDEX IF NOT EXISTS idx_tool_result_events_session ON tool_result_events(source, session_id);
CREATE INDEX IF NOT EXISTS idx_tool_result_events_subagent ON tool_result_events(subagent_session_id);

CREATE TABLE IF NOT EXISTS compactions (
  source                TEXT NOT NULL,
  session_id            TEXT NOT NULL,
  ts                    TEXT NOT NULL,
  preceding_message_id  TEXT,
  tokens_before_compact INTEGER,
  PRIMARY KEY (source, session_id, ts)
);

CREATE TABLE IF NOT EXISTS archive_state (
  id                    INTEGER PRIMARY KEY CHECK (id = 1),
  ledger_offset_bytes   INTEGER NOT NULL DEFAULT 0,
  ledger_mtime_ms       INTEGER NOT NULL DEFAULT 0,
  archive_version       INTEGER NOT NULL,
  last_built_at         TEXT,
  last_rebuild_at       TEXT
);
`;

export interface ArchiveStatus {
  archivePath: string;
  exists: boolean;
  archiveVersion: number;
  ledgerOffsetBytes: number;
  ledgerMtimeMs: number;
  ledgerSizeBytes: number;
  ledgerMtimeMsCurrent: number;
  upToDate: boolean;
  lastBuiltAt: string | null;
  lastRebuildAt: string | null;
  rowCounts: {
    sessions: number;
    turns: number;
    toolCalls: number;
    toolResultEvents: number;
    compactions: number;
  };
  /**
   * Histogram of `turns.attribution_fidelity` values. Keys are the
   * `FidelityClass` strings actually present plus `unknown` (NULL — turns
   * emitted before upstream fidelity work landed). Absent keys are zero.
   */
  fidelityHistogram: Record<string, number>;
}

export interface BuildResult {
  /** Bytes of the ledger that were newly scanned in this call. */
  scannedBytes: number;
  /** Distinct turns appended or updated. */
  turnsApplied: number;
  /** Distinct sessions touched. */
  sessionsTouched: number;
  /** Stamp lines applied. Stamps cause a re-fold of every turn in the */
  /* affected session (cheap because turns is indexed by session). */
  stampsApplied: number;
  /** Compaction events ingested. */
  compactionsApplied: number;
  /** Tool-result-event lines materialized into `tool_result_events`. */
  toolResultEventsApplied: number;
  /** True iff the archive was rebuilt from zero before this build. */
  rebuiltFromZero: boolean;
}

/**
 * Open (and create-if-missing) the archive database. Callers must `close()`
 * the returned handle. This function is also responsible for applying schema
 * migrations: if the on-disk `archive_version` is older than `ARCHIVE_VERSION`
 * we delete and recreate, since the archive is by design rebuildable.
 */
export async function openArchive(): Promise<DatabaseSync> {
  const dbPath = archivePath();
  await mkdir(path.dirname(dbPath), { recursive: true });

  let db = new DatabaseSync(dbPath);
  // Pragmas: WAL gives us safe-ish concurrency with readers and a measurable
  // write speedup; foreign_keys we don't currently rely on but turning it on
  // costs nothing and guards against future cross-table schema drift.
  db.exec('PRAGMA journal_mode = WAL');
  db.exec('PRAGMA foreign_keys = ON');
  db.exec(SCHEMA_SQL);
  applyAdditiveMigrations(db);

  const versionRow = db
    .prepare('SELECT archive_version FROM archive_state WHERE id = 1')
    .get() as { archive_version?: number | bigint } | undefined;
  const onDiskVersion =
    versionRow && versionRow.archive_version !== undefined ? Number(versionRow.archive_version) : null;
  if (onDiskVersion === null) {
    db.prepare(
      `INSERT INTO archive_state (id, ledger_offset_bytes, ledger_mtime_ms, archive_version)
       VALUES (1, 0, 0, ?)`,
    ).run(ARCHIVE_VERSION);
  } else if (onDiskVersion !== ARCHIVE_VERSION) {
    // Schema mismatch: drop the file and rebuild from scratch on next build.
    // Safe because the archive is derived state.
    db.close();
    await unlink(dbPath).catch(() => undefined);
    db = new DatabaseSync(dbPath);
    db.exec('PRAGMA journal_mode = WAL');
    db.exec('PRAGMA foreign_keys = ON');
    db.exec(SCHEMA_SQL);
    applyAdditiveMigrations(db);
    db.prepare(
      `INSERT INTO archive_state (id, ledger_offset_bytes, ledger_mtime_ms, archive_version)
       VALUES (1, 0, 0, ?)`,
    ).run(ARCHIVE_VERSION);
  }
  return db;
}

/**
 * Forward-migrate an existing archive that pre-dates a column added under the
 * same `ARCHIVE_VERSION`. The schema constant uses `CREATE TABLE IF NOT
 * EXISTS`, which is a no-op on an existing table — so a column added there
 * later won't appear on already-built archives. We add it explicitly here.
 *
 * Strategy: idempotent `ALTER TABLE … ADD COLUMN` guarded by `PRAGMA
 * table_info` so re-opens are cheap and don't double-add. Only used for
 * additive, NULL-defaulted columns; anything else needs a `ARCHIVE_VERSION`
 * bump and a clean rebuild.
 *
 * Added in #110: fidelity columns on `turns` and `sessions`.
 */
function applyAdditiveMigrations(db: DatabaseSync): void {
  ensureColumn(db, 'turns', 'attribution_fidelity', 'TEXT');
  ensureColumn(db, 'turns', 'tokens_present', 'INTEGER');
  ensureColumn(db, 'turns', 'cost_present', 'INTEGER');
  ensureColumn(db, 'turns', 'subagent_description', 'TEXT');
  ensureColumn(db, 'sessions', 'min_fidelity', 'TEXT');
  ensureColumn(db, 'sessions', 'has_full_attribution', 'INTEGER');
  // Index on the fidelity column — `CREATE INDEX IF NOT EXISTS` is already
  // idempotent so we can just rerun the SCHEMA_SQL one. But because the column
  // may have just been added on this open, we need to (re)create the index
  // here to cover that path explicitly.
  db.exec(
    'CREATE INDEX IF NOT EXISTS idx_turns_attribution_fidelity ON turns(attribution_fidelity)',
  );
}

function ensureColumn(
  db: DatabaseSync,
  table: string,
  column: string,
  decl: string,
): void {
  const cols = db.prepare(`PRAGMA table_info(${table})`).all() as Array<{ name: string }>;
  if (cols.some((c) => c.name === column)) return;
  db.exec(`ALTER TABLE ${table} ADD COLUMN ${column} ${decl}`);
}

async function fileExists(p: string): Promise<boolean> {
  try {
    const s = await stat(p);
    return s.isFile();
  } catch {
    return false;
  }
}

/**
 * Drop the archive entirely and rebuild from the ledger.
 *
 * Equivalent to `unlink(archive.sqlite)` followed by `buildArchive()`, but
 * exposed as a single call so the CLI can lock once and so callers can rely
 * on a single atomic semantics (either we have a fresh archive or the call
 * threw).
 */
export async function rebuildArchive(): Promise<BuildResult> {
  return withLock('archive', async () => {
    const dbPath = archivePath();
    await unlink(dbPath).catch(() => undefined);
    // WAL/journal sidecar files are recreated on next open; nuke them too so
    // we don't accidentally read stale committed-but-not-checkpointed data.
    await unlink(`${dbPath}-wal`).catch(() => undefined);
    await unlink(`${dbPath}-shm`).catch(() => undefined);
    const result = await buildArchiveLocked({ isRebuild: true });
    return { ...result, rebuiltFromZero: true };
  });
}

/**
 * Apply any ledger tail not yet materialized into the archive.
 *
 * Idempotent: calling repeatedly with no new ledger writes is a no-op. Safe
 * to interleave with `appendTurns` / `stamp` because both honor the same
 * `'ledger'` lock — we hold the dedicated `'archive'` lock here so two
 * concurrent `archive build` calls can't race, but we don't block the write
 * path.
 */
export async function buildArchive(): Promise<BuildResult> {
  return withLock('archive', () => buildArchiveLocked());
}

async function buildArchiveLocked(opts: { isRebuild?: boolean } = {}): Promise<BuildResult> {
  const ledger = ledgerPath();
  const db = await openArchive();
  try {
    const stateRow = db
      .prepare(
        'SELECT ledger_offset_bytes, ledger_mtime_ms FROM archive_state WHERE id = 1',
      )
      .get() as { ledger_offset_bytes?: number | bigint; ledger_mtime_ms?: number | bigint } | undefined;
    let startOffset = stateRow ? Number(stateRow.ledger_offset_bytes ?? 0) : 0;

    const nowIso = new Date().toISOString();

    if (!(await fileExists(ledger))) {
      // No ledger on disk: still stamp build/rebuild timestamps so `archive
      // status` reflects this run, but leave the cursor at zero.
      if (opts.isRebuild) {
        db.prepare(
          `UPDATE archive_state SET last_built_at = ?, last_rebuild_at = ? WHERE id = 1`,
        ).run(nowIso, nowIso);
      } else {
        db.prepare(
          `UPDATE archive_state SET last_built_at = ? WHERE id = 1`,
        ).run(nowIso);
      }
      return {
        scannedBytes: 0,
        turnsApplied: 0,
        sessionsTouched: 0,
        stampsApplied: 0,
        compactionsApplied: 0,
        toolResultEventsApplied: 0,
        rebuiltFromZero: false,
      };
    }

    const ledgerStat = await stat(ledger);

    // If the ledger shrank or was rewritten in place (e.g. `burn rebuild
    // --reclassify` swaps the file via rename), our byte cursor is no longer
    // meaningful. Rebuild from byte zero rather than guessing.
    if (ledgerStat.size < startOffset) {
      db.exec('DELETE FROM sessions');
      db.exec('DELETE FROM turns');
      db.exec('DELETE FROM tool_calls');
      db.exec('DELETE FROM tool_result_events');
      db.exec('DELETE FROM compactions');
      db.prepare(
        'UPDATE archive_state SET ledger_offset_bytes = 0, ledger_mtime_ms = 0 WHERE id = 1',
      ).run();
      startOffset = 0;
    }

    const result = await applyLedgerRange(db, ledger, startOffset);

    // Use the parser's safe boundary (last newline-terminated byte) as the
    // ledger cursor — NOT `ledgerStat.size` — so a partial trailing line gets
    // re-read on the next build instead of being silently skipped. mtime
    // tracks the on-disk file as observed at this build, which is fine even
    // if the cursor is short of EOF.
    if (opts.isRebuild) {
      db.prepare(
        `UPDATE archive_state
         SET ledger_offset_bytes = ?, ledger_mtime_ms = ?, last_built_at = ?, last_rebuild_at = ?
         WHERE id = 1`,
      ).run(result.safeOffset, Math.floor(ledgerStat.mtimeMs), nowIso, nowIso);
    } else {
      db.prepare(
        `UPDATE archive_state
         SET ledger_offset_bytes = ?, ledger_mtime_ms = ?, last_built_at = ?
         WHERE id = 1`,
      ).run(result.safeOffset, Math.floor(ledgerStat.mtimeMs), nowIso);
    }

    return {
      scannedBytes: result.scannedBytes,
      turnsApplied: result.turnsApplied,
      sessionsTouched: result.sessionsTouched,
      stampsApplied: result.stampsApplied,
      compactionsApplied: result.compactionsApplied,
      toolResultEventsApplied: result.toolResultEventsApplied,
      rebuiltFromZero: false,
    };
  } finally {
    db.close();
  }
}

interface ApplyResult {
  scannedBytes: number;
  turnsApplied: number;
  sessionsTouched: number;
  stampsApplied: number;
  compactionsApplied: number;
  toolResultEventsApplied: number;
  /**
   * Byte offset of the parser's last newline boundary inside the ledger.
   * Equals `startOffset` when nothing was scanned. The caller stamps this
   * (NOT the file size) as the ledger cursor so a partial trailing line is
   * re-read on the next build instead of being silently skipped.
   */
  safeOffset: number;
}

/**
 * Stream the ledger from `startOffset` to EOF, splitting on newlines, and
 * apply each parsed line to the archive in a single transaction.
 *
 * We deliberately do NOT support resuming mid-line: the ledger appends one
 * complete JSON object per line via `appendFile`, so a partial trailing line
 * means a writer was interrupted. We stop at the last newline boundary and
 * record that as the new cursor; the truncated tail will be reprocessed on
 * the next build (and will either be complete by then or still incomplete,
 * either way safe).
 */
async function applyLedgerRange(
  db: DatabaseSync,
  ledger: string,
  startOffset: number,
): Promise<ApplyResult> {
  // Two-pass approach: first scan the new tail for stamps, since we need
  // them to enrich freshly-materialized turns. The full-archive enrichment
  // path (stamps that arrived earlier and reference newer sessions) is
  // handled by re-folding affected sessions at the end.
  //
  // A future optimization is to keep a `stamps` table and JOIN at query
  // time, but materializing into the row keeps SELECTs simple and matches
  // the issue's "materialized enrichment columns" goal.
  const newStamps: StampLine[] = [];
  const turnLines: TurnLine[] = [];
  const compactionLines: CompactionLine[] = [];
  const toolResultEventLines: ToolResultEventLine[] = [];

  let bytesScanned = 0;
  let safeOffset = startOffset;
  for await (const item of readJsonlFrom(ledger, startOffset)) {
    bytesScanned += item.byteLength;
    safeOffset = item.endOffset;
    const parsed = item.parsed;
    if (isTurnLine(parsed)) {
      turnLines.push(parsed);
    } else if (isStampLine(parsed)) {
      newStamps.push(parsed);
    } else if (isCompactionLine(parsed)) {
      compactionLines.push(parsed);
    } else if (isToolResultEventLine(parsed)) {
      toolResultEventLines.push(parsed);
    }
  }

  if (
    turnLines.length === 0 &&
    newStamps.length === 0 &&
    compactionLines.length === 0 &&
    toolResultEventLines.length === 0
  ) {
    return {
      scannedBytes: bytesScanned,
      turnsApplied: 0,
      sessionsTouched: 0,
      stampsApplied: 0,
      compactionsApplied: 0,
      toolResultEventsApplied: 0,
      safeOffset,
    };
  }

  // Collect every stamp in the ledger up to and including the new tail. We
  // need the full set to fold onto a turn whose session was first stamped
  // long ago. Cheap on small ledgers; for huge ledgers a stamp index is the
  // obvious next step but not in scope for the foundation PR.
  const allStamps = await collectAllStamps(ledger);

  const sessionsTouched = new Set<string>();
  for (const tl of turnLines) {
    sessionsTouched.add(`${tl.record.source}|${tl.record.sessionId}`);
  }
  for (const s of newStamps) {
    if (s.selector.sessionId) {
      // Stamps may reference a session whose turns we have not seen in this
      // tail; we still need to touch its row to refold enrichment.
      sessionsTouched.add(`*|${s.selector.sessionId}`);
    }
  }

  // One transaction for the whole tail. SQLite's per-statement fsync would
  // dominate runtime otherwise (thousands of inserts per ingest is normal).
  db.exec('BEGIN');
  try {
    const insertTurn = db.prepare(`
      INSERT INTO turns (
        source, session_id, message_id, turn_index, ts, model, project, project_key,
        activity, stop_reason, has_edits, retries,
        is_sidechain, subagent_id, parent_subagent_id, parent_tool_use_id, subagent_type,
        subagent_description,
        input_tokens, output_tokens, reasoning_tokens,
        cache_read_tokens, cache_create_5m_tokens, cache_create_1h_tokens,
        workflow_id, agent_id, persona, tier, enrichment_json,
        attribution_fidelity, tokens_present, cost_present
      ) VALUES (
        ?, ?, ?, ?, ?, ?, ?, ?,
        ?, ?, ?, ?,
        ?, ?, ?, ?, ?,
        ?,
        ?, ?, ?,
        ?, ?, ?,
        ?, ?, ?, ?, ?,
        ?, ?, ?
      )
      ON CONFLICT(source, session_id, message_id) DO UPDATE SET
        turn_index = excluded.turn_index,
        ts = excluded.ts,
        model = excluded.model,
        project = excluded.project,
        project_key = excluded.project_key,
        activity = excluded.activity,
        stop_reason = excluded.stop_reason,
        has_edits = excluded.has_edits,
        retries = excluded.retries,
        is_sidechain = excluded.is_sidechain,
        subagent_id = excluded.subagent_id,
        parent_subagent_id = excluded.parent_subagent_id,
        parent_tool_use_id = excluded.parent_tool_use_id,
        subagent_type = excluded.subagent_type,
        subagent_description = excluded.subagent_description,
        input_tokens = excluded.input_tokens,
        output_tokens = excluded.output_tokens,
        reasoning_tokens = excluded.reasoning_tokens,
        cache_read_tokens = excluded.cache_read_tokens,
        cache_create_5m_tokens = excluded.cache_create_5m_tokens,
        cache_create_1h_tokens = excluded.cache_create_1h_tokens,
        workflow_id = excluded.workflow_id,
        agent_id = excluded.agent_id,
        persona = excluded.persona,
        tier = excluded.tier,
        enrichment_json = excluded.enrichment_json,
        attribution_fidelity = excluded.attribution_fidelity,
        tokens_present = excluded.tokens_present,
        cost_present = excluded.cost_present
    `);

    const deleteToolCalls = db.prepare(
      'DELETE FROM tool_calls WHERE source = ? AND session_id = ? AND message_id = ?',
    );
    const insertToolCall = db.prepare(`
      INSERT INTO tool_calls (
        source, session_id, message_id, call_index,
        tool_use_id, tool_name, target, args_hash, is_error
      ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
    `);

    for (const tl of turnLines) {
      const t = tl.record;
      const enrichment = foldStamps(t, allStamps);
      writeTurn(insertTurn, t, enrichment);
      deleteToolCalls.run(t.source, t.sessionId, t.messageId);
      for (let i = 0; i < t.toolCalls.length; i++) {
        const tc = t.toolCalls[i]!;
        insertToolCall.run(
          t.source,
          t.sessionId,
          t.messageId,
          i,
          tc.id ?? null,
          tc.name,
          tc.target ?? null,
          tc.argsHash ?? null,
          tc.isError === undefined ? null : tc.isError ? 1 : 0,
        );
      }
    }

    // For every session that received a brand-new stamp in this tail, refold
    // enrichment onto its existing rows so consumers see the latest values.
    if (newStamps.length > 0) {
      const refold = db.prepare(`
        SELECT source, session_id, message_id, ts FROM turns
        WHERE session_id = ?
      `);
      const refoldUpdate = db.prepare(`
        UPDATE turns
        SET workflow_id = ?, agent_id = ?, persona = ?, tier = ?, enrichment_json = ?
        WHERE source = ? AND session_id = ? AND message_id = ?
      `);
      const sessionIdsForRefold = new Set<string>();
      for (const s of newStamps) {
        if (s.selector.sessionId) sessionIdsForRefold.add(s.selector.sessionId);
        if (s.selector.messageId) {
          // messageId stamps are rare but we still need the session row for
          // the refold lookup; the stamp will only fold onto its one turn.
          const row = db
            .prepare('SELECT session_id FROM turns WHERE message_id = ? LIMIT 1')
            .get(s.selector.messageId) as { session_id?: string } | undefined;
          if (row?.session_id) sessionIdsForRefold.add(row.session_id);
        }
      }
      for (const sid of sessionIdsForRefold) {
        const rows = refold.all(sid) as Array<{
          source: string;
          session_id: string;
          message_id: string;
          ts: string;
        }>;
        for (const row of rows) {
          // Synthesize the minimum TurnRecord shape stampMatches needs.
          const fakeTurn: Pick<TurnRecord, 'sessionId' | 'messageId' | 'ts'> = {
            sessionId: row.session_id,
            messageId: row.message_id,
            ts: row.ts,
          };
          const enrichment = foldStampsAgainst(fakeTurn, allStamps);
          refoldUpdate.run(
            enrichment['workflowId'] ?? null,
            enrichment['agentId'] ?? null,
            enrichment['persona'] ?? null,
            enrichment['tier'] ?? null,
            JSON.stringify(enrichment),
            row.source,
            row.session_id,
            row.message_id,
          );
        }
      }
    }

    if (compactionLines.length > 0) {
      const insertCompaction = db.prepare(`
        INSERT OR REPLACE INTO compactions (
          source, session_id, ts, preceding_message_id, tokens_before_compact
        ) VALUES (?, ?, ?, ?, ?)
      `);
      for (const cl of compactionLines) {
        const e = cl.record;
        insertCompaction.run(
          e.source,
          e.sessionId,
          e.ts,
          e.precedingMessageId ?? null,
          e.tokensBeforeCompact ?? null,
        );
      }
    }

    if (toolResultEventLines.length > 0) {
      // INSERT OR REPLACE keyed on the table's PK (source, session_id,
      // message_id, tool_use_id, event_index). The execution-graph record
      // already carries `contentLength` and `contentHash`; if a future
      // record lacks them we still write null and a follow-up can enrich
      // from the content sidecar (#33). The table allows NULL for both.
      const insertToolResultEvent = db.prepare(`
        INSERT OR REPLACE INTO tool_result_events (
          source, session_id, message_id, tool_use_id, call_index,
          event_index, status, content_length, content_hash, is_error,
          subagent_session_id, agent_id, event_source, ts
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
      `);
      for (const tre of toolResultEventLines) {
        const r = tre.record;
        insertToolResultEvent.run(
          r.source,
          r.sessionId,
          // PK requires NOT NULL message_id; some event sources (queue events,
          // subagent notifications) don't carry one. Substitute the empty
          // string so the row still lands and so two NULL-message_id events
          // for the same (source, sessionId, toolUseId, eventIndex) collide
          // on the PK (which is the desired idempotent behavior).
          r.messageId ?? '',
          r.toolUseId,
          r.callIndex ?? 0,
          r.eventIndex,
          r.status,
          r.contentLength ?? null,
          r.contentHash ?? null,
          r.isError === undefined ? null : r.isError ? 1 : 0,
          r.subagentSessionId ?? null,
          r.agentId ?? null,
          r.eventSource,
          r.ts ?? null,
        );
      }
    }

    rebuildSessions(db, sessionsTouched);

    // NOTE: the ledger cursor is intentionally NOT written here. The caller
    // (`buildArchiveLocked`) writes `safeOffset` after this transaction
    // commits, so that the cursor stamp lives alongside the mtime /
    // last_built_at update in a single coherent UPDATE.
    db.exec('COMMIT');
  } catch (err) {
    db.exec('ROLLBACK');
    throw err;
  }

  // Distinct session ids actually touched in this build (stamps may have
  // referenced sessions whose turns were already in the archive — those still
  // count once, as the same session id, regardless of source).
  const distinctSessionIds = new Set<string>();
  for (const key of sessionsTouched) {
    const sep = key.indexOf('|');
    distinctSessionIds.add(sep >= 0 ? key.slice(sep + 1) : key);
  }

  return {
    scannedBytes: bytesScanned,
    turnsApplied: turnLines.length,
    sessionsTouched: distinctSessionIds.size,
    stampsApplied: newStamps.length,
    compactionsApplied: compactionLines.length,
    toolResultEventsApplied: toolResultEventLines.length,
    safeOffset,
  };
}

/**
 * Re-derive `sessions` rows for the given (source, session) pairs from the
 * current contents of `turns`. Cheap because `turns` is indexed by session.
 */
function rebuildSessions(db: DatabaseSync, sessionsTouched: Set<string>): void {
  // Some entries are `*|<sessionId>` from stamps that targeted a session
  // we've never seen turns for. Resolve them to the actual `(source,
  // session_id)` pair if it exists; otherwise drop them — there's nothing
  // to derive a session row from.
  const resolved = new Set<string>();
  for (const key of sessionsTouched) {
    if (key.startsWith('*|')) {
      const sid = key.slice(2);
      const rows = db
        .prepare('SELECT DISTINCT source FROM turns WHERE session_id = ?')
        .all(sid) as Array<{ source: string }>;
      for (const r of rows) resolved.add(`${r.source}|${sid}`);
    } else {
      resolved.add(key);
    }
  }

  if (resolved.size === 0) return;

  // Fidelity rollup details:
  //  - min_fidelity is the *worst* class observed. Rank cost-only=1,
  //    aggregate-only=2, partial=3, usage-only=4, full=5; we MIN the rank
  //    across known-fidelity rows and map back to the label. Rows with NULL
  //    attribution_fidelity are ignored — absence ≠ "worst", it's "unknown".
  //    A session with no fidelity-tagged turns leaves min_fidelity NULL.
  //  - has_full_attribution is 1 iff there is at least one fidelity-tagged
  //    turn AND every such turn is class='full'. NULL when no turns carry
  //    fidelity, 0 when at least one falls below 'full'. We deliberately do
  //    NOT treat unknown-fidelity rows as failing — they're unknown.
  const upsertSession = db.prepare(`
    INSERT INTO sessions (
      source, session_id, project, project_key, started_at, ended_at,
      turn_count, model_set_json, workflow_id, agent_id, parent_agent_id, has_subagent,
      min_fidelity, has_full_attribution
    )
    SELECT
      source,
      session_id,
      MIN(project),
      MIN(project_key),
      MIN(ts),
      MAX(ts),
      COUNT(*),
      json_group_array(DISTINCT model),
      MIN(workflow_id),
      MIN(agent_id),
      MIN(parent_subagent_id),
      COALESCE(MAX(is_sidechain), 0),
      CASE MIN(CASE attribution_fidelity
                 WHEN 'cost-only'      THEN 1
                 WHEN 'aggregate-only' THEN 2
                 WHEN 'partial'        THEN 3
                 WHEN 'usage-only'     THEN 4
                 WHEN 'full'           THEN 5
                 ELSE NULL
               END)
        WHEN 1 THEN 'cost-only'
        WHEN 2 THEN 'aggregate-only'
        WHEN 3 THEN 'partial'
        WHEN 4 THEN 'usage-only'
        WHEN 5 THEN 'full'
        ELSE NULL
      END,
      CASE
        WHEN SUM(CASE WHEN attribution_fidelity IS NOT NULL THEN 1 ELSE 0 END) = 0 THEN NULL
        WHEN SUM(CASE WHEN attribution_fidelity IS NOT NULL AND attribution_fidelity <> 'full' THEN 1 ELSE 0 END) = 0 THEN 1
        ELSE 0
      END
    FROM turns
    WHERE source = ? AND session_id = ?
    GROUP BY source, session_id
    ON CONFLICT(source, session_id) DO UPDATE SET
      project = excluded.project,
      project_key = excluded.project_key,
      started_at = excluded.started_at,
      ended_at = excluded.ended_at,
      turn_count = excluded.turn_count,
      model_set_json = excluded.model_set_json,
      workflow_id = excluded.workflow_id,
      agent_id = excluded.agent_id,
      parent_agent_id = excluded.parent_agent_id,
      has_subagent = excluded.has_subagent,
      min_fidelity = excluded.min_fidelity,
      has_full_attribution = excluded.has_full_attribution
  `);

  for (const key of resolved) {
    const sep = key.indexOf('|');
    const source = key.slice(0, sep);
    const sessionId = key.slice(sep + 1);
    upsertSession.run(source, sessionId);
  }
}

function writeTurn(
  insertTurn: ReturnType<DatabaseSync['prepare']>,
  t: TurnRecord,
  enrichment: Enrichment,
): void {
  // Project the optional fidelity metadata onto the three persisted columns.
  // Absence (older lines pre-#41 / pre-Codex/OpenCode #84/#89) → all NULL,
  // which downstream queries should read as "unknown".
  let attributionFidelity: string | null = null;
  let tokensPresent: number | null = null;
  let costPresent: number | null = null;
  if (t.fidelity) {
    attributionFidelity = t.fidelity.class;
    const cov = t.fidelity.coverage;
    tokensPresent =
      cov.hasInputTokens || cov.hasOutputTokens || cov.hasReasoningTokens ? 1 : 0;
    costPresent = t.fidelity.granularity === 'cost-only' ? 1 : 0;
  }

  insertTurn.run(
    t.source,
    t.sessionId,
    t.messageId,
    t.turnIndex,
    t.ts,
    t.model,
    t.project ?? null,
    t.projectKey ?? null,
    t.activity ?? null,
    t.stopReason ?? null,
    t.hasEdits === undefined ? null : t.hasEdits ? 1 : 0,
    t.retries ?? null,
    t.subagent ? (t.subagent.isSidechain ? 1 : 0) : null,
    t.subagent?.agentId ?? null,
    t.subagent?.parentAgentId ?? null,
    t.subagent?.parentToolUseId ?? null,
    t.subagent?.subagentType ?? null,
    t.subagent?.description ?? null,
    t.usage.input,
    t.usage.output,
    t.usage.reasoning,
    t.usage.cacheRead,
    t.usage.cacheCreate5m,
    t.usage.cacheCreate1h,
    enrichment['workflowId'] ?? null,
    enrichment['agentId'] ?? null,
    enrichment['persona'] ?? null,
    enrichment['tier'] ?? null,
    JSON.stringify(enrichment),
    attributionFidelity,
    tokensPresent,
    costPresent,
  );
}

function foldStamps(turn: TurnRecord, stamps: StampLine[]): Enrichment {
  return foldStampsAgainst(turn, stamps);
}

function foldStampsAgainst(
  turn: Pick<TurnRecord, 'sessionId' | 'messageId' | 'ts'>,
  stamps: StampLine[],
): Enrichment {
  const out: Enrichment = {};
  for (const s of stamps) {
    if (!stampMatches(s, turn as TurnRecord)) continue;
    for (const [k, v] of Object.entries(s.enrichment)) {
      out[k] = v;
    }
  }
  return out;
}

async function collectAllStamps(ledger: string): Promise<StampLine[]> {
  const stamps: StampLine[] = [];
  if (!(await fileExists(ledger))) return stamps;
  const rl = createInterface({
    input: createReadStream(ledger, { encoding: 'utf8' }),
    crlfDelay: Infinity,
  });
  try {
    for await (const line of rl) {
      const t = line.trim();
      if (!t) continue;
      try {
        const parsed = JSON.parse(t);
        if (isStampLine(parsed)) stamps.push(parsed);
      } catch {
        // skip malformed
      }
    }
  } finally {
    rl.close();
  }
  stamps.sort((a, b) => a.ts.localeCompare(b.ts));
  return stamps;
}

interface JsonlItem {
  parsed: unknown;
  byteLength: number;
  endOffset: number;
}

/**
 * Stream complete JSON-per-line objects from `filePath` starting at byte
 * `startOffset`, yielding (parsed, endOffset) for each. Stops cleanly at the
 * last newline boundary; partial trailing data is left for the next call.
 */
async function* readJsonlFrom(filePath: string, startOffset: number): AsyncIterable<JsonlItem> {
  const stream = createReadStream(filePath, {
    encoding: 'utf8',
    start: startOffset,
  });
  let buf = '';
  let cursor = startOffset;
  for await (const chunkRaw of stream) {
    const chunk = typeof chunkRaw === 'string' ? chunkRaw : (chunkRaw as Buffer).toString('utf8');
    buf += chunk;
    let nl: number;
    while ((nl = buf.indexOf('\n')) !== -1) {
      const line = buf.slice(0, nl);
      const lineByteLen = Buffer.byteLength(line, 'utf8') + 1; // +1 for the LF
      buf = buf.slice(nl + 1);
      cursor += lineByteLen;
      const trimmed = line.trim();
      if (trimmed.length === 0) {
        continue;
      }
      try {
        const parsed = JSON.parse(trimmed);
        yield { parsed, byteLength: lineByteLen, endOffset: cursor };
      } catch {
        // Skip malformed lines but advance the cursor past them — a corrupt
        // line at the start of the unprocessed tail would otherwise wedge the
        // build forever. Same defensive stance the streaming reader takes.
      }
    }
  }
}

/**
 * Snapshot the archive's current state without acquiring the build lock.
 * Safe to call alongside a running `buildArchive` — SQLite WAL gives us a
 * consistent read view.
 */
export async function getArchiveStatus(): Promise<ArchiveStatus> {
  const dbPath = archivePath();
  const exists = await fileExists(dbPath);
  const ledger = ledgerPath();
  let ledgerSize = 0;
  let ledgerMtime = 0;
  if (await fileExists(ledger)) {
    const st = await stat(ledger);
    ledgerSize = st.size;
    ledgerMtime = Math.floor(st.mtimeMs);
  }

  if (!exists) {
    return {
      archivePath: dbPath,
      exists: false,
      archiveVersion: ARCHIVE_VERSION,
      ledgerOffsetBytes: 0,
      ledgerMtimeMs: 0,
      ledgerSizeBytes: ledgerSize,
      ledgerMtimeMsCurrent: ledgerMtime,
      upToDate: false,
      lastBuiltAt: null,
      lastRebuildAt: null,
      rowCounts: {
        sessions: 0,
        turns: 0,
        toolCalls: 0,
        toolResultEvents: 0,
        compactions: 0,
      },
      fidelityHistogram: {},
    };
  }

  const db = await openArchive();
  try {
    const state = db
      .prepare(
        `SELECT ledger_offset_bytes, ledger_mtime_ms, archive_version,
                last_built_at, last_rebuild_at FROM archive_state WHERE id = 1`,
      )
      .get() as
      | {
          ledger_offset_bytes?: number | bigint;
          ledger_mtime_ms?: number | bigint;
          archive_version?: number | bigint;
          last_built_at?: string | null;
          last_rebuild_at?: string | null;
        }
      | undefined;
    const offset = state ? Number(state.ledger_offset_bytes ?? 0) : 0;
    const mtime = state ? Number(state.ledger_mtime_ms ?? 0) : 0;
    const version = state ? Number(state.archive_version ?? ARCHIVE_VERSION) : ARCHIVE_VERSION;

    const sessions = db.prepare('SELECT COUNT(*) AS n FROM sessions').get() as { n: number | bigint };
    const turns = db.prepare('SELECT COUNT(*) AS n FROM turns').get() as { n: number | bigint };
    const toolCalls = db.prepare('SELECT COUNT(*) AS n FROM tool_calls').get() as {
      n: number | bigint;
    };
    const toolResultEvents = db
      .prepare('SELECT COUNT(*) AS n FROM tool_result_events')
      .get() as { n: number | bigint };
    const compactions = db.prepare('SELECT COUNT(*) AS n FROM compactions').get() as {
      n: number | bigint;
    };

    // Fidelity histogram on `turns`. NULL bucket surfaces as `unknown` so
    // JSON consumers can spot upstream gaps (Codex/OpenCode pre-#84/#89, or
    // any older line that pre-dates `TurnRecord.fidelity`).
    const fidelityRows = db
      .prepare(
        `SELECT COALESCE(attribution_fidelity, 'unknown') AS k, COUNT(*) AS n
         FROM turns
         GROUP BY COALESCE(attribution_fidelity, 'unknown')`,
      )
      .all() as Array<{ k: string; n: number | bigint }>;
    const fidelityHistogram: Record<string, number> = {};
    for (const r of fidelityRows) {
      fidelityHistogram[r.k] = Number(r.n);
    }

    return {
      archivePath: dbPath,
      exists: true,
      archiveVersion: version,
      ledgerOffsetBytes: offset,
      ledgerMtimeMs: mtime,
      ledgerSizeBytes: ledgerSize,
      ledgerMtimeMsCurrent: ledgerMtime,
      upToDate: offset === ledgerSize,
      lastBuiltAt: state?.last_built_at ?? null,
      lastRebuildAt: state?.last_rebuild_at ?? null,
      rowCounts: {
        sessions: Number(sessions.n),
        turns: Number(turns.n),
        toolCalls: Number(toolCalls.n),
        toolResultEvents: Number(toolResultEvents.n),
        compactions: Number(compactions.n),
      },
      fidelityHistogram,
    };
  } finally {
    db.close();
  }
}
