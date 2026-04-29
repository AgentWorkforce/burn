import { classifyActivity } from './classifier.js';
import { makeFidelity, EMPTY_COVERAGE } from './fidelity.js';
import { argsHash, contentHash } from './hash.js';
import { resolveProject } from './git.js';
import type {
  ContentRecord,
  ContentStoreMode,
  Coverage,
  Fidelity,
  SessionRelationshipRecord,
  Subagent,
  ToolCall,
  ToolResultEventRecord,
  TurnRecord,
  Usage,
  UserTurnBlock,
  UserTurnRecord,
} from './types.js';
import {
  createUserTurnTokenCounter,
  makeTextBlock,
  makeToolResultBlock,
  type UserTurnTokenCounter,
  type UserTurnTokenizer,
} from './userTurn.js';

interface SessionInfo {
  id: string;
  parentID?: string;
  directory?: string;
}

interface MessageTokens {
  input?: number;
  output?: number;
  reasoning?: number;
  cache?: {
    read?: number;
    write?: number;
  };
}

interface AssistantMessage {
  id: string;
  sessionID: string;
  role: 'assistant';
  time: { created: number };
  providerID?: string;
  modelID?: string;
  path?: { cwd?: string };
  tokens?: MessageTokens;
}

interface UserMessage {
  id: string;
  sessionID: string;
  role: 'user';
  time: { created: number };
}

interface ToolPart {
  id?: string;
  sessionID?: string;
  messageID?: string;
  type: 'tool';
  callID?: string;
  tool?: string;
  state?: {
    input?: Record<string, unknown>;
    status?: string;
    metadata?: { exit?: number; [k: string]: unknown };
    output?: unknown;
    [k: string]: unknown;
  };
}

interface StepFinishPart {
  id?: string;
  sessionID?: string;
  messageID?: string;
  type: 'step-finish';
  reason?: string;
  tokens?: MessageTokens;
}

interface TextPart {
  id?: string;
  sessionID?: string;
  messageID?: string;
  type: 'text';
  text?: string;
  synthetic?: boolean;
}

type Part =
  | ToolPart
  | StepFinishPart
  | TextPart
  | { id?: string; sessionID?: string; messageID?: string; type: string; [k: string]: unknown };

export interface OpencodeStreamCursorState {
  lastEventId?: string;
  emittedMessageIds?: string[];
  emittedToolEventIds?: string[];
}

export interface OpencodeStreamIngestOptions {
  contentMode?: ContentStoreMode;
  tokenizer?: UserTurnTokenizer;
  cursor?: OpencodeStreamCursorState;
}

export interface OpencodeStreamIngestResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  relationships: SessionRelationshipRecord[];
  toolResultEvents: ToolResultEventRecord[];
  userTurns: UserTurnRecord[];
  cursor: OpencodeStreamCursorState;
}

export interface OpencodeStreamIngestor {
  ingest(payload: unknown, eventId?: string): Promise<OpencodeStreamIngestResult>;
  snapshotCursor(): OpencodeStreamCursorState;
}

interface NormalizedEvent {
  type: string;
  properties: Record<string, unknown>;
}

export async function createOpencodeStreamIngestor(
  options: OpencodeStreamIngestOptions = {},
): Promise<OpencodeStreamIngestor> {
  const tokenCounter = await createUserTurnTokenCounter(options.tokenizer);
  return new Ingestor(options, tokenCounter);
}

class Ingestor implements OpencodeStreamIngestor {
  private readonly contentMode: ContentStoreMode;
  private readonly tokenCounter: UserTurnTokenCounter;
  private readonly sessions = new Map<string, SessionInfo>();
  private readonly streamOwnedSessions = new Set<string>();
  private readonly messages = new Map<string, AssistantMessage | UserMessage>();
  private readonly partsByMessage = new Map<string, Map<string, Part>>();
  private readonly emittedMessageIds: Set<string>;
  private readonly emittedToolEventIds: Set<string>;
  private lastEventId: string | undefined;

  constructor(options: OpencodeStreamIngestOptions, tokenCounter: UserTurnTokenCounter) {
    this.contentMode = options.contentMode ?? 'full';
    this.tokenCounter = tokenCounter;
    this.emittedMessageIds = new Set(options.cursor?.emittedMessageIds ?? []);
    this.emittedToolEventIds = new Set(options.cursor?.emittedToolEventIds ?? []);
    this.lastEventId = options.cursor?.lastEventId;
  }

