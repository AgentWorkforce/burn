import type { CompactionEvent, TurnRecord } from '@relayburn/reader';

export type Enrichment = Record<string, string>;

export interface MessageIdRange {
  fromMessageId?: string;
  toMessageId?: string;
  fromTs?: string;
  toTs?: string;
}

export interface StampSelector {
  sessionId?: string;
  messageId?: string;
  range?: MessageIdRange;
}

export interface TurnLine {
  v: 1;
  kind: 'turn';
  record: TurnRecord;
}

export interface StampLine {
  v: 1;
  kind: 'stamp';
  ts: string;
  selector: StampSelector;
  enrichment: Enrichment;
}

export interface CompactionLine {
  v: 1;
  kind: 'compaction';
  record: CompactionEvent;
}

export type LedgerLine = TurnLine | StampLine | CompactionLine;

export function isTurnLine(line: unknown): line is TurnLine {
  return (
    !!line &&
    typeof line === 'object' &&
    (line as { kind?: string }).kind === 'turn' &&
    (line as { v?: number }).v === 1
  );
}

export function isStampLine(line: unknown): line is StampLine {
  return (
    !!line &&
    typeof line === 'object' &&
    (line as { kind?: string }).kind === 'stamp' &&
    (line as { v?: number }).v === 1
  );
}

export function isCompactionLine(line: unknown): line is CompactionLine {
  return (
    !!line &&
    typeof line === 'object' &&
    (line as { kind?: string }).kind === 'compaction' &&
    (line as { v?: number }).v === 1
  );
}

export function stampMatches(stamp: StampLine, turn: TurnRecord): boolean {
  const s = stamp.selector;
  if (s.sessionId !== undefined && s.sessionId !== turn.sessionId) return false;
  if (s.messageId !== undefined && s.messageId !== turn.messageId) return false;
  if (s.range) {
    if (s.range.fromTs && turn.ts < s.range.fromTs) return false;
    if (s.range.toTs && turn.ts > s.range.toTs) return false;
  }
  return (
    s.sessionId !== undefined || s.messageId !== undefined || s.range !== undefined
  );
}
