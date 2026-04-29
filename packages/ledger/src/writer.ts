import type {
  CompactionEvent,
  SessionRelationshipRecord,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import { getAdapter } from './adapters/factory.js';
import type { Enrichment, StampLine, StampSelector } from './schema.js';

export async function appendTurns(turns: TurnRecord[]): Promise<void> {
  return getAdapter().appendTurns(turns);
}

export async function appendCompactions(events: CompactionEvent[]): Promise<void> {
  return getAdapter().appendCompactions(events);
}

export async function appendRelationships(
  records: SessionRelationshipRecord[],
): Promise<void> {
  return getAdapter().appendRelationships(records);
}

export async function appendUserTurns(records: UserTurnRecord[]): Promise<void> {
  return getAdapter().appendUserTurns(records);
}

export async function appendToolResultEvents(
  records: ToolResultEventRecord[],
): Promise<void> {
  return getAdapter().appendToolResultEvents(records);
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
  return getAdapter().appendStamp(line);
}