  async ingest(payload: unknown, eventId?: string): Promise<OpencodeStreamIngestResult> {
    if (eventId !== undefined && eventId.length > 0) this.lastEventId = eventId;
    const ev = normalizeEvent(payload);
    const flush = new Set<string>();
    if (ev) {
      switch (ev.type) {
        case 'session.created':
          this.updateSession(ev.properties);
          {
            const id = sessionIdFromInfo(ev.properties['info']);
            if (id) this.streamOwnedSessions.add(id);
          }
          break;
        case 'session.updated':
          this.updateSession(ev.properties);
          break;
        case 'session.deleted':
          {
            const id = sessionIdFromInfo(ev.properties['info']) ?? stringProp(ev.properties, 'sessionID');
            if (id) this.dropSession(id);
          }
          break;
        case 'message.updated':
          this.updateMessage(ev.properties);
          break;
        case 'message.part.updated':
          this.updatePart(ev.properties);
          break;
        case 'message.part.removed':
          this.removePart(ev.properties);
          break;
        case 'session.idle':
          {
            const id = stringProp(ev.properties, 'sessionID');
            if (id) flush.add(id);
          }
          break;
        case 'session.status':
          if (isIdleStatus(ev.properties['status'])) {
            const id = stringProp(ev.properties, 'sessionID');
            if (id) flush.add(id);
          }
          break;
      }
    }
    return this.flush(flush);
  }

  snapshotCursor(): OpencodeStreamCursorState {
    const cursor: OpencodeStreamCursorState = {
      emittedMessageIds: [...this.emittedMessageIds],
      emittedToolEventIds: [...this.emittedToolEventIds],
    };
    if (this.lastEventId !== undefined) cursor.lastEventId = this.lastEventId;
    return cursor;
  }

  private updateSession(properties: Record<string, unknown>): void {
    const raw = properties['info'];
    if (!raw || typeof raw !== 'object') return;
    const rec = raw as Record<string, unknown>;
    if (typeof rec.id !== 'string') return;
    const session: SessionInfo = { id: rec.id };
    if (typeof rec.parentID === 'string') session.parentID = rec.parentID;
    if (typeof rec.directory === 'string') session.directory = rec.directory;
    this.sessions.set(session.id, session);
  }

  private updateMessage(properties: Record<string, unknown>): void {
    const raw = properties['info'];
    if (!raw || typeof raw !== 'object') return;
    const rec = raw as Record<string, unknown>;
    if (isCompleteAssistantLike(rec)) {
      if (!this.streamOwnedSessions.has(rec.sessionID)) return;
      this.messages.set(rec.id, rec as unknown as AssistantMessage);
    } else if (isCompleteUserLike(rec)) {
      if (!this.streamOwnedSessions.has(rec.sessionID)) return;
      this.messages.set(rec.id, rec as unknown as UserMessage);
    }
  }

  private updatePart(properties: Record<string, unknown>): void {
    const raw = properties['part'];
    if (!raw || typeof raw !== 'object') return;
    const part = raw as Part;
    if (typeof part.sessionID !== 'string' || typeof part.messageID !== 'string') return;
    if (!this.streamOwnedSessions.has(part.sessionID)) return;
    const id = typeof part.id === 'string' && part.id.length > 0 ? part.id : partKey(part);
    let bucket = this.partsByMessage.get(part.messageID);
    if (!bucket) {
      bucket = new Map();
      this.partsByMessage.set(part.messageID, bucket);
    }
    bucket.set(id, part);
  }

  private removePart(properties: Record<string, unknown>): void {
    const messageID = stringProp(properties, 'messageID');
    const partID = stringProp(properties, 'partID');
    const sessionID = stringProp(properties, 'sessionID');
    if (!messageID || !partID || !sessionID) return;
    if (!this.streamOwnedSessions.has(sessionID)) return;
    this.partsByMessage.get(messageID)?.delete(partID);
  }

