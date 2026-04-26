import type {
  CompactionEvent,
  SessionRelationshipRecord,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

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

// Execution graph (#42). Two new ledger line kinds, both append-only and
// keyed by (source, sessionId, …) the same way TurnLine is. Old readers
// that don't know about these kinds simply skip them — the existing
// `isTurnLine` / `isStampLine` / `isCompactionLine` guards already filter
// to only the kinds they understand.
export interface SessionRelationshipLine {
  v: 1;
  kind: 'relationship';
  record: SessionRelationshipRecord;
}

export interface ToolResultEventLine {
  v: 1;
  kind: 'tool_result_event';
  record: ToolResultEventRecord;
}

// Per-user-turn block info (#2 / #94). One line per user line in a session,
// carrying the byte/approx-token size of each tool_result and free-text block
// the user supplied. Append-only and dedup'd through the same ledger-id index
// as turns/compactions via `userTurnIdHash` keyed on (source, sessionId,
// userUuid). Old readers that don't recognize `kind: 'user_turn'` simply skip
// the line — the existing `isTurnLine` / `isStampLine` / etc. guards already
// filter to known kinds.
export interface UserTurnLine {
  v: 1;
  kind: 'user_turn';
  record: UserTurnRecord;
}

export type LedgerLine =
  | TurnLine
  | StampLine
  | CompactionLine
  | SessionRelationshipLine
  | ToolResultEventLine
  | UserTurnLine;

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

export function isSessionRelationshipLine(
  line: unknown,
): line is SessionRelationshipLine {
  return (
    !!line &&
    typeof line === 'object' &&
    (line as { kind?: string }).kind === 'relationship' &&
    (line as { v?: number }).v === 1
  );
}

export function isToolResultEventLine(
  line: unknown,
): line is ToolResultEventLine {
  return (
    !!line &&
    typeof line === 'object' &&
    (line as { kind?: string }).kind === 'tool_result_event' &&
    (line as { v?: number }).v === 1
  );
}

export function isUserTurnLine(line: unknown): line is UserTurnLine {
  return (
    !!line &&
    typeof line === 'object' &&
    (line as { kind?: string }).kind === 'user_turn' &&
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
