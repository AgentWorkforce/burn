import { createReadStream } from 'node:fs';
import {
  appendFile,
  mkdir,
  readFile,
  readdir,
  stat,
  unlink,
} from 'node:fs/promises';
import { createInterface } from 'node:readline';
import * as path from 'node:path';

import type {
  CompactionEvent,
  ContentRecord,
  SessionRelationshipRecord,
  SourceKind,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import {
  appendHashes,
  compactionIdHash,
  loadIndex,
  relationshipIdHash,
  toolResultEventIdHash,
  turnContentFingerprint,
  turnIdHash,
  userTurnIdHash,
} from '../index-sidecar.js';
import { contentDir, contentFilePath, isValidSessionId, ledgerPath } from '../paths.js';
import {
  isCompactionLine,
  isSessionRelationshipLine,
  isStampLine,
  isToolResultEventLine,
  isTurnLine,
  isUserTurnLine,
  stampMatches,
  type CompactionLine,
  type Enrichment,
  type LedgerLine,
  type SessionRelationshipLine,
  type StampLine,
  type ToolResultEventLine,
  type TurnLine,
  type UserTurnLine,
} from '../schema.js';
import type { PruneOptions, PruneResult, ReadContentSelector } from '../content.js';
import type { EnrichedTurn, Query } from '../reader.js';
import type { StorageAdapter } from './adapter.js';
import { FileLockManager } from './file-lock.js';

export class FileAdapter implements StorageAdapter {
  readonly kind = 'file' as const;

  private readonly locks = new FileLockManager();

  async init(): Promise<void> {}

  async close(): Promise<void> {}

  async withLock<T>(name: string, fn: () => Promise<T>): Promise<T> {
    return this.locks.withLock(name, fn);
  }

  async appendTurns(turns: TurnRecord[]): Promise<void> {
    if (turns.length === 0) return;
    const idx = await loadIndex();
    // Snapshot content set before this batch: content-fingerprint dedup only
    // compares against historically-committed turns, never within the same batch.
    const historicalContent = new Set(idx.content);
    const fresh: TurnRecord[] = [];
    const newIds: string[] = [];
    const newContent: string[] = [];
    for (const t of turns) {
      const id = turnIdHash(t);
      if (idx.ids.has(id)) continue;
      const cf = turnContentFingerprint(t);
      if (historicalContent.has(cf)) continue;
      fresh.push(t);
      newIds.push(id);
      newContent.push(cf);
      idx.ids.add(id); // primary dedup DOES apply within batch: same messageId = same turn
    }
    if (fresh.length === 0) return;
    for (const cf of newContent) idx.content.add(cf);
    const lines: TurnLine[] = fresh.map((record) => ({ v: 1, kind: 'turn', record }));
    await this.appendLines(lines);
    await appendHashes(newIds, newContent);
  }

  async appendCompactions(events: CompactionEvent[]): Promise<void> {
    if (events.length === 0) return;
    // Dedup piggybacks on the ledger-id index. Compaction ids share the same
    // namespace as turn ids: they hash different inputs so collisions are not
    // a practical concern, and we get crash-safe persistence for free.
    const idx = await loadIndex();
    const fresh: CompactionEvent[] = [];
    const newIds: string[] = [];
    for (const e of events) {
      const id = compactionIdHash(e);
      if (idx.ids.has(id)) continue;
      fresh.push(e);
      newIds.push(id);
      idx.ids.add(id);
    }
    if (fresh.length === 0) return;
    const lines: CompactionLine[] = fresh.map((record) => ({
      v: 1,
      kind: 'compaction',
      record,
    }));
    await this.appendLines(lines);
    await appendHashes(newIds, []);
  }

  async appendRelationships(records: SessionRelationshipRecord[]): Promise<void> {
    if (records.length === 0) return;
    const idx = await loadIndex();
    const fresh: SessionRelationshipRecord[] = [];
    const newIds: string[] = [];
    for (const r of records) {
      const id = relationshipIdHash(r);
      if (idx.ids.has(id)) continue;
      fresh.push(r);
      newIds.push(id);
      idx.ids.add(id);
    }
    if (fresh.length === 0) return;
    const lines: SessionRelationshipLine[] = fresh.map((record) => ({
      v: 1,
      kind: 'relationship',
      record,
    }));
    await this.appendLines(lines);
    await appendHashes(newIds, []);
  }

  async appendUserTurns(records: UserTurnRecord[]): Promise<void> {
    if (records.length === 0) return;
    // Dedup piggybacks on the ledger-id index. User-turn ids share the same
    // namespace as turn / compaction / relationship / tool-result-event ids:
    // they hash different inputs (source|sessionId|userUuid) so collisions are
    // not a practical concern, and we get crash-safe persistence for free.
    const idx = await loadIndex();
    const fresh: UserTurnRecord[] = [];
    const newIds: string[] = [];
    for (const r of records) {
      const id = userTurnIdHash(r);
      if (idx.ids.has(id)) continue;
      fresh.push(r);
      newIds.push(id);
      idx.ids.add(id);
    }
    if (fresh.length === 0) return;
    const lines: UserTurnLine[] = fresh.map((record) => ({
      v: 1,
      kind: 'user_turn',
      record,
    }));
    await this.appendLines(lines);
    await appendHashes(newIds, []);
  }

  async appendToolResultEvents(records: ToolResultEventRecord[]): Promise<void> {
    if (records.length === 0) return;
    const idx = await loadIndex();
    const fresh: ToolResultEventRecord[] = [];
    const newIds: string[] = [];
    for (const r of records) {
      const id = toolResultEventIdHash(r);
      if (idx.ids.has(id)) continue;
      fresh.push(r);
      newIds.push(id);
      idx.ids.add(id);
    }
    if (fresh.length === 0) return;
    const lines: ToolResultEventLine[] = fresh.map((record) => ({
      v: 1,
      kind: 'tool_result_event',
      record,
    }));
    await this.appendLines(lines);
    await appendHashes(newIds, []);
  }

  async appendStamp(line: StampLine): Promise<void> {
    const relationship = spawnEnvRelationshipFromStamp(line);
    if (!relationship) {
      await this.appendLines([line]);
      return;
    }

    const idx = await loadIndex();
    const id = relationshipIdHash(relationship);
    if (idx.ids.has(id)) {
      await this.appendLines([line]);
      return;
    }
    idx.ids.add(id);
    await this.appendLines([
      line,
      {
        v: 1,
        kind: 'relationship',
        record: relationship,
      },
    ]);
    await appendHashes([id], []);
  }

  async appendContent(records: ContentRecord[]): Promise<void> {
    if (records.length === 0) return;
    const grouped = new Map<string, ContentRecord[]>();
    for (const r of records) {
      const key = r.sessionId;
      if (!key) continue;
      if (!isValidSessionId(key)) {
        process.stderr.write(
          `[burn] skipping content record with unsafe sessionId: ${JSON.stringify(key)}\n`,
        );
        continue;
      }
      let bucket = grouped.get(key);
      if (!bucket) {
        bucket = [];
        grouped.set(key, bucket);
      }
      bucket.push(r);
    }
    if (grouped.size === 0) return;
    await mkdir(contentDir(), { recursive: true });
    for (const [sessionId, items] of grouped) {
      await this.appendSessionContent(sessionId, items);
    }
  }

  async *queryTurns(q: Query = {}): AsyncIterable<EnrichedTurn> {
    const filePath = ledgerPath();
    if (!(await fileExists(filePath))) return;
    const stamps = await collectStamps(filePath);
    for await (const parsed of streamLines(filePath)) {
      if (!isTurnLine(parsed)) continue;
      const enrichment = foldStamps(parsed.record, stamps);
      if (!turnPasses(parsed.record, q, enrichment)) continue;
      yield { ...parsed.record, enrichment };
    }
  }

  async *queryCompactions(q: Query = {}): AsyncIterable<CompactionEvent> {
    const filePath = ledgerPath();
    if (!(await fileExists(filePath))) return;
    for await (const parsed of streamLines(filePath)) {
      if (!isCompactionLine(parsed)) continue;
      if (!compactionPasses(parsed.record, q)) continue;
      yield parsed.record;
    }
  }

  async *queryRelationships(
    q: Query = {},
  ): AsyncIterable<SessionRelationshipRecord> {
    const filePath = ledgerPath();
    if (!(await fileExists(filePath))) return;
    for await (const parsed of streamLines(filePath)) {
      if (!isSessionRelationshipLine(parsed)) continue;
      if (!relationshipPasses(parsed.record, q)) continue;
      yield parsed.record;
    }
  }

  async *queryToolResultEvents(
    q: Query = {},
  ): AsyncIterable<ToolResultEventRecord> {
    const filePath = ledgerPath();
    if (!(await fileExists(filePath))) return;
    for await (const parsed of streamLines(filePath)) {
      if (!isToolResultEventLine(parsed)) continue;
      if (!toolResultEventPasses(parsed.record, q)) continue;
      yield parsed.record;
    }
  }

  async *queryUserTurns(q: Query = {}): AsyncIterable<UserTurnRecord> {
    const filePath = ledgerPath();
    if (!(await fileExists(filePath))) return;
    for await (const parsed of streamLines(filePath)) {
      if (!isUserTurnLine(parsed)) continue;
      if (!userTurnPasses(parsed.record, q)) continue;
      yield parsed.record;
    }
  }

  async *readContent(selector: ReadContentSelector): AsyncIterable<ContentRecord> {
    const file = contentFilePath(selector.sessionId);
    let raw: string;
    try {
      raw = await readFile(file, 'utf8');
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') return;
      throw err;
    }
    for (const line of raw.split('\n')) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      let parsed: unknown;
      try {
        parsed = JSON.parse(trimmed);
      } catch {
        continue;
      }
      if (!parsed || typeof parsed !== 'object') continue;
      const rec = parsed as ContentRecord;
      if (selector.messageId !== undefined && rec.messageId !== selector.messageId)
        continue;
      yield rec;
    }
  }

  async listContentSessionIds(): Promise<string[]> {
    const dir = contentDir();
    const out: string[] = [];
    let entries: string[];
    try {
      entries = await readdir(dir);
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') return out;
      throw err;
    }
    for (const name of entries) {
      if (!name.endsWith('.jsonl')) continue;
      const sessionId = name.slice(0, -'.jsonl'.length);
      if (!isValidSessionId(sessionId)) continue;
      // An empty sidecar signals "attempted but nothing written" and should be
      // re-parsed rather than treated as already-populated.
      try {
        const st = await stat(path.join(dir, name));
        if (st.size > 0) out.push(sessionId);
      } catch {
        // raced with deletion; ignore
      }
    }
    return out;
  }

  async pruneContent(options: PruneOptions): Promise<PruneResult> {
    const dir = contentDir();
    let entries: string[];
    try {
      entries = await readdir(dir);
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') {
        return { filesDeleted: 0, bytesFreed: 0, skippedRecoverable: 0 };
      }
      throw err;
    }
    const cutoff = Date.now() - options.olderThanMs;
    const isRecoverable = options.isRecoverable;
    let filesDeleted = 0;
    let bytesFreed = 0;
    let skippedRecoverable = 0;
    for (const name of entries) {
      if (!name.endsWith('.jsonl')) continue;
      const sessionId = name.slice(0, -'.jsonl'.length);
      if (!isValidSessionId(sessionId)) continue;
      const full = path.join(dir, name);
      // Acquire the same per-session lock used by appendSessionContent so a
      // prune cannot race with an in-flight write for this session. We re-stat
      // inside the lock to ensure we're deciding on the post-write mtime.
      type Outcome =
        | { kind: 'deleted'; size: number }
        | { kind: 'skippedRecoverable' }
        | null;
      const outcome: Outcome = await this.withLock(`content.${sessionId}`, async () => {
        let st: Awaited<ReturnType<typeof stat>>;
        try {
          st = await stat(full);
        } catch {
          return null;
        }
        if (!st.isFile()) return null;
        // Inclusive cutoff: files whose mtime equals now - olderThanMs are
        // eligible. This also makes `pruneContent({olderThanMs: 0})` clear the
        // directory reliably.
        if (st.mtimeMs > cutoff) return null;
        // Source-aware protection: if the upstream agent's session file still
        // exists, the sidecar is recoverable via `burn state rebuild content`, so
        // deleting it on retention alone is silently lossy. Callers opt in by
        // supplying `isRecoverable`; the ledger package itself never reaches
        // out to adapter-specific paths.
        if (isRecoverable) {
          try {
            if (await isRecoverable(sessionId)) {
              return { kind: 'skippedRecoverable' };
            }
          } catch {
            // If the predicate throws, fall through to the existing behavior:
            // we'd rather prune than fail-open and accumulate forever on a
            // broken source index.
          }
        }
        try {
          await unlink(full);
          return { kind: 'deleted', size: st.size };
        } catch {
          // raced with another deleter or was already gone
          return null;
        }
      });
      if (outcome?.kind === 'deleted') {
        filesDeleted++;
        bytesFreed += outcome.size;
      } else if (outcome?.kind === 'skippedRecoverable') {
        skippedRecoverable++;
      }
    }
    return { filesDeleted, bytesFreed, skippedRecoverable };
  }

  private async appendLines(lines: LedgerLine[]): Promise<void> {
    if (lines.length === 0) return;
    const filePath = ledgerPath();
    await ensureDir(filePath);
    const payload = lines.map((l) => JSON.stringify(l)).join('\n') + '\n';
    // Hold the same 'ledger' lock that reclassifyLedger acquires for its
    // read-modify-write pass. Without this, an appendFile that lands between
    // reclassify's readFile and rename would write to the soon-orphaned old
    // inode, and its turns would be silently dropped when the rename swaps in
    // the rewritten file. The race is hard to force in tests on a fast SSD
    // (libuv tends to drain queued appendFiles before reclassify starts
    // reading) but is real under heavier contention.
    await this.withLock('ledger', async () => {
      await appendFile(filePath, payload, { encoding: 'utf8' });
    });
  }

  private async appendSessionContent(
    sessionId: string,
    records: ContentRecord[],
  ): Promise<void> {
    const file = contentFilePath(sessionId);
    await this.withLock(`content.${sessionId}`, async () => {
      const lines = records.map((r) => JSON.stringify(r));
      let existing = new Set<string>();
      try {
        const raw = await readFile(file, 'utf8');
        existing = new Set(raw.split('\n').filter((line) => line.length > 0));
      } catch (err) {
        if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
      }
      const fresh = lines.filter((line) => {
        if (existing.has(line)) return false;
        existing.add(line);
        return true;
      });
      if (fresh.length === 0) return;
      const payload = fresh.join('\n') + '\n';
      await appendFile(file, payload, { encoding: 'utf8' });
    });
  }
}