  private dropSession(sessionId: string): void {
    this.sessions.delete(sessionId);
    this.streamOwnedSessions.delete(sessionId);
    for (const [id, message] of this.messages.entries()) {
      if (message.sessionID === sessionId) this.messages.delete(id);
    }
  }

  private async flush(sessionIds: Set<string>): Promise<OpencodeStreamIngestResult> {
    const turns: TurnRecord[] = [];
    const content: ContentRecord[] = [];
    const relationships: SessionRelationshipRecord[] = [];
    const toolResultEvents: ToolResultEventRecord[] = [];
    const userTurns: UserTurnRecord[] = [];

    for (const sessionId of sessionIds) {
      if (!this.streamOwnedSessions.has(sessionId)) continue;
      const session = this.sessions.get(sessionId) ?? { id: sessionId };
      const assistants = [...this.messages.values()]
        .filter((m): m is AssistantMessage => m.role === 'assistant' && m.sessionID === sessionId)
        .sort((a, b) => a.time.created - b.time.created);
      const users = [...this.messages.values()]
        .filter((m): m is UserMessage => m.role === 'user' && m.sessionID === sessionId)
        .sort((a, b) => a.time.created - b.time.created);
      relationships.push(...buildRelationships(session, assistants));

      const allToolEvents = collectToolResultEventsForSession(sessionId, assistants, (messageId) =>
        this.partsFor(messageId),
      );
      for (const ev of allToolEvents) {
        const key = toolEventKey(ev);
        if (this.emittedToolEventIds.has(key)) continue;
        this.emittedToolEventIds.add(key);
        toolResultEvents.push(ev);
      }

      for (let i = 0; i < assistants.length; i++) {
        const m = assistants[i]!;
        if (this.emittedMessageIds.has(m.id)) continue;
        const parts = this.partsFor(m.id);
        if (!isFinalAssistant(m, parts)) continue;
        const prev = i > 0 ? assistants[i - 1]! : undefined;
        const userMsg = findPrecedingUser(users, m.time.created);
        const userMsgForGap =
          userMsg && (!prev || userMsg.time.created > prev.time.created) ? userMsg : undefined;
        const userTurn = buildUserTurnRecord(
          sessionId,
          prev,
          m,
          userMsgForGap,
          prev ? this.partsFor(prev.id) : [],
          userMsgForGap ? this.partsFor(userMsgForGap.id) : [],
          this.tokenCounter,
        );
        if (userTurn) userTurns.push(userTurn);

        const { toolCalls, filesTouched, erroredCallIds } = extractToolsAndFiles(parts);
        const usage = toUsage(m.tokens);
        const record = buildTurnRecord(
          session,
          m,
          i,
          parts,
          toolCalls,
          filesTouched,
          erroredCallIds,
          usage,
          userMsg ? extractTextParts(this.partsFor(userMsg.id), { includeSynthetic: false }).join('\n') : '',
        );
        turns.push(record);
        this.emittedMessageIds.add(m.id);
        if (this.contentMode === 'full') {
          if (userMsg) {
            const userTs = new Date(userMsg.time.created).toISOString();
            for (const t of extractTextParts(this.partsFor(userMsg.id), { includeSynthetic: false })) {
              content.push({
                v: 1,
                source: 'opencode',
                sessionId,
                messageId: userMsg.id,
                ts: userTs,
                role: 'user',
                kind: 'text',
                text: t,
              });
            }
          }
          content.push(...extractAssistantContent(parts, sessionId, m.id, record.ts));
        }
      }
    }

    return {
      turns,
      content,
      relationships,
      toolResultEvents,
      userTurns,
      cursor: this.snapshotCursor(),
    };
  }

  private partsFor(messageId: string): Part[] {
    const bucket = this.partsByMessage.get(messageId);
    if (!bucket) return [];
    return [...bucket.values()].sort((a, b) =>
      (a.id ?? '') < (b.id ?? '') ? -1 : (a.id ?? '') > (b.id ?? '') ? 1 : 0,
    );
  }
}

function normalizeEvent(payload: unknown): NormalizedEvent | null {
  if (!payload || typeof payload !== 'object') return null;
  const rec = payload as Record<string, unknown>;
  const nested = rec['payload'];
  if (nested && typeof nested === 'object') return normalizeEvent(nested);
  const type = rec['type'];
  if (typeof type !== 'string') return null;
  const properties = rec['properties'];
  return {
    type,
    properties:
      properties && typeof properties === 'object'
        ? (properties as Record<string, unknown>)
        : {},
  };
}

