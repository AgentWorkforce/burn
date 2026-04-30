import type {
  CompactionEvent,
  ContentRecord,
  SessionRelationshipRecord,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import type { PruneOptions, PruneResult, ReadContentSelector } from '../content.js';
import type { EnrichedTurn, Query } from '../reader.js';
import type { StampLine } from '../schema.js';

export type StorageAdapterKind = 'file' | 'sqlite' | 'postgres' | 'http';

export type ContentLine = ContentRecord;

export interface StorageAdapter {
  readonly kind: StorageAdapterKind;

  appendTurns(turns: TurnRecord[]): Promise<void>;
  appendCompactions(events: CompactionEvent[]): Promise<void>;
  appendRelationships(records: SessionRelationshipRecord[]): Promise<void>;
  appendToolResultEvents(records: ToolResultEventRecord[]): Promise<void>;
  appendUserTurns(records: UserTurnRecord[]): Promise<void>;
  appendStamp(stamp: StampLine): Promise<void>;
  appendContent(records: ContentRecord[]): Promise<void>;

  queryTurns(q: Query): AsyncIterable<EnrichedTurn>;
  queryCompactions(q: Query): AsyncIterable<CompactionEvent>;
  queryRelationships(q: Query): AsyncIterable<SessionRelationshipRecord>;
  queryToolResultEvents(q: Query): AsyncIterable<ToolResultEventRecord>;
  queryUserTurns(q: Query): AsyncIterable<UserTurnRecord>;
  readContent(selector: ReadContentSelector): AsyncIterable<ContentLine>;
  listContentSessionIds(): Promise<string[]>;
  pruneContent(options: PruneOptions): Promise<PruneResult>;

  withLock<T>(name: string, fn: () => Promise<T>): Promise<T>;

  init(): Promise<void>;
  close(): Promise<void>;
}
