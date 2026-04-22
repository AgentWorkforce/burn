import { createReadStream } from 'node:fs';
import { stat } from 'node:fs/promises';
import { createInterface } from 'node:readline';

import type { SourceKind, TurnRecord } from '@relayburn/reader';

import { ledgerPath } from './paths.js';
import {
  isStampLine,
  isTurnLine,
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
