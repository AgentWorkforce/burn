import { AsyncLocalStorage } from 'node:async_hooks';
import { createHash } from 'node:crypto';
import { mkdir } from 'node:fs/promises';
import * as path from 'node:path';
import { DatabaseSync, type StatementSync } from 'node:sqlite';

import type {
  CompactionEvent,
  ContentRecord,
  SessionRelationshipRecord,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import {
  CONTENT_WINDOW,
  compactionIdHash,
  relationshipIdHash,
  toolResultEventIdHash,
  turnContentFingerprint,
  turnIdHash,
  userTurnIdHash,
} from '../index-sidecar.js';
import { isValidSessionId, sqlitePath } from '../paths.js';
import {
  stampMatches,
  type Enrichment,
  type StampLine,
} from '../schema.js';
import type { PruneOptions, PruneResult, ReadContentSelector } from '../content.js';
import type { EnrichedTurn, Query } from '../reader.js';
import type { ContentLine, StorageAdapter } from './adapter.js';
import { LATEST_VERSION, MIGRATIONS } from './migrations/index.js';

// `node:sqlite` is still flagged ExperimentalWarning in Node 22 even though
// it works without `--experimental-sqlite`. The same filter the archive uses
// in `archive.ts` would double-install here; share state via a module-level
// guard so either entry point can install it once and the other is a no-op.
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

// How long another process can hold a named lock before we treat its row in
// `locks` as orphaned and break it. Mirrors `STALE_MS` in `file-lock.ts` so
// the two adapters give callers the same self-healing window. Tests can
// shrink it via the `staleMs` constructor option.
const DEFAULT_STALE_MS = 5_000;

// Wait budget for `BEGIN IMMEDIATE` to win the database write lock. SQLite's
// `PRAGMA busy_timeout` retries internally until this elapses, so a brief
// concurrent writer never surfaces as `SQLITE_BUSY` to the caller. 30s
// generously covers a slow archive build.
const BUSY_TIMEOUT_MS = 30_000;

// Retry cadence for `withLock` waiting on a named row in `locks`. Two-phase
// like `FileLockManager`: tight retries cover ordinary contention, slower
// retries outlast `DEFAULT_STALE_MS` so a single invocation can self-heal an
// orphan left by a crashed peer.
const FAST_RETRY_DELAY_MS = 20;
const FAST_RETRIES = 50;
const SLOW_RETRY_DELAY_MS = 250;
const SLOW_RETRIES = 40;

export interface SqliteAdapterOptions {
  /** Override the on-disk DB path. Defaults to `sqlitePath()`. */
  dbPath?: string;
  /** Override the named-lock stale threshold. Tests use a small value. */
  staleMs?: number;
}

export class SqliteAdapter implements StorageAdapter {
  readonly kind = 'sqlite' as const;

  private readonly dbPath: string;
  private readonly staleMs: number;
  private db: DatabaseSync | undefined;
  private readonly heldLocks = new AsyncLocalStorage<Set<string>>();
  // Per-process serialization of named-lock acquisition. Without this, two
  // async tasks in the same process can both see "row absent" and both try
  // to insert — the second hits a UNIQUE violation and bubbles up. Funnels
  // them through one Promise chain per lock name instead.
  private readonly localQueues = new Map<string, Promise<void>>();

  constructor(options: SqliteAdapterOptions = {}) {
    installSqliteWarningFilter();
    this.dbPath = options.dbPath ?? sqlitePath();
    this.staleMs = options.staleMs ?? DEFAULT_STALE_MS;
  }

  async init(): Promise<void> {
    if (this.db) return;
    await mkdir(path.dirname(this.dbPath), { recursive: true });
    this.db = new DatabaseSync(this.dbPath);
    // WAL gives concurrent readers while a writer holds BEGIN IMMEDIATE; the
    // busy_timeout makes `SQLITE_BUSY` block-and-retry rather than failing
    // the caller mid-transaction.
    this.db.exec('PRAGMA journal_mode = WAL');
    this.db.exec('PRAGMA foreign_keys = ON');
    this.db.exec('PRAGMA synchronous = NORMAL');
    this.db.exec(`PRAGMA busy_timeout = ${BUSY_TIMEOUT_MS}`);
    this.applyMigrations();
  }

  async close(): Promise<void> {
    if (!this.db) return;
    try {
      this.db.close();
    } finally {
      this.db = undefined;
    }
  }

  private requireDb(): DatabaseSync {
    if (!this.db) {
      throw new Error('SqliteAdapter not initialized; call init() first');
    }
    return this.db;
  }

  private async ensureInit(): Promise<DatabaseSync> {
    if (!this.db) await this.init();
    return this.requireDb();
  }

  private applyMigrations(): void {
    const db = this.requireDb();
    // Ensure `schema_state` exists before reading it — the very first
    // migration is what creates it, so on a brand-new DB the SELECT below
    // would otherwise fail. Bootstrap the row inside the migration SQL.
    let onDiskVersion = 0;
    try {
      const row = db
        .prepare('SELECT version FROM schema_state WHERE id = 1')
        .get() as { version?: number | bigint } | undefined;
      onDiskVersion = row && row.version !== undefined ? Number(row.version) : 0;
    } catch {
      // schema_state missing: unmigrated DB. Treat as version 0.
      onDiskVersion = 0;
    }

    for (const migration of MIGRATIONS) {
      if (migration.version <= onDiskVersion) continue;
      const sql = migration.sql.sqlite;
      if (!sql) {
        throw new Error(
          `migration ${migration.version} has no sqlite SQL — cannot apply`,
        );
      }
      db.exec('BEGIN IMMEDIATE');
      try {
        db.exec(sql);
        db.prepare(
          `INSERT INTO schema_state (id, version) VALUES (1, ?)
           ON CONFLICT(id) DO UPDATE SET version = excluded.version`,
        ).run(migration.version);
        db.exec('COMMIT');
      } catch (err) {
        db.exec('ROLLBACK');
        throw err;
      }
    }

    if (onDiskVersion > LATEST_VERSION) {
      throw new Error(
        `sqlite store is at schema version ${onDiskVersion} but this build only ` +
          `knows up to ${LATEST_VERSION}; please upgrade @relayburn/ledger`,
      );
    }
  }

  // ---------------------------------------------------------------------
  // Cross-process named locks.
  //
  // Acquire = INSERT a row keyed on `name`; release = DELETE it. The INSERT
  // happens inside `BEGIN IMMEDIATE` so two processes racing for the same
  // name see one win and one collide on the UNIQUE primary key. Stale rows
  // (acquired_at_ms older than `staleMs`) get reaped opportunistically so a
  // single invocation can wait out a crashed peer.
  // ---------------------------------------------------------------------

  async withLock<T>(name: string, fn: () => Promise<T>): Promise<T> {
    const held = this.heldLocks.getStore();
    if (held?.has(name)) return fn();

    // Per-process queue: serialize acquisitions of the same name so two
    // concurrent withLock calls in this process don't both observe an
    // empty row and both attempt the same INSERT.
    const prev = this.localQueues.get(name) ?? Promise.resolve();
    let release!: () => void;
    const next = new Promise<void>((resolve) => {
      release = resolve;
    });
    this.localQueues.set(name, prev.then(() => next));
    await prev;

    try {
      await this.ensureInit();
      await this.acquireRow(name);
      const nextHeld = new Set(held ?? []);
      nextHeld.add(name);
      try {
        return await this.heldLocks.run(nextHeld, fn);
      } finally {
        this.releaseRow(name);
      }
    } finally {
      release();
      // Best-effort GC of finished queue entries so the Map doesn't grow
      // unbounded across long-lived adapters.
      if (this.localQueues.get(name) === prev.then(() => next)) {
        // Microtask hand-off: clear once any waiters chained on us settle.
        queueMicrotask(() => {
          if (this.localQueues.get(name)?.then === undefined) {
            this.localQueues.delete(name);
          }
        });
      }
    }
  }

  private async acquireRow(name: string): Promise<void> {
    const db = this.requireDb();
    const total = FAST_RETRIES + SLOW_RETRIES;
    for (let attempt = 0; attempt < total; attempt++) {
      const acquired = this.tryAcquireRowOnce(db, name);
      if (acquired) return;
      const delayMs =
        attempt < FAST_RETRIES ? FAST_RETRY_DELAY_MS : SLOW_RETRY_DELAY_MS;
      await delay(delayMs);
    }
    throw new Error(
      `could not acquire sqlite lock '${name}' after ${total} attempts (~${
        FAST_RETRIES * FAST_RETRY_DELAY_MS + SLOW_RETRIES * SLOW_RETRY_DELAY_MS
      }ms)`,
    );
  }

  private tryAcquireRowOnce(db: DatabaseSync, name: string): boolean {
    const now = Date.now();
    db.exec('BEGIN IMMEDIATE');
    try {
      const existing = db
        .prepare('SELECT acquired_at_ms FROM locks WHERE name = ?')
        .get(name) as { acquired_at_ms?: number | bigint } | undefined;
      if (existing) {
        const age = now - Number(existing.acquired_at_ms ?? 0);
        if (age < this.staleMs) {
          db.exec('ROLLBACK');
          return false;
        }
        // Stale: break it. The owning process either crashed or wedged for
        // longer than the threshold; either way reclaiming is safe.
        db.prepare('DELETE FROM locks WHERE name = ?').run(name);
      }
      db.prepare(
        'INSERT INTO locks (name, acquired_at_ms, pid) VALUES (?, ?, ?)',
      ).run(name, now, process.pid);
      db.exec('COMMIT');
      return true;
    } catch (err) {
      try {
        db.exec('ROLLBACK');
      } catch {
        // already rolled back; ignore
      }
      // UNIQUE conflict is a normal "another acquirer beat us"; surface as
      // "not acquired" so the caller retries. Anything else is a real
      // failure and bubbles.
      const code = (err as { code?: string }).code;
      if (
        code === 'SQLITE_CONSTRAINT_UNIQUE' ||
        code === 'SQLITE_CONSTRAINT_PRIMARYKEY' ||
        code === 'SQLITE_CONSTRAINT'
      ) {
        return false;
      }
      const message = (err as { message?: string }).message ?? '';
      if (/UNIQUE constraint failed|constraint failed/i.test(message)) {
        return false;
      }
      throw err;
    }
  }

  private releaseRow(name: string): void {
    if (!this.db) return;
    try {
      this.db.prepare('DELETE FROM locks WHERE name = ?').run(name);
    } catch {
      // Best-effort: a stale-break by another acquirer may have already
      // removed the row. Either way the next acquirer will see an empty
      // slot.
    }
  }

  // ---------------------------------------------------------------------
  // Append paths. INSERT OR IGNORE collapses duplicates against the
  // primary-key dedup hash, replacing the JSONL adapter's `.idx` sidecar.
  // ---------------------------------------------------------------------

  async appendTurns(turns: TurnRecord[]): Promise<void> {
    if (turns.length === 0) return;
    const db = await this.ensureInit();
    const recentContent = db.prepare(
      `SELECT content_fp FROM turns ORDER BY rowid DESC LIMIT ?`,
    );
    const insert = db.prepare(`
      INSERT OR IGNORE INTO turns (
        id_hash, content_fp, source, session_id, message_id, turn_index,
        ts, project, project_key, record_json
      ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    `);
    db.exec('BEGIN IMMEDIATE');
    try {
      const historicalContent = new Set(
        Array.from(
          recentContent.iterate(CONTENT_WINDOW) as IterableIterator<{ content_fp: string }>,
          (row) => row.content_fp,
        ),
      );
      for (const t of turns) {
        const contentFp = turnContentFingerprint(t);
        if (historicalContent.has(contentFp)) continue;
        insert.run(
          turnIdHash(t),
          contentFp,
          t.source,
          t.sessionId,
          t.messageId,
          t.turnIndex,
          t.ts,
          t.project ?? null,
          t.projectKey ?? null,
          JSON.stringify(t),
        );
      }
      db.exec('COMMIT');
    } catch (err) {
      db.exec('ROLLBACK');
      throw err;
    }
  }

  async appendCompactions(events: CompactionEvent[]): Promise<void> {
    if (events.length === 0) return;
    const db = await this.ensureInit();
    const insert = db.prepare(`
      INSERT OR IGNORE INTO compactions (
        id_hash, source, session_id, ts, record_json
      ) VALUES (?, ?, ?, ?, ?)
    `);
    db.exec('BEGIN IMMEDIATE');
    try {
      for (const e of events) {
        insert.run(
          compactionIdHash(e),
          e.source,
          e.sessionId,
          e.ts,
          JSON.stringify(e),
        );
      }
      db.exec('COMMIT');
    } catch (err) {
      db.exec('ROLLBACK');
      throw err;
    }
  }

  async appendRelationships(records: SessionRelationshipRecord[]): Promise<void> {
    if (records.length === 0) return;
    const db = await this.ensureInit();
    const insert = db.prepare(`
      INSERT OR IGNORE INTO relationships (
        id_hash, source, session_id, related_session_id, ts, record_json
      ) VALUES (?, ?, ?, ?, ?, ?)
    `);
    db.exec('BEGIN IMMEDIATE');
    try {
      for (const r of records) {
        insert.run(
          relationshipIdHash(r),
          r.source,
          r.sessionId,
          r.relatedSessionId ?? null,
          r.ts ?? null,
          JSON.stringify(r),
        );
      }
      db.exec('COMMIT');
    } catch (err) {
      db.exec('ROLLBACK');
      throw err;
    }
  }

  async appendUserTurns(records: UserTurnRecord[]): Promise<void> {
    if (records.length === 0) return;
    const db = await this.ensureInit();
    const insert = db.prepare(`
      INSERT OR IGNORE INTO user_turns (
        id_hash, source, session_id, ts, record_json
      ) VALUES (?, ?, ?, ?, ?)
    `);
    db.exec('BEGIN IMMEDIATE');
    try {
      for (const r of records) {
        insert.run(
          userTurnIdHash(r),
          r.source,
          r.sessionId,
          r.ts,
          JSON.stringify(r),
        );
      }
      db.exec('COMMIT');
    } catch (err) {
      db.exec('ROLLBACK');
      throw err;
    }
  }

  async appendToolResultEvents(records: ToolResultEventRecord[]): Promise<void> {
    if (records.length === 0) return;
    const db = await this.ensureInit();
    const insert = db.prepare(`
      INSERT OR IGNORE INTO tool_result_events (
        id_hash, source, session_id, ts, record_json
      ) VALUES (?, ?, ?, ?, ?)
    `);
    db.exec('BEGIN IMMEDIATE');
    try {
      for (const r of records) {
        insert.run(
          toolResultEventIdHash(r),
          r.source,
          r.sessionId,
          r.ts ?? null,
          JSON.stringify(r),
        );
      }
      db.exec('COMMIT');
    } catch (err) {
      db.exec('ROLLBACK');
      throw err;
    }
  }

  async appendStamp(line: StampLine): Promise<void> {
    const db = await this.ensureInit();
    const relationship = spawnEnvRelationshipFromStamp(line);
    db.exec('BEGIN IMMEDIATE');
    try {
      db.prepare(
        `INSERT INTO stamps (ts, selector_json, enrichment_json, session_id, message_id)
         VALUES (?, ?, ?, ?, ?)`,
      ).run(
        line.ts,
        JSON.stringify(line.selector),
        JSON.stringify(line.enrichment),
        line.selector.sessionId ?? null,
        line.selector.messageId ?? null,
      );
      if (relationship) {
        db.prepare(
          `INSERT OR IGNORE INTO relationships (
             id_hash, source, session_id, related_session_id, ts, record_json
           ) VALUES (?, ?, ?, ?, ?, ?)`,
        ).run(
          relationshipIdHash(relationship),
          relationship.source,
          relationship.sessionId,
          relationship.relatedSessionId ?? null,
          relationship.ts ?? null,
          JSON.stringify(relationship),
        );
      }
      db.exec('COMMIT');
    } catch (err) {
      db.exec('ROLLBACK');
      throw err;
    }
  }

  async appendContent(records: ContentRecord[]): Promise<void> {
    if (records.length === 0) return;
    const filtered: ContentRecord[] = [];
    for (const r of records) {
      const sid = r.sessionId;
      if (!sid) continue;
      if (!isValidSessionId(sid)) {
        process.stderr.write(
          `[burn] skipping content record with unsafe sessionId: ${JSON.stringify(sid)}\n`,
        );
        continue;
      }
      filtered.push(r);
    }
    if (filtered.length === 0) return;
    const db = await this.ensureInit();
    const insert = db.prepare(`
      INSERT OR IGNORE INTO content (
        source, session_id, message_id, ts, content_fp, record_json, mtime_ms
      ) VALUES (?, ?, ?, ?, ?, ?, ?)
    `);
    const now = Date.now();
    db.exec('BEGIN IMMEDIATE');
    try {
      for (const r of filtered) {
        const json = JSON.stringify(r);
        const fp = sha16(json);
        insert.run(
          r.source,
          r.sessionId,
          r.messageId,
          r.ts,
          fp,
          json,
          now,
        );
      }
      db.exec('COMMIT');
    } catch (err) {
      db.exec('ROLLBACK');
      throw err;
    }
  }

  // ---------------------------------------------------------------------
  // Query paths. Use prepared statements + `iterate()` for streaming so
  // large result sets don't materialize in memory.
  // ---------------------------------------------------------------------

  async *queryTurns(q: Query = {}): AsyncIterable<EnrichedTurn> {
    const db = await this.ensureInit();
    const stamps = collectStamps(db);
    const stmt = db.prepare(
      `SELECT record_json FROM turns ORDER BY rowid`,
    );
    for (const row of stmt.iterate() as IterableIterator<{ record_json: string }>) {
      const turn = safeParseTurn(row.record_json);
      if (!turn) continue;
      const enrichment = foldStamps(turn, stamps);
      if (!turnPasses(turn, q, enrichment)) continue;
      yield { ...turn, enrichment };
    }
  }

  async *queryCompactions(q: Query = {}): AsyncIterable<CompactionEvent> {
    const db = await this.ensureInit();
    const stmt = db.prepare(`SELECT record_json FROM compactions ORDER BY rowid`);
    for (const row of stmt.iterate() as IterableIterator<{ record_json: string }>) {
      const r = safeParse<CompactionEvent>(row.record_json);
      if (!r) continue;
      if (!compactionPasses(r, q)) continue;
      yield r;
    }
  }

  async *queryRelationships(q: Query = {}): AsyncIterable<SessionRelationshipRecord> {
    const db = await this.ensureInit();
    const stmt = db.prepare(`SELECT record_json FROM relationships ORDER BY rowid`);
    for (const row of stmt.iterate() as IterableIterator<{ record_json: string }>) {
      const r = safeParse<SessionRelationshipRecord>(row.record_json);
      if (!r) continue;
      if (!relationshipPasses(r, q)) continue;
      yield r;
    }
  }

  async *queryToolResultEvents(q: Query = {}): AsyncIterable<ToolResultEventRecord> {
    const db = await this.ensureInit();
    const stmt = db.prepare(`SELECT record_json FROM tool_result_events ORDER BY rowid`);
    for (const row of stmt.iterate() as IterableIterator<{ record_json: string }>) {
      const r = safeParse<ToolResultEventRecord>(row.record_json);
      if (!r) continue;
      if (!toolResultEventPasses(r, q)) continue;
      yield r;
    }
  }

  async *queryUserTurns(q: Query = {}): AsyncIterable<UserTurnRecord> {
    const db = await this.ensureInit();
    const stmt = db.prepare(`SELECT record_json FROM user_turns ORDER BY rowid`);
    for (const row of stmt.iterate() as IterableIterator<{ record_json: string }>) {
      const r = safeParse<UserTurnRecord>(row.record_json);
      if (!r) continue;
      if (!userTurnPasses(r, q)) continue;
      yield r;
    }
  }

  async *readContent(selector: ReadContentSelector): AsyncIterable<ContentLine> {
    const db = await this.ensureInit();
    let stmt: StatementSync;
    let params: unknown[];
    if (selector.messageId !== undefined) {
      stmt = db.prepare(
        `SELECT record_json FROM content
         WHERE session_id = ? AND message_id = ?
         ORDER BY seq`,
      );
      params = [selector.sessionId, selector.messageId];
    } else {
      stmt = db.prepare(
        `SELECT record_json FROM content WHERE session_id = ? ORDER BY seq`,
      );
      params = [selector.sessionId];
    }
    for (const row of stmt.iterate(...(params as never[])) as IterableIterator<{
      record_json: string;
    }>) {
      const r = safeParse<ContentRecord>(row.record_json);
      if (!r) continue;
      yield r;
    }
  }

  async listContentSessionIds(): Promise<string[]> {
    const db = await this.ensureInit();
    const rows = db
      .prepare(`SELECT DISTINCT session_id FROM content`)
      .all() as Array<{ session_id: string }>;
    return rows.map((r) => r.session_id).filter(isValidSessionId);
  }

  async pruneContent(options: PruneOptions): Promise<PruneResult> {
    const db = await this.ensureInit();
    const cutoff = Date.now() - options.olderThanMs;
    // Identify sessions whose most recent write is older than the cutoff —
    // mirrors FileAdapter, which only deletes a sidecar when its own mtime
    // is past the threshold (a session still being written stays put).
    const candidates = db
      .prepare(
        `SELECT session_id, MAX(mtime_ms) AS max_mtime, SUM(LENGTH(record_json)) AS bytes
         FROM content GROUP BY session_id HAVING MAX(mtime_ms) <= ?`,
      )
      .all(cutoff) as Array<{
      session_id: string;
      max_mtime: number | bigint;
      bytes: number | bigint;
    }>;

    let filesDeleted = 0;
    let bytesFreed = 0;
    let skippedRecoverable = 0;
    const isRecoverable = options.isRecoverable;
    for (const row of candidates) {
      const sessionId = row.session_id;
      if (!isValidSessionId(sessionId)) continue;
      const outcome = await this.withLock(`content.${sessionId}`, async () => {
        // Re-stat inside the lock: another writer may have appended after
        // we listed candidates, pushing the session past the cutoff.
        const fresh = db
          .prepare(
            `SELECT MAX(mtime_ms) AS max_mtime, SUM(LENGTH(record_json)) AS bytes
             FROM content WHERE session_id = ?`,
          )
          .get(sessionId) as
          | { max_mtime?: number | bigint; bytes?: number | bigint }
          | undefined;
        if (!fresh || fresh.max_mtime === undefined) return null;
        if (Number(fresh.max_mtime) > cutoff) return null;
        if (isRecoverable) {
          try {
            if (await isRecoverable(sessionId)) {
              return { kind: 'skippedRecoverable' as const };
            }
          } catch {
            // fall through to delete: prune is best-effort, callers can
            // gate stricter behavior at the source-index layer
          }
        }
        const bytes = Number(fresh.bytes ?? 0);
        db.prepare('DELETE FROM content WHERE session_id = ?').run(sessionId);
        return { kind: 'deleted' as const, bytes };
      });
      if (outcome?.kind === 'deleted') {
        filesDeleted++;
        bytesFreed += outcome.bytes;
      } else if (outcome?.kind === 'skippedRecoverable') {
        skippedRecoverable++;
      }
    }
    return { filesDeleted, bytesFreed, skippedRecoverable };
  }
}

// ---------------------------------------------------------------------
// Helpers (mirror the file adapter's pure-function predicates).
// ---------------------------------------------------------------------

function delay(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

function sha16(input: string): string {
  return createHash('sha256').update(input).digest('hex').slice(0, 16);
}

function safeParse<T>(json: string): T | undefined {
  try {
    return JSON.parse(json) as T;
  } catch {
    return undefined;
  }
}

function safeParseTurn(json: string): TurnRecord | undefined {
  return safeParse<TurnRecord>(json);
}

interface CollectedStamp extends StampLine {}

function collectStamps(db: DatabaseSync): CollectedStamp[] {
  const rows = db
    .prepare(
      `SELECT ts, selector_json, enrichment_json FROM stamps ORDER BY ts, seq`,
    )
    .all() as Array<{
    ts: string;
    selector_json: string;
    enrichment_json: string;
  }>;
  const out: CollectedStamp[] = [];
  for (const row of rows) {
    let selector: StampLine['selector'] = {};
    let enrichment: Enrichment = {};
    try {
      selector = JSON.parse(row.selector_json) as StampLine['selector'];
    } catch {
      selector = {};
    }
    try {
      enrichment = JSON.parse(row.enrichment_json) as Enrichment;
    } catch {
      enrichment = {};
    }
    out.push({ v: 1, kind: 'stamp', ts: row.ts, selector, enrichment });
  }
  return out;
}

function foldStamps(turn: TurnRecord, stamps: StampLine[]): Enrichment {
  const out: Enrichment = {};
  for (const s of stamps) {
    if (!stampMatches(s, turn)) continue;
    for (const [k, v] of Object.entries(s.enrichment)) {
      out[k] = v;
    }
  }
  return out;
}

function turnPasses(turn: TurnRecord, q: Query, enrichment: Enrichment): boolean {
  if (q.since && turn.ts < q.since) return false;
  if (q.until && turn.ts > q.until) return false;
  if (q.project && turn.project !== q.project && turn.projectKey !== q.project) return false;
  if (q.sessionId && turn.sessionId !== q.sessionId) return false;
  if (q.source && turn.source !== q.source) return false;
  if (q.enrichment) {
    for (const [k, v] of Object.entries(q.enrichment)) {
      if (v === undefined) continue;
      if (enrichment[k] !== v) return false;
    }
  }
  return true;
}

function compactionPasses(e: CompactionEvent, q: Query): boolean {
  if (q.since && e.ts < q.since) return false;
  if (q.until && e.ts > q.until) return false;
  if (q.sessionId && e.sessionId !== q.sessionId) return false;
  if (q.source && e.source !== q.source) return false;
  return true;
}

function relationshipPasses(r: SessionRelationshipRecord, q: Query): boolean {
  if (q.since && r.ts && r.ts < q.since) return false;
  if (q.until && r.ts && r.ts > q.until) return false;
  if (q.sessionId && r.sessionId !== q.sessionId && r.relatedSessionId !== q.sessionId)
    return false;
  if (q.source && r.source !== q.source) return false;
  return true;
}

function toolResultEventPasses(r: ToolResultEventRecord, q: Query): boolean {
  if (q.since && r.ts && r.ts < q.since) return false;
  if (q.until && r.ts && r.ts > q.until) return false;
  if (q.sessionId && r.sessionId !== q.sessionId) return false;
  if (q.source && r.source !== q.source) return false;
  return true;
}

function userTurnPasses(r: UserTurnRecord, q: Query): boolean {
  if (q.since && r.ts < q.since) return false;
  if (q.until && r.ts > q.until) return false;
  if (q.sessionId && r.sessionId !== q.sessionId) return false;
  if (q.source && r.source !== q.source) return false;
  return true;
}

// Mirrors FileAdapter: a stamp carrying parentAgentId (the spawn-env
// signal) implies a subagent relationship between this session and its
// parent. We synthesize the row alongside the stamp so downstream queries
// see the relationship even when the source log itself didn't carry it.
function spawnEnvRelationshipFromStamp(
  line: StampLine,
): SessionRelationshipRecord | null {
  const sessionId = line.selector.sessionId;
  if (typeof sessionId !== 'string' || sessionId.length === 0) return null;
  const parentAgentId = line.enrichment['parentAgentId'];
  if (typeof parentAgentId !== 'string' || parentAgentId.length === 0) return null;

  const relationship: SessionRelationshipRecord = {
    v: 1,
    source: 'spawn-env',
    sessionId,
    relatedSessionId: parentAgentId,
    relationshipType: 'subagent',
    ts: line.ts,
  };
  const agentId = line.enrichment['agentId'];
  if (typeof agentId === 'string' && agentId.length > 0) relationship.agentId = agentId;
  return relationship;
}