async function ensureDir(filePath: string): Promise<void> {
  await mkdir(path.dirname(filePath), { recursive: true });
}

async function fileExists(p: string): Promise<boolean> {
  try {
    const s = await stat(p);
    return s.isFile();
  } catch {
    return false;
  }
}

async function* streamLines(filePath: string): AsyncIterable<unknown> {
  const rl = createInterface({
    input: createReadStream(filePath, { encoding: 'utf8' }),
    crlfDelay: Infinity,
  });
  try {
    for await (const line of rl) {
      const t = line.trim();
      if (!t) continue;
      try {
        yield JSON.parse(t);
      } catch {
        // skip malformed
      }
    }
  } finally {
    rl.close();
  }
}

async function collectStamps(filePath: string): Promise<StampLine[]> {
  const stamps: StampLine[] = [];
  for await (const parsed of streamLines(filePath)) {
    if (isStampLine(parsed)) stamps.push(parsed);
  }
  stamps.sort((a, b) => a.ts.localeCompare(b.ts));
  return stamps;
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

function compactionPasses(e: CompactionEvent, q: Query): boolean {
  if (q.since && e.ts < q.since) return false;
  if (q.until && e.ts > q.until) return false;
  if (q.sessionId && e.sessionId !== q.sessionId) return false;
  if (q.source && e.source !== q.source) return false;
  // project and enrichment don't filter compaction events (they live at the
  // session level; callers that need project-scoping should pre-filter by
  // sessionId from a turn query).
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
  // project and enrichment don't filter user turns (they live at the session
  // level; callers that need project-scoping should pre-filter by sessionId
  // from a turn query).
  return true;
}

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