function sessionIdFromInfo(raw: unknown): string | undefined {
  if (!raw || typeof raw !== 'object') return undefined;
  const id = (raw as Record<string, unknown>)['id'];
  return typeof id === 'string' ? id : undefined;
}

function stringProp(rec: Record<string, unknown>, key: string): string | undefined {
  const value = rec[key];
  return typeof value === 'string' && value.length > 0 ? value : undefined;
}

function isIdleStatus(raw: unknown): boolean {
  if (raw === 'idle') return true;
  if (!raw || typeof raw !== 'object') return false;
  return (raw as Record<string, unknown>)['type'] === 'idle';
}

function isCompleteAssistantLike(rec: Record<string, unknown>): rec is Record<string, unknown> & AssistantMessage {
  return (
    rec['role'] === 'assistant' &&
    typeof rec['id'] === 'string' &&
    typeof rec['sessionID'] === 'string' &&
    typeof (rec['time'] as { created?: unknown } | undefined)?.created === 'number'
  );
}

function isCompleteUserLike(rec: Record<string, unknown>): rec is Record<string, unknown> & UserMessage {
  return (
    rec['role'] === 'user' &&
    typeof rec['id'] === 'string' &&
    typeof rec['sessionID'] === 'string' &&
    typeof (rec['time'] as { created?: unknown } | undefined)?.created === 'number'
  );
}

function partKey(part: Part): string {
  return `${part.messageID ?? ''}:${part.type}:${JSON.stringify(part).length}`;
}

function isFinalAssistant(m: AssistantMessage, parts: Part[]): boolean {
  return m.tokens !== undefined || parts.some((p) => p.type === 'step-finish');
}

function buildRelationships(
  session: SessionInfo,
  assistants: AssistantMessage[],
): SessionRelationshipRecord[] {
  const firstTs =
    assistants.length > 0 ? new Date(assistants[0]!.time.created).toISOString() : undefined;
  const root: SessionRelationshipRecord = {
    v: 1,
    source: 'opencode',
    sessionId: session.id,
    relationshipType: 'root',
  };
  if (firstTs !== undefined) root.ts = firstTs;
  const out = [root];
  if (typeof session.parentID === 'string' && session.parentID.length > 0) {
    const sub: SessionRelationshipRecord = {
      v: 1,
      source: 'native-opencode',
      sessionId: session.id,
      relatedSessionId: session.parentID,
      relationshipType: 'subagent',
    };
    if (firstTs !== undefined) sub.ts = firstTs;
    out.push(sub);
  }
  return out;
}

function collectToolResultEventsForSession(
  sessionId: string,
  assistants: AssistantMessage[],
  partsFor: (messageId: string) => Part[],
): ToolResultEventRecord[] {
  const out: ToolResultEventRecord[] = [];
  const callIndexCounters = new Map<string, number>();
  let eventIndex = 0;
  for (const m of assistants) {
    const parts = partsFor(m.id);
    if (!isFinalAssistant(m, parts)) continue;
    const terminalTools = parts.filter(isTerminalToolPart);
    const turnUsage = toUsage(m.tokens);
    const usageShare =
      terminalTools.length > 0 ? divideUsage(turnUsage, terminalTools.length) : undefined;
    const usageAttribution =
      terminalTools.length === 1
        ? 'single-tool-turn'
        : terminalTools.length > 1
          ? 'even-split-turn'
          : undefined;
    for (const tp of terminalTools) {
      const state = tp.state!;
      const isError = isFailedTool(tp);
      const callIndex = callIndexCounters.get(tp.callID) ?? 0;
      callIndexCounters.set(tp.callID, callIndex + 1);
      const record: ToolResultEventRecord = {
        v: 1,
        source: 'opencode',
        sessionId,
        messageId: m.id,
        toolUseId: tp.callID,
        callIndex,
        eventIndex: eventIndex++,
        ts: new Date(m.time.created).toISOString(),
        status: isError ? 'errored' : 'completed',
        eventSource: 'tool_result',
      };
      if (isError) record.isError = true;
      if (usageShare !== undefined) {
        record.usage = usageShare;
        if (usageAttribution !== undefined) record.usageAttribution = usageAttribution;
      }
      const measured = measureToolOutput(state.output);
      if (measured.length !== undefined) record.contentLength = measured.length;
      if (measured.hash !== undefined) record.contentHash = measured.hash;
      out.push(record);
    }
  }
  return out;
}

