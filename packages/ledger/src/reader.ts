import type {
  CompactionEvent,
  SessionRelationshipRecord,
  SourceKind,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import { getAdapter } from './adapters/factory.js';
import type { Enrichment } from './schema.js';

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

export async function* query(q: Query = {}): AsyncIterable<EnrichedTurn> {
  yield* getAdapter().queryTurns(q);
}

export async function queryAll(q: Query = {}): Promise<EnrichedTurn[]> {
  return collect(getAdapter().queryTurns(q));
}

export async function queryCompactions(q: Query = {}): Promise<CompactionEvent[]> {
  return collect(getAdapter().queryCompactions(q));
}

export async function queryRelationships(
  q: Query = {},
): Promise<SessionRelationshipRecord[]> {
  return collect(getAdapter().queryRelationships(q));
}

export async function queryToolResultEvents(
  q: Query = {},
): Promise<ToolResultEventRecord[]> {
  return collect(getAdapter().queryToolResultEvents(q));
}

export async function queryUserTurns(q: Query = {}): Promise<UserTurnRecord[]> {
  return collect(getAdapter().queryUserTurns(q));
}

async function collect<T>(items: AsyncIterable<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const item of items) out.push(item);
  return out;
}
