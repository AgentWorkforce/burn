import { createReadStream } from 'node:fs';
import { stat } from 'node:fs/promises';
import { createInterface } from 'node:readline';

import type {
  CompactionEvent,
  SessionRelationshipRecord,
  SourceKind,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import { ledgerPath } from './paths.js';
import {
  isCompactionLine,
  isSessionRelationshipLine,
  isStampLine,
  isToolResultEventLine,
  isTurnLine,
  isUserTurnLine,
  stampMatches,
  type Enrichment,
  type StampLine,
} from './schema.js';

export interface Query {
  since?: string;
  until?: string;
  project?: string;
  sessionId?: string;
  source?: SourceKind;
  enrichment?: Partial<Record<string, string>>;
}

export interface EnrichedTurn extends TurnRecord {
  enrichment: Enrichment;
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

export async function* query(q: Query = {}): AsyncIterable<EnrichedTurn> {
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

export async function queryAll(q: Query = {}): Promise<EnrichedTurn[]> {
  const out: EnrichedTurn[] = [];
  for await (const t of query(q)) out.push(t);
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

export async function queryCompactions(q: Query = {}): Promise<CompactionEvent[]> {
  const filePath = ledgerPath();
  if (!(await fileExists(filePath))) return [];
  const out: CompactionEvent[] = [];
  for await (const parsed of streamLines(filePath)) {
    if (!isCompactionLine(parsed)) continue;
    if (!compactionPasses(parsed.record, q)) continue;
    out.push(parsed.record);
  }
  return out;
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

export async function queryRelationships(
  q: Query = {},
): Promise<SessionRelationshipRecord[]> {
  const filePath = ledgerPath();
  if (!(await fileExists(filePath))) return [];
  const out: SessionRelationshipRecord[] = [];
  for await (const parsed of streamLines(filePath)) {
    if (!isSessionRelationshipLine(parsed)) continue;
    if (!relationshipPasses(parsed.record, q)) continue;
    out.push(parsed.record);
  }
  return out;
}

export async function queryToolResultEvents(
  q: Query = {},
): Promise<ToolResultEventRecord[]> {
  const filePath = ledgerPath();
  if (!(await fileExists(filePath))) return [];
  const out: ToolResultEventRecord[] = [];
  for await (const parsed of streamLines(filePath)) {
    if (!isToolResultEventLine(parsed)) continue;
    if (!toolResultEventPasses(parsed.record, q)) continue;
    out.push(parsed.record);
  }
  return out;
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

export async function queryUserTurns(q: Query = {}): Promise<UserTurnRecord[]> {
  const filePath = ledgerPath();
  if (!(await fileExists(filePath))) return [];
  const out: UserTurnRecord[] = [];
  for await (const parsed of streamLines(filePath)) {
    if (!isUserTurnLine(parsed)) continue;
    if (!userTurnPasses(parsed.record, q)) continue;
    out.push(parsed.record);
  }
  return out;
}