function toolEventKey(ev: ToolResultEventRecord): string {
  return `${ev.sessionId}|${ev.toolUseId}|${ev.eventIndex}`;
}

function buildTurnRecord(
  session: SessionInfo,
  m: AssistantMessage,
  turnIndex: number,
  parts: Part[],
  toolCalls: ToolCall[],
  filesTouched: string[],
  erroredCallIds: Set<string>,
  usage: Usage,
  userText: string,
): TurnRecord {
  const model = buildModel(m.providerID, m.modelID);
  const project = m.path?.cwd ?? session.directory;
  let usageCoverage = coverageFromTokens(m.tokens);
  for (const sf of stepFinishTokens(parts)) {
    usageCoverage = mergeUsageCoverage(usageCoverage, coverageFromTokens(sf));
  }
  const record: TurnRecord = {
    v: 1,
    source: 'opencode',
    sessionId: m.sessionID,
    messageId: m.id,
    turnIndex,
    ts: new Date(m.time.created).toISOString(),
    model,
    usage,
    toolCalls,
    fidelity: buildFidelity(usageCoverage),
  };
  if (project !== undefined) {
    const resolved = resolveProject(project);
    record.project = resolved.project;
    if (resolved.projectKey !== undefined) record.projectKey = resolved.projectKey;
  }
  if (filesTouched.length > 0) record.filesTouched = filesTouched;
  if (session.parentID) {
    const sub: Subagent = { isSidechain: true };
    record.subagent = sub;
  }
  const stopReason = lastStepFinishReason(parts);
  if (stopReason !== undefined) record.stopReason = stopReason;
  const assistantText = extractAssistantText(parts);
  const hasFailedTool = toolCalls.some((tc) => erroredCallIds.has(tc.id));
  const classified = classifyActivity({
    toolCalls,
    text: [userText, assistantText].filter((s) => s.length > 0).join('\n'),
    hasFailedTool,
    reasoningTokens: usage.reasoning,
  });
  record.activity = classified.activity;
  record.retries = classified.retries;
  record.hasEdits = classified.hasEdits;
  return record;
}

function buildUserTurnRecord(
  sessionId: string,
  prev: AssistantMessage | undefined,
  next: AssistantMessage,
  userMsg: UserMessage | undefined,
  prevParts: Part[],
  userParts: Part[],
  tokenCounter: UserTurnTokenCounter,
): UserTurnRecord | undefined {
  const blocks: UserTurnBlock[] = [];
  if (prev) {
    for (const p of prevParts) {
      if (!isTerminalToolPart(p)) continue;
      const isError = isFailedTool(p);
      blocks.push(makeToolResultBlock(p.callID, p.state?.output ?? '', isError, tokenCounter));
    }
  }
  let ts = userMsg ? new Date(userMsg.time.created).toISOString() : '';
  for (const text of extractTextParts(userParts, { includeSynthetic: true })) {
    blocks.push(makeTextBlock(text, tokenCounter));
  }
  if (blocks.length === 0) return undefined;
  if (!ts) ts = new Date(next.time.created).toISOString();
  const userUuid = userMsg ? userMsg.id : `${sessionId}:${prev?.id ?? 'start'}->${next.id}`;
  const record: UserTurnRecord = {
    v: 1,
    source: 'opencode',
    sessionId,
    userUuid,
    ts,
    blocks,
    followingMessageId: next.id,
  };
  if (prev) record.precedingMessageId = prev.id;
  return record;
}

