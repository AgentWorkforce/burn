import { appendFile, mkdir } from 'node:fs/promises';
import * as path from 'node:path';

import type {
  CompactionEvent,
  SessionRelationshipRecord,
  ToolResultEventRecord,
  TurnRecord,
} from '@relayburn/reader';

import {
  appendHashes,
  compactionIdHash,
  loadIndex,
  relationshipIdHash,
  toolResultEventIdHash,
  turnContentFingerprint,
  turnIdHash,
} from './index-sidecar.js';
import { withLock } from './lock.js';
import { ledgerPath } from './paths.js';
import type {
  CompactionLine,
  Enrichment,
  LedgerLine,
  SessionRelationshipLine,
  StampLine,
  StampSelector,
  ToolResultEventLine,
  TurnLine,
} from './schema.js';

async function ensureDir(filePath: string): Promise<void> {
  await mkdir(path.dirname(filePath), { recursive: true });
}

async function appendLines(lines: LedgerLine[]): Promise<void> {
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
  await withLock('ledger', async () => {
    await appendFile(filePath, payload, { encoding: 'utf8' });
  });
}

export async function appendTurns(turns: TurnRecord[]): Promise<void> {
  if (turns.length === 0) return;
  const idx = await loadIndex();
  // Snapshot content set before this batch — content-fingerprint dedup only
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
    idx.ids.add(id); // primary dedup DOES apply within batch — same messageId = same turn
  }
  if (fresh.length === 0) return;
  for (const cf of newContent) idx.content.add(cf);
  const lines: TurnLine[] = fresh.map((record) => ({ v: 1, kind: 'turn', record }));
  await appendLines(lines);
  await appendHashes(newIds, newContent);
}

export async function appendCompactions(events: CompactionEvent[]): Promise<void> {
  if (events.length === 0) return;
  // Dedup piggybacks on the ledger-id index. Compaction ids share the same
  // namespace as turn ids — they hash different inputs so collisions are not
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
  await appendLines(lines);
  await appendHashes(newIds, []);
}

export async function appendRelationships(
  records: SessionRelationshipRecord[],
): Promise<void> {
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
  await appendLines(lines);
  await appendHashes(newIds, []);
}

export async function appendToolResultEvents(
  records: ToolResultEventRecord[],
): Promise<void> {
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
  await appendLines(lines);
  await appendHashes(newIds, []);
}

export async function stamp(
  selector: StampSelector,
  enrichment: Enrichment,
): Promise<void> {
  if (
    selector.sessionId === undefined &&
    selector.messageId === undefined &&
    selector.range === undefined
  ) {
    throw new Error('stamp requires at least one selector field (sessionId, messageId, or range)');
  }
  const line: StampLine = {
    v: 1,
    kind: 'stamp',
    ts: new Date().toISOString(),
    selector,
    enrichment,
  };
  await appendLines([line]);
}