function extractToolsAndFiles(parts: Part[]): {
  toolCalls: ToolCall[];
  filesTouched: string[];
  erroredCallIds: Set<string>;
} {
  const toolCalls: ToolCall[] = [];
  const seen = new Set<string>();
  const files = new Set<string>();
  const erroredCallIds = new Set<string>();
  for (const p of parts) {
    if (p.type !== 'tool') continue;
    const tp = p as ToolPart;
    if (typeof tp.callID !== 'string' || typeof tp.tool !== 'string') continue;
    if (seen.has(tp.callID)) continue;
    seen.add(tp.callID);
    const input = tp.state?.input ?? {};
    const call: ToolCall = {
      id: tp.callID,
      name: tp.tool,
      argsHash: argsHash(input),
    };
    const target = pickTarget(tp.tool, input);
    if (target !== undefined) call.target = target;
    if (tp.tool === 'skill') {
      const skillName = input.skill ?? input.name ?? input.skill_name;
      if (typeof skillName === 'string') call.skillName = skillName;
    }
    toolCalls.push(call);
    if (target !== undefined && isFileTool(tp.tool)) files.add(target);
    if (isFailedTool(tp)) erroredCallIds.add(tp.callID);
  }
  return { toolCalls, filesTouched: [...files], erroredCallIds };
}

function extractAssistantContent(
  parts: Part[],
  sessionId: string,
  messageId: string,
  ts: string,
): ContentRecord[] {
  const out: ContentRecord[] = [];
  for (const p of parts) {
    if (p.type === 'text') {
      const tp = p as TextPart;
      if (tp.synthetic === true) continue;
      if (typeof tp.text === 'string' && tp.text.length > 0) {
        out.push({
          v: 1,
          source: 'opencode',
          sessionId,
          messageId,
          ts,
          role: 'assistant',
          kind: 'text',
          text: tp.text,
        });
      }
      continue;
    }
    if (p.type === 'tool') {
      const tp = p as ToolPart;
      if (typeof tp.callID !== 'string' || typeof tp.tool !== 'string') continue;
      const input = tp.state?.input ?? {};
      out.push({
        v: 1,
        source: 'opencode',
        sessionId,
        messageId,
        ts,
        role: 'assistant',
        kind: 'tool_use',
        toolUse: { id: tp.callID, name: tp.tool, input },
      });
      const state = tp.state;
      if (state && Object.prototype.hasOwnProperty.call(state, 'output')) {
        const result: ContentRecord = {
          v: 1,
          source: 'opencode',
          sessionId,
          messageId,
          ts,
          role: 'tool_result',
          kind: 'tool_result',
          toolResult: { toolUseId: tp.callID, content: state.output ?? '' },
        };
        if (isFailedTool(tp)) result.toolResult!.isError = true;
        out.push(result);
      }
    }
  }
  return out;
}

function extractTextParts(parts: Part[], opts: { includeSynthetic: boolean }): string[] {
  const out: string[] = [];
  for (const p of parts) {
    if (p.type !== 'text') continue;
    const tp = p as TextPart;
    if (!opts.includeSynthetic && tp.synthetic === true) continue;
    if (typeof tp.text === 'string' && tp.text.length > 0) out.push(tp.text);
  }
  return out;
}

function isTerminalToolPart(p: Part): p is ToolPart & { callID: string } {
  if (p.type !== 'tool') return false;
  const tp = p as ToolPart;
  if (typeof tp.callID !== 'string' || tp.callID.length === 0) return false;
  const state = tp.state;
  return !!state && Object.prototype.hasOwnProperty.call(state, 'output');
}

function isFailedTool(tp: ToolPart): boolean {
  const state = tp.state;
  if (!state) return false;
  if (state.status === 'error') return true;
  const exit = state.metadata?.exit;
  if (typeof exit === 'number' && exit !== 0) return true;
  return false;
}

function extractAssistantText(parts: Part[]): string {
  return extractTextParts(parts, { includeSynthetic: false }).join('\n');
}

function findPrecedingUser(users: UserMessage[], tsCreated: number): UserMessage | undefined {
  let best: UserMessage | undefined;
  for (const u of users) {
    if (u.time.created <= tsCreated) best = u;
    else break;
  }
  return best;
}

function pickTarget(name: string, input: Record<string, unknown>): string | undefined {
  const s = (k: string): string | undefined => {
    const v = input[k];
    return typeof v === 'string' ? v : undefined;
  };
  switch (name) {
    case 'read':
    case 'write':
    case 'edit':
      return s('filePath') ?? s('file_path') ?? s('path');
    case 'bash':
      return s('command');
    case 'grep':
    case 'glob':
      return s('pattern');
    case 'webfetch':
      return s('url');
    case 'task':
      return s('subagent_type') ?? s('description') ?? s('prompt');
    default:
      return s('filePath') ?? s('file_path') ?? s('path') ?? s('url') ?? s('command');
  }
}

function isFileTool(name: string): boolean {
  return name === 'read' || name === 'write' || name === 'edit';
}

function lastStepFinishReason(parts: Part[]): string | undefined {
  for (let i = parts.length - 1; i >= 0; i--) {
    const p = parts[i]!;
    if (p.type === 'step-finish') {
      const sf = p as StepFinishPart;
      if (typeof sf.reason === 'string') return sf.reason;
    }
  }
  return undefined;
}

function stepFinishTokens(parts: Part[]): MessageTokens[] {
  const out: MessageTokens[] = [];
  for (const p of parts) {
    if (p.type !== 'step-finish') continue;
    const sf = p as StepFinishPart;
    if (sf.tokens) out.push(sf.tokens);
  }
  return out;
}

function buildModel(providerID: string | undefined, modelID: string | undefined): string {
  if (providerID && modelID) return `${providerID}/${modelID}`;
  return modelID ?? providerID ?? '';
}

function toUsage(t: MessageTokens | undefined): Usage {
  const input = t?.input ?? 0;
  const output = t?.output ?? 0;
  const reasoning = t?.reasoning ?? 0;
  const cacheRead = t?.cache?.read ?? 0;
  const cacheWrite = t?.cache?.write ?? 0;
  return {
    input,
    output,
    reasoning,
    cacheRead,
    cacheCreate5m: cacheWrite,
    cacheCreate1h: 0,
  };
}

function divideUsage(usage: Usage, divisor: number): Usage {
  if (divisor <= 1) return { ...usage };
  return {
    input: usage.input / divisor,
    output: usage.output / divisor,
    reasoning: usage.reasoning / divisor,
    cacheRead: usage.cacheRead / divisor,
    cacheCreate5m: usage.cacheCreate5m / divisor,
    cacheCreate1h: usage.cacheCreate1h / divisor,
  };
}

type OpencodeUsageCoverage = Pick<
  Coverage,
  | 'hasInputTokens'
  | 'hasOutputTokens'
  | 'hasReasoningTokens'
  | 'hasCacheReadTokens'
  | 'hasCacheCreateTokens'
>;

function coverageFromTokens(t: MessageTokens | undefined): OpencodeUsageCoverage {
  return {
    hasInputTokens: t?.input !== undefined,
    hasOutputTokens: t?.output !== undefined,
    hasReasoningTokens: t?.reasoning !== undefined,
    hasCacheReadTokens: t?.cache?.read !== undefined,
    hasCacheCreateTokens: t?.cache?.write !== undefined,
  };
}

function mergeUsageCoverage(
  a: OpencodeUsageCoverage,
  b: OpencodeUsageCoverage,
): OpencodeUsageCoverage {
  return {
    hasInputTokens: a.hasInputTokens || b.hasInputTokens,
    hasOutputTokens: a.hasOutputTokens || b.hasOutputTokens,
    hasReasoningTokens: a.hasReasoningTokens || b.hasReasoningTokens,
    hasCacheReadTokens: a.hasCacheReadTokens || b.hasCacheReadTokens,
    hasCacheCreateTokens: a.hasCacheCreateTokens || b.hasCacheCreateTokens,
  };
}

function buildFidelity(usageCoverage: OpencodeUsageCoverage): Fidelity {
  const coverage: Coverage = {
    ...EMPTY_COVERAGE,
    ...usageCoverage,
    hasToolCalls: true,
    hasToolResultEvents: true,
    hasSessionRelationships: true,
    hasRawContent: true,
  };
  return makeFidelity('per-turn', coverage);
}

function measureToolOutput(output: unknown): { length?: number; hash?: string } {
  if (typeof output === 'string') return { length: output.length, hash: contentHash(output) };
  if (output === undefined || output === null) return {};
  try {
    const serialized = JSON.stringify(output);
    if (typeof serialized !== 'string') return {};
    return { length: serialized.length, hash: contentHash(serialized) };
  } catch {
    return {};
  }
}
