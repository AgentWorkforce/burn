import { createReadStream } from 'node:fs';
import { open } from 'node:fs/promises';
import { basename } from 'node:path';
import { createInterface } from 'node:readline';

import { classifyActivity } from './classifier.js';
import { EMPTY_COVERAGE, makeFidelity } from './fidelity.js';
import { resolveProject } from './git.js';
import { argsHash, contentHash } from './hash.js';
import {
  createUserTurnTokenCounter,
  makeTextBlock,
  makeToolResultBlock,
} from './userTurn.js';
import type { UserTurnTokenCounter, UserTurnTokenizer } from './userTurn.js';
import type {
  CompactionEvent,
  ContentRecord,
  ContentStoreMode,
  Coverage,
  Fidelity,
  SessionRelationshipRecord,
  Subagent,
  ToolCall,
  ToolResultEventRecord,
  ToolResultStatus,
  TurnRecord,
  Usage,
  UserTurnBlock,
  UserTurnRecord,
} from './types.js';

interface AssistantLine {
  type: 'assistant';
  message: {
    id: string;
    model?: string;
    content?: ContentBlock[];
    stop_reason?: string | null;
    usage?: ClaudeUsage;
  };
  sessionId?: string;
  timestamp?: string;
  cwd?: string;
  isSidechain?: boolean;
  uuid?: string;
  parentUuid?: string | null;
  // Free-form version tag from the upstream Claude Code build, populated when
  // present. Used by the relationship-evidence pass to fill `sourceVersion`.
  version?: string;
}

type ContentBlock =
  | { type: 'text'; text?: string }
  | { type: 'thinking'; thinking?: string }
  | {
      type: 'tool_use';
      id?: string;
      name?: string;
      input?: Record<string, unknown>;
    }
  | { type: string; [k: string]: unknown };

interface ClaudeUsage {
  input_tokens?: number;
  output_tokens?: number;
  cache_read_input_tokens?: number;
  cache_creation_input_tokens?: number;
  cache_creation?: {
    ephemeral_5m_input_tokens?: number;
    ephemeral_1h_input_tokens?: number;
  };
}

interface UserLine {
  type: 'user';
  message?: {
    role?: string;
    content?:
      | string
      | Array<
          | { type: 'tool_result'; tool_use_id?: string; content?: unknown; is_error?: boolean }
          | { type: string; [k: string]: unknown }
        >;
  };
  isSidechain?: boolean;
  sessionId?: string;
  timestamp?: string;
  cwd?: string;
  uuid?: string;
  parentUuid?: string | null;
  version?: string;
}

// Line-level registry used to walk `parentUuid` chains across the session when
// resolving subagent invocation roots. Only assistant lines that carry an
// Agent/Task tool_use block are tagged with `agentToolUse`; that's the signal
// that a child user line with sidechain=true is the root of a new invocation —
// but only when the child user line is *not* the tool_result coming back from
// that spawn (continuation within the same invocation).
interface LineNode {
  uuid: string;
  parentUuid?: string;
  kind: 'user' | 'assistant';
  isSidechain: boolean;
  agentToolUse?: {
    id: string;
    subagentType?: string;
    description?: string;
  };
  // tool_use ids for which this user line carries a tool_result block. Used
  // to distinguish a subagent spawn (child is the initial prompt) from a
  // parent-thread continuation after the subagent completed (child is the
  // tool_result for the Agent/Task call).
  toolResultIds?: Set<string>;
}

interface WorkingRecord {
  messageId: string;
  firstTs: string;
  model: string;
  sessionId: string;
  cwd?: string;
  isSidechain: boolean;
  usage: Usage;
  // Subset of `Coverage` derived from which usage fields the upstream message
  // actually carried (vs defaulted to 0 by `toUsage`). The remaining coverage
  // flags — tool calls, tool-result events, session relationships, raw
  // content — are filled in at finalize time once we know what the turn
  // contains. Kept partial here to keep `toUsage` purely about token data.
  usageCoverage: Pick<
    Coverage,
    'hasInputTokens'
    | 'hasOutputTokens'
    | 'hasCacheReadTokens'
    | 'hasCacheCreateTokens'
  >;
  blocks: ContentBlock[];
  stopReason?: string;
  // uuid of the first assistant line carrying this messageId; used as the
  // starting point when walking parentUuid chains to resolve subagent roots.
  firstAssistantUuid?: string;
  parentAssistantUuid?: string;
}

export interface ParseOptions {
  sessionPath?: string;
  contentMode?: ContentStoreMode;
  // Controls how UserTurnBlock.approxTokens is computed. The default uses
  // cl100k; callers can opt into the historical bytes/4 heuristic for a cheap
  // proportional signal.
  tokenizer?: UserTurnTokenizer;
  // The session id derived from the on-disk filename (e.g. the `.jsonl`
  // basename for Claude). When omitted but `sessionPath` is set, the parser
  // derives it from the basename. Used as the authoritative "file session id"
  // when comparing against in-log `sessionId` fields — a mismatch is the
  // evidence channel for fork / continuation classification.
  fileSessionId?: string;
}

// Per-file relationship evidence. Single-file parse passes can't see
// across session files, so they collect the signals that a subsequent
// `reconcileClaudeSessionRelationships` call needs to upgrade `root` rows to
// `fork` / `continuation` once it has visibility into the rest of the corpus.
//
// Strictly metadata-only: no raw content, no token counts. Cheap to keep in
// memory or thread through cursors.
export interface ClaudeRelationshipEvidence {
  // The on-disk session id for this file (the `.jsonl` basename for Claude),
  // when known. Reconciliation skips files without it.
  fileSessionId?: string;
  // The first wall-clock timestamp seen on a line that carried `sessionId`
  // for the file's session. Used to anchor any reconciled row's `ts` field.
  firstTs?: string;
  // Distinct in-log `sessionId` values observed in the file. When non-empty
  // and not equal to `fileSessionId`, the first foreign id is reported as
  // `sourceSessionId` on emitted relationship rows.
  inLogSessionIds: string[];
  // First non-empty `version` field observed on any line. Surfaced as
  // `sourceVersion` on emitted relationship rows.
  sourceVersion?: string;
  // The `parentUuid` carried by the very first non-sidechain *user* line in
  // the file. When this points to a uuid registered by a different file,
  // it is the strongest evidence of a continuation across files. Only the
  // first user line is considered: assistant lines reference earlier lines
  // *inside* the same file, so their parentUuid is uninformative.
  firstParentUuid?: string;
  // All in-file uuids (user + assistant) so cross-file reconciliation can
  // resolve `firstParentUuid` references back to a session id.
  seenUuids: string[];
  // True iff the file carried a `/resume` or `/continue` slash-command user
  // line. The local parse already emits a `continuation` row in that case;
  // reconciliation uses the same flag to skip a duplicate emit.
  hasResumeMarker: boolean;
  // The session id named by a `/resume <id>` marker, when one was carried
  // and parses as a non-empty token. Used as `relatedSessionId` on the
  // emitted continuation row.
  resumeTargetSessionId?: string;
  // Explicit per-line relationship targets. These are emitted directly by the
  // parser, but reconciliation also needs to know about them so it can avoid
  // adding a duplicate edge from cross-file inference.
  explicitContinuationTargetSessionIds?: string[];
  explicitForkTargetSessionIds?: string[];
}

export interface ParseResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  events: CompactionEvent[];
  // Normalized execution-graph metadata. `relationships` describes how
  // sessions relate (`root` + one row per discovered subagent invocation +
  // `continuation` rows when the file carries a `/resume` marker). Cross-file
  // fork / continuation rows are produced by `reconcileClaudeSessionRelationships`
  // given the per-file `evidence` from multiple parses.
  // `toolResultEvents` is the chronological tool_result stream keyed by
  // `toolUseId`.
  relationships: SessionRelationshipRecord[];
  toolResultEvents: ToolResultEventRecord[];
  // Per-user-turn block info between assistant turns. Ordered by appearance
  // in the session log; entries reference adjacent assistant turns by
  // `precedingMessageId` / `followingMessageId`.
  userTurns: UserTurnRecord[];
  // Per-file relationship evidence. Carries the signals needed by
  // `reconcileClaudeSessionRelationships` to upgrade `root` rows to `fork` /
  // `continuation` once cross-file knowledge is available. Always populated;
  // a single-file caller can ignore it.
  evidence: ClaudeRelationshipEvidence;
}

export async function parseClaudeSession(
  filePath: string,
  options: ParseOptions = {},
): Promise<ParseResult> {
  const contentMode = options.contentMode ?? 'off';
  const captureContent = contentMode === 'full';
  const tokenCounter = await createUserTurnTokenCounter(options.tokenizer);
  const working = new Map<string, WorkingRecord>();
  const order: string[] = [];
  const nodesByUuid = new Map<string, LineNode>();
  const invocationCache = new Map<string, InvocationInfo | null>();
  // Track content with a monotonic sequence tied to line-read order so user
  // and assistant records can be merged back into chronological order at the
  // end (one TurnRecord may span multiple lines; we key its assistant content
  // to the seq of its first appearance).
  const userPending: Array<{ seq: number; record: ContentRecord }> = [];
  const firstSeq = new Map<string, number>();
  const userTextByMessageId = new Map<string, string>();
  const erroredToolUseIds = new Set<string>();
  const replacementMetaByToolUseId = new Map<string, ReplacementMeta>();
  const events: CompactionEvent[] = [];
  // Per-user-turn block info between assistant turns. Captured in source order
  // so consumers can recover per-tool-call cost as a delta against the next
  // assistant turn's input/cacheRead numbers.
  const userTurns: UserTurnRecord[] = [];
  // The user-turn record that hasn't yet been linked to its following
  // assistant turn — set when we read a user line, cleared when the next
  // assistant turn with a new messageId arrives.
  let pendingUserTurn: UserTurnRecord | undefined;
  // Track the most recent completed assistant messageId so a compact_boundary
  // system record can be anchored to the turn right before it.
  let lastAssistantMessageId: string | undefined;
  let currentUserText = '';
  let seq = 0;
  // Execution graph. Tool-result chronology events are emitted as user
  // lines carrying `tool_result` blocks are read so chronology matches log
  // order even within a single user line carrying multiple results. The
  // counter is shared across the whole file for this parse pass.
  const toolResultEvents: ToolResultEventRecord[] = [];
  const toolResultCounters = new Map<string, number>(); // toolUseId -> next callIndex
  let nextEventIndex = 0;
  // Per-session relationship rows. Roots emit on the first line we see for a
  // sessionId; subagent rows are derived after working records are resolved.
  const relationships: SessionRelationshipRecord[] = [];
  const seenRootSessionIds = new Set<string>();
  const seenExplicitRelationshipIds = new Set<string>();
  // Relationship-evidence collector. Populated as lines are read and
  // surfaced in the parse result so a cross-file reconciliation pass can
  // upgrade `root` rows to `fork` / `continuation`.
  const fileSessionId = deriveFileSessionId(options);
  const evidence = newEvidence(fileSessionId);

  const rl = createInterface({
    input: createReadStream(filePath, { encoding: 'utf8' }),
    crlfDelay: Infinity,
  });

  try {
    for await (const line of rl) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      let parsed: unknown;
      try {
        parsed = JSON.parse(trimmed);
      } catch {
        continue;
      }
      if (!parsed || typeof parsed !== 'object') continue;
      const rec = parsed as Record<string, unknown>;

      if (rec.type === 'assistant') {
        const al = rec as unknown as AssistantLine;
        const mid = al.message?.id;
        // Link any pending user-turn record to this assistant turn — but only
        // the first time we see a new messageId, so multi-line assistant
        // messages don't shadow the link. Once linked, the user turn is
        // closed off.
        if (typeof mid === 'string' && pendingUserTurn && !working.has(mid)) {
          pendingUserTurn.followingMessageId = mid;
          pendingUserTurn = undefined;
        }
        if (captureContent && typeof mid === 'string' && !firstSeq.has(mid)) {
          firstSeq.set(mid, seq);
        }
        if (typeof mid === 'string' && !userTextByMessageId.has(mid)) {
          userTextByMessageId.set(mid, currentUserText);
        }
        if (typeof mid === 'string') lastAssistantMessageId = mid;
        if (typeof al.sessionId === 'string' && al.sessionId.length > 0) {
          recordRoot(relationships, seenRootSessionIds, al.sessionId, al.timestamp, fileSessionId);
          collectExplicitClaudeRelationships(
            rec,
            evidence,
            relationships,
            seenExplicitRelationshipIds,
            fileSessionId ?? al.sessionId,
            al.timestamp,
          );
        }
        recordEvidenceFromLine(evidence, al);
        ingestAssistant(al, working, order, nodesByUuid);
      } else if (rec.type === 'user') {
        const ul = rec as unknown as UserLine;
        registerUserNode(ul, nodesByUuid);
        const prompt = extractPlainUserText(ul);
        if (prompt) currentUserText = prompt;
        collectErroredToolUseIds(ul, erroredToolUseIds);
        collectReplacementMeta(ul, replacementMetaByToolUseId);
        if (typeof ul.sessionId === 'string' && ul.sessionId.length > 0) {
          recordRoot(relationships, seenRootSessionIds, ul.sessionId, ul.timestamp, fileSessionId);
          collectExplicitClaudeRelationships(
            rec,
            evidence,
            relationships,
            seenExplicitRelationshipIds,
            fileSessionId ?? ul.sessionId,
            ul.timestamp,
          );
        }
        recordEvidenceFromLine(evidence, ul);
        recordResumeMarker(evidence, ul);
        nextEventIndex = collectToolResultEvents(
          ul,
          toolResultEvents,
          toolResultCounters,
          nextEventIndex,
        );
        const userTurn = buildUserTurnRecord(ul, lastAssistantMessageId, tokenCounter);
        if (userTurn) {
          userTurns.push(userTurn);
          pendingUserTurn = userTurn;
        }
        if (captureContent) {
          for (const c of extractUserContent(ul)) userPending.push({ seq, record: c });
        }
      } else if (rec.type === 'system') {
        if (rec['subtype'] === 'compact_boundary') {
          const sl = rec as { sessionId?: string; timestamp?: string };
          const sessionId = sl.sessionId ?? '';
          const ts = sl.timestamp ?? '';
          if (sessionId) {
            const ev: CompactionEvent = {
              v: 1,
              source: 'claude-code',
              sessionId,
              ts,
            };
            if (lastAssistantMessageId) ev.precedingMessageId = lastAssistantMessageId;
            events.push(ev);
          }
        }
        const systemEvent = buildClaudeSystemToolResultEvent(
          rec,
          toolResultCounters,
          nextEventIndex,
        );
        if (systemEvent) {
          toolResultEvents.push(systemEvent);
          nextEventIndex++;
        }
      }
      seq++;
    }
  } finally {
    rl.close();
  }

  const turns: TurnRecord[] = [];
  const assistantPending: Array<{ seq: number; sub: number; record: ContentRecord }> = [];
  for (let i = 0; i < order.length; i++) {
    const id = order[i]!;
    const w = working.get(id);
    if (!w) continue;
    const toolCalls = extractToolCalls(w.blocks, erroredToolUseIds, replacementMetaByToolUseId);
    const filesTouched = extractFilesTouched(toolCalls);
    const subagent = resolveSubagent(w, nodesByUuid, invocationCache);

    const record: TurnRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: w.sessionId,
      messageId: w.messageId,
      turnIndex: i,
      ts: w.firstTs,
      model: w.model,
      usage: w.usage,
      toolCalls,
    };
    if (options.sessionPath !== undefined) record.sessionPath = options.sessionPath;
    if (w.cwd !== undefined) {
      const resolved = resolveProject(w.cwd);
      record.project = resolved.project;
      if (resolved.projectKey !== undefined) record.projectKey = resolved.projectKey;
    }
    if (filesTouched.length > 0) record.filesTouched = filesTouched;
    if (subagent) record.subagent = subagent;
    if (w.stopReason !== undefined) record.stopReason = w.stopReason;
    record.fidelity = buildClaudeFidelity(w.usageCoverage);
    applyClassification(record, w, userTextByMessageId, erroredToolUseIds);
    turns.push(record);

    if (captureContent) {
      const seqForMsg = firstSeq.get(w.messageId) ?? 0;
      extractAssistantContent(w).forEach((r, sub) => {
        // sub starts at 1 so user content at the same seq sorts before assistant
        assistantPending.push({ seq: seqForMsg, sub: sub + 1, record: r });
      });
    }
  }

  annotateCompactionEvents(events, turns);
  collectSubagentRelationships(turns, relationships);
  annotateSpawnEvents(toolResultEvents, turns);
  emitLocalContinuationFromResume(relationships, evidence);
  annotateRelationshipsWithEvidence(relationships, evidence);

  const content: ContentRecord[] = captureContent
    ? mergeContentByOrder(userPending, assistantPending)
    : [];
  return { turns, content, events, relationships, toolResultEvents, userTurns, evidence };
}

function mergeContentByOrder(
  userPending: Array<{ seq: number; record: ContentRecord }>,
  assistantPending: Array<{ seq: number; sub: number; record: ContentRecord }>,
): ContentRecord[] {
  const merged: Array<{ seq: number; sub: number; record: ContentRecord }> = [];
  for (const u of userPending) merged.push({ seq: u.seq, sub: 0, record: u.record });
  for (const a of assistantPending) merged.push(a);
  merged.sort((a, b) => a.seq - b.seq || a.sub - b.sub);
  return merged.map((m) => m.record);
}

function ingestAssistant(
  line: AssistantLine,
  working: Map<string, WorkingRecord>,
  order: string[],
  nodesByUuid?: Map<string, LineNode>,
): void {
  const msg = line.message;
  if (!msg || typeof msg.id !== 'string') return;
  const messageId = msg.id;

  let w = working.get(messageId);
  if (!w) {
    const initial = toUsage(msg.usage);
    w = {
      messageId,
      firstTs: line.timestamp ?? '',
      model: msg.model ?? '',
      sessionId: line.sessionId ?? '',
      isSidechain: line.isSidechain === true,
      usage: initial.usage,
      usageCoverage: initial.coverage,
      blocks: [],
    };
    if (line.cwd !== undefined) w.cwd = line.cwd;
    if (typeof line.uuid === 'string') w.firstAssistantUuid = line.uuid;
    if (line.parentUuid) w.parentAssistantUuid = line.parentUuid;
    working.set(messageId, w);
    order.push(messageId);
  } else {
    if (line.isSidechain === true) w.isSidechain = true;
    if (!w.model && msg.model) w.model = msg.model;
    // Merge coverage from continuation lines so a usage field that arrives on
    // a follow-up assistant line for the same messageId still flips its flag.
    if (msg.usage !== undefined) {
      const next = toUsage(msg.usage);
      w.usageCoverage = mergeUsageCoverage(w.usageCoverage, next.coverage);
    }
  }
  if (typeof msg.stop_reason === 'string') w.stopReason = msg.stop_reason;
  if (Array.isArray(msg.content)) {
    for (const block of msg.content) w.blocks.push(block);
  }
  registerAssistantNode(line, nodesByUuid);
}

function makeLineNode(
  line: { uuid?: string; parentUuid?: string | null; isSidechain?: boolean },
  kind: 'user' | 'assistant',
): LineNode | undefined {
  if (typeof line.uuid !== 'string') return undefined;
  const node: LineNode = {
    uuid: line.uuid,
    kind,
    isSidechain: line.isSidechain === true,
  };
  if (typeof line.parentUuid === 'string' && line.parentUuid.length > 0) {
    node.parentUuid = line.parentUuid;
  }
  return node;
}

function registerAssistantNode(
  line: AssistantLine,
  nodesByUuid?: Map<string, LineNode>,
): void {
  if (!nodesByUuid) return;
  const node = makeLineNode(line, 'assistant');
  if (!node) return;
  // Only the *first* Agent/Task tool_use in a line is captured — a single
  // assistant line typically carries a single content block, so this is
  // unambiguous in practice.
  if (Array.isArray(line.message?.content)) {
    for (const block of line.message!.content) {
      if (!block || typeof block !== 'object' || block.type !== 'tool_use') continue;
      const tu = block as { id?: string; name?: string; input?: Record<string, unknown> };
      if (tu.name !== 'Agent' && tu.name !== 'Task') continue;
      if (typeof tu.id !== 'string') continue;
      const input = tu.input ?? {};
      const subagentType = typeof input['subagent_type'] === 'string' ? (input['subagent_type'] as string) : undefined;
      const description = typeof input['description'] === 'string' ? (input['description'] as string) : undefined;
      const at: LineNode['agentToolUse'] = { id: tu.id };
      if (subagentType !== undefined) at.subagentType = subagentType;
      if (description !== undefined) at.description = description;
      node.agentToolUse = at;
      break;
    }
  }
  nodesByUuid.set(node.uuid, node);
}

function registerUserNode(
  line: UserLine,
  nodesByUuid?: Map<string, LineNode>,
): void {
  if (!nodesByUuid) return;
  const node = makeLineNode(line, 'user');
  if (!node) return;
  const body = line.message?.content;
  if (Array.isArray(body)) {
    for (const block of body) {
      if (!block || typeof block !== 'object' || block.type !== 'tool_result') continue;
      const tr = block as { tool_use_id?: string };
      if (typeof tr.tool_use_id !== 'string') continue;
      if (!node.toolResultIds) node.toolResultIds = new Set();
      node.toolResultIds.add(tr.tool_use_id);
    }
  }
  nodesByUuid.set(node.uuid, node);
}

// Light pre-scan of [0, endOffset) that registers LineNodes and execution
// graph counters only — no turn emission, no classification, no content
// capture. Used by the incremental parser so resuming ingest can still
// resolve parentUuid chains that reach back before the resume point. Returns
// the messageId of the last assistant line seen so a resumed pass can anchor
// `precedingMessageId` on user turns whose preceding assistant was already
// ingested in a prior pass. Also populates `evidence` with the
// relationship signals carried by the already-ingested prefix —
// `firstParentUuid` and `inLogSessionIds` are file invariants, so a resumed
// pass that reads only the tail must still see them.
async function prescanNodes(
  filePath: string,
  endOffset: number,
  nodesByUuid: Map<string, LineNode>,
  evidence: ClaudeRelationshipEvidence,
  toolResultCounters: Map<string, number>,
): Promise<{ lastAssistantMessageId?: string; nextEventIndex: number }> {
  if (endOffset <= 0) return { nextEventIndex: 0 };
  const handle = await open(filePath, 'r');
  let buf: Buffer;
  try {
    const st = await handle.stat();
    const length = Math.min(endOffset, st.size);
    if (length <= 0) return { nextEventIndex: 0 };
    buf = Buffer.allocUnsafe(length);
    await handle.read(buf, 0, length, 0);
  } finally {
    await handle.close();
  }
  let p = 0;
  let lastAssistantMessageId: string | undefined;
  let nextEventIndex = 0;
  while (p < buf.length) {
    const nlIdx = buf.indexOf(0x0a, p);
    if (nlIdx === -1) break;
    const text = buf.subarray(p, nlIdx).toString('utf8').trim();
    p = nlIdx + 1;
    if (!text) continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      continue;
    }
    if (!parsed || typeof parsed !== 'object') continue;
    const rec = parsed as Record<string, unknown>;
    if (rec.type === 'assistant') {
      const al = rec as unknown as AssistantLine;
      registerAssistantNode(al, nodesByUuid);
      recordEvidenceFromLine(evidence, al);
      recordExplicitRelationshipEvidence(evidence, rec);
      const mid = al.message?.id;
      if (typeof mid === 'string') lastAssistantMessageId = mid;
    } else if (rec.type === 'user') {
      const ul = rec as unknown as UserLine;
      registerUserNode(ul, nodesByUuid);
      recordEvidenceFromLine(evidence, ul);
      recordExplicitRelationshipEvidence(evidence, rec);
      recordResumeMarker(evidence, ul);
      const harvested: ToolResultEventRecord[] = [];
      nextEventIndex = collectToolResultEvents(
        ul,
        harvested,
        toolResultCounters,
        nextEventIndex,
      );
    } else if (rec.type === 'system') {
      const systemEvent = buildClaudeSystemToolResultEvent(
        rec,
        toolResultCounters,
        nextEventIndex,
      );
      if (systemEvent) nextEventIndex++;
    }
  }
  const out: { lastAssistantMessageId?: string; nextEventIndex: number } = {
    nextEventIndex,
  };
  if (lastAssistantMessageId !== undefined) out.lastAssistantMessageId = lastAssistantMessageId;
  return out;
}

function extractAssistantContent(w: WorkingRecord): ContentRecord[] {
  const out: ContentRecord[] = [];
  if (!w.sessionId || !w.messageId) return out;
  const ts = w.firstTs;
  for (const block of w.blocks) {
    if (!block || typeof block !== 'object') continue;
    if (block.type === 'text') {
      const b = block as { text?: string };
      if (typeof b.text === 'string' && b.text.length > 0) {
        out.push({
          v: 1,
          source: 'claude-code',
          sessionId: w.sessionId,
          messageId: w.messageId,
          ts,
          role: 'assistant',
          kind: 'text',
          text: b.text,
        });
      }
    } else if (block.type === 'thinking') {
      const b = block as { thinking?: string };
      if (typeof b.thinking === 'string' && b.thinking.length > 0) {
        out.push({
          v: 1,
          source: 'claude-code',
          sessionId: w.sessionId,
          messageId: w.messageId,
          ts,
          role: 'assistant',
          kind: 'thinking',
          text: b.thinking,
        });
      }
    } else if (block.type === 'tool_use') {
      const b = block as { id?: string; name?: string; input?: Record<string, unknown> };
      if (typeof b.id === 'string' && typeof b.name === 'string') {
        out.push({
          v: 1,
          source: 'claude-code',
          sessionId: w.sessionId,
          messageId: w.messageId,
          ts,
          role: 'assistant',
          kind: 'tool_use',
          toolUse: { id: b.id, name: b.name, input: b.input ?? {} },
        });
      }
    }
  }
  return out;
}

function extractUserContent(line: UserLine): ContentRecord[] {
  const out: ContentRecord[] = [];
  const sessionId = line.sessionId;
  const messageId = line.uuid;
  const ts = line.timestamp ?? '';
  if (!sessionId || !messageId) return out;
  const body = line.message?.content;
  if (typeof body === 'string') {
    if (body.length > 0) {
      out.push({
        v: 1,
        source: 'claude-code',
        sessionId,
        messageId,
        ts,
        role: 'user',
        kind: 'text',
        text: body,
      });
    }
    return out;
  }
  if (!Array.isArray(body)) return out;
  for (const block of body) {
    if (!block || typeof block !== 'object') continue;
    if (block.type === 'tool_result') {
      const tr = block as { tool_use_id?: string; content?: unknown; is_error?: boolean };
      if (typeof tr.tool_use_id !== 'string') continue;
      const record: ContentRecord = {
        v: 1,
        source: 'claude-code',
        sessionId,
        messageId,
        ts,
        role: 'tool_result',
        kind: 'tool_result',
        toolResult: { toolUseId: tr.tool_use_id, content: tr.content ?? '' },
      };
      if (tr.is_error === true) record.toolResult!.isError = true;
      out.push(record);
    } else if (block.type === 'text') {
      const b = block as { text?: string };
      if (typeof b.text === 'string' && b.text.length > 0) {
        out.push({
          v: 1,
          source: 'claude-code',
          sessionId,
          messageId,
          ts,
          role: 'user',
          kind: 'text',
          text: b.text,
        });
      }
    }
  }
  return out;
}

// Convert a user line into a `UserTurnRecord` carrying one block per
// `tool_result` and one `text` block per free-text chunk. Returns undefined
// when the line lacks a session id or uuid (we'd have nothing to anchor it
// to), or carries no measurable blocks.
//
// Token estimate uses the parser-selected user-turn tokenizer. The explicit
// `heuristic` mode is still useful for proportional allocation across blocks
// within the same user turn, where the constant cancels.
function buildUserTurnRecord(
  line: UserLine,
  precedingMessageId: string | undefined,
  tokenCounter: UserTurnTokenCounter,
): UserTurnRecord | undefined {
  const sessionId = line.sessionId;
  const userUuid = line.uuid;
  if (!sessionId || !userUuid) return undefined;
  const blocks = extractUserTurnBlocks(line, tokenCounter);
  if (blocks.length === 0) return undefined;
  const record: UserTurnRecord = {
    v: 1,
    source: 'claude-code',
    sessionId,
    userUuid,
    ts: line.timestamp ?? '',
    blocks,
  };
  if (precedingMessageId !== undefined) record.precedingMessageId = precedingMessageId;
  return record;
}

function extractUserTurnBlocks(
  line: UserLine,
  tokenCounter: UserTurnTokenCounter,
): UserTurnBlock[] {
  const out: UserTurnBlock[] = [];
  const body = line.message?.content;
  if (typeof body === 'string') {
    if (body.length > 0) out.push(makeTextBlock(body, tokenCounter));
    return out;
  }
  if (!Array.isArray(body)) return out;
  for (const block of body) {
    if (!block || typeof block !== 'object') continue;
    if (block.type === 'tool_result') {
      const tr = block as { tool_use_id?: string; content?: unknown; is_error?: boolean };
      if (typeof tr.tool_use_id !== 'string') continue;
      out.push(
        makeToolResultBlock(tr.tool_use_id, tr.content, tr.is_error === true, tokenCounter),
      );
    } else if (block.type === 'text') {
      const tb = block as { text?: string };
      if (typeof tb.text === 'string' && tb.text.length > 0) {
        out.push(makeTextBlock(tb.text, tokenCounter));
      }
    }
  }
  return out;
}

interface UsageWithCoverage {
  usage: Usage;
  coverage: WorkingRecord['usageCoverage'];
}

function toUsage(u: ClaudeUsage | undefined): UsageWithCoverage {
  const input = u?.input_tokens ?? 0;
  const output = u?.output_tokens ?? 0;
  const cacheRead = u?.cache_read_input_tokens ?? 0;
  const create5m = u?.cache_creation?.ephemeral_5m_input_tokens ?? 0;
  const create1h = u?.cache_creation?.ephemeral_1h_input_tokens ?? 0;
  const totalCreate = u?.cache_creation_input_tokens ?? 0;
  // Coverage tracks *whether the field was supplied*, not whether it's > 0.
  // A request with zero cache reads still has `hasCacheReadTokens: true` if
  // the upstream message included `cache_read_input_tokens: 0`.
  const coverage = {
    hasInputTokens: u?.input_tokens !== undefined,
    hasOutputTokens: u?.output_tokens !== undefined,
    hasCacheReadTokens: u?.cache_read_input_tokens !== undefined,
    hasCacheCreateTokens:
      u?.cache_creation_input_tokens !== undefined ||
      u?.cache_creation?.ephemeral_5m_input_tokens !== undefined ||
      u?.cache_creation?.ephemeral_1h_input_tokens !== undefined,
  };
  if (create5m === 0 && create1h === 0 && totalCreate > 0) {
    return {
      usage: { input, output, reasoning: 0, cacheRead, cacheCreate5m: totalCreate, cacheCreate1h: 0 },
      coverage,
    };
  }
  return {
    usage: { input, output, reasoning: 0, cacheRead, cacheCreate5m: create5m, cacheCreate1h: create1h },
    coverage,
  };
}

function mergeUsageCoverage(
  a: WorkingRecord['usageCoverage'],
  b: WorkingRecord['usageCoverage'],
): WorkingRecord['usageCoverage'] {
  // Multiple assistant lines for the same messageId can carry usage fields in
  // either of them (Claude streams partials). Treat coverage as monotonic:
  // once any line shows a field, the merged turn has it.
  return {
    hasInputTokens: a.hasInputTokens || b.hasInputTokens,
    hasOutputTokens: a.hasOutputTokens || b.hasOutputTokens,
    hasCacheReadTokens: a.hasCacheReadTokens || b.hasCacheReadTokens,
    hasCacheCreateTokens: a.hasCacheCreateTokens || b.hasCacheCreateTokens,
  };
}

function buildClaudeFidelity(
  usageCoverage: WorkingRecord['usageCoverage'],
): Fidelity {
  // Coverage is *capability* not *presence*: a turn with no tool_use blocks
  // still has `hasToolCalls: true`, because the question is "would this
  // source surface tool calls if they happened?" — not "did this turn have
  // tools?". Same logic for tool-result events, session relationships, and
  // raw content. Numeric usage is the exception: those flags reflect which
  // fields the upstream message actually carried, since Claude can omit them
  // (e.g. cache_creation absent on cache-cold requests).
  const coverage: Coverage = {
    ...EMPTY_COVERAGE,
    ...usageCoverage,
    // Reasoning is not represented in Claude Code's JSONL session log — the
    // model's thinking blocks are stored as content, not as a separate token
    // count. Mark unavailable so command-level projections don't pretend
    // otherwise.
    hasReasoningTokens: false,
    hasToolCalls: true,
    hasToolResultEvents: true,
    hasSessionRelationships: true,
    hasRawContent: true,
  };
  return makeFidelity('per-turn', coverage);
}

function extractToolCalls(
  blocks: ContentBlock[],
  erroredToolUseIds: Set<string>,
  replacementMetaByToolUseId?: Map<string, ReplacementMeta>,
): ToolCall[] {
  const out: ToolCall[] = [];
  const seen = new Set<string>();
  for (const b of blocks) {
    if (!b || b.type !== 'tool_use') continue;
    const tu = b as { id?: string; name?: string; input?: Record<string, unknown> };
    if (typeof tu.id !== 'string' || typeof tu.name !== 'string') continue;
    if (seen.has(tu.id)) continue;
    seen.add(tu.id);
    const input = tu.input ?? {};
    const call: ToolCall = {
      id: tu.id,
      name: tu.name,
      argsHash: argsHash(input),
    };
    const target = pickTarget(tu.name, input);
    if (target !== undefined) call.target = target;
    if (erroredToolUseIds.has(tu.id)) call.isError = true;
    applyEditHashes(call, input);
    const meta = replacementMetaByToolUseId?.get(tu.id);
    if (meta) {
      if (meta.replacedTools && meta.replacedTools.length > 0) {
        call.replacedTools = meta.replacedTools.slice();
      }
      if (typeof meta.collapsedCalls === 'number' && meta.collapsedCalls > 0) {
        call.collapsedCalls = meta.collapsedCalls;
      }
    }
    out.push(call);
  }
  return out;
}

function applyEditHashes(call: ToolCall, input: Record<string, unknown>): void {
  if (call.name === 'Edit' || call.name === 'NotebookEdit') {
    const oldStr = input['old_string'];
    const newStr = input['new_string'];
    if (typeof oldStr === 'string') call.editPreHash = contentHash(oldStr);
    if (typeof newStr === 'string') call.editPostHash = contentHash(newStr);
  } else if (call.name === 'Write') {
    const content = input['content'];
    if (typeof content === 'string') call.editPostHash = contentHash(content);
  }
}

// ---------------------------------------------------------------------------
// Execution graph helpers.
// ---------------------------------------------------------------------------

// Emit a `root` row for a session id. When the file's authoritative session
// id is known and differs from the in-log id (`/resume` or fork), prefer the
// file id as the canonical key — the in-log id is captured separately as
// `sourceSessionId` provenance by `annotateRelationshipsWithEvidence`. This
// keeps each on-disk session file pinned to exactly one root, which is what
// downstream cross-file reconciliation joins on.
function recordRoot(
  out: SessionRelationshipRecord[],
  seen: Set<string>,
  sessionId: string,
  ts: string | undefined,
  fileSessionId: string | undefined,
): void {
  const canonical = fileSessionId ?? sessionId;
  if (seen.has(canonical)) return;
  seen.add(canonical);
  const row: SessionRelationshipRecord = {
    v: 1,
    source: 'claude-code',
    sessionId: canonical,
    relationshipType: 'root',
  };
  if (typeof ts === 'string' && ts.length > 0) row.ts = ts;
  out.push(row);
}

function collectExplicitClaudeRelationships(
  line: Record<string, unknown>,
  evidence: ClaudeRelationshipEvidence,
  out: SessionRelationshipRecord[],
  seen: Set<string>,
  sessionId: string,
  fallbackTs: string | undefined,
): void {
  recordExplicitRelationshipEvidence(evidence, line);
  for (const row of buildExplicitClaudeRelationships(line, sessionId, fallbackTs)) {
    const key = relationshipKey(row);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(row);
  }
}

function collectExplicitClaudeRelationshipsIncremental(
  line: Record<string, unknown>,
  evidence: ClaudeRelationshipEvidence,
  out: Array<{ offset: number; record: SessionRelationshipRecord }>,
  seen: Set<string>,
  sessionId: string,
  fallbackTs: string | undefined,
  offset: number,
): void {
  recordExplicitRelationshipEvidence(evidence, line);
  for (const row of buildExplicitClaudeRelationships(line, sessionId, fallbackTs)) {
    const key = relationshipKey(row);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ offset, record: row });
  }
}

function buildExplicitClaudeRelationships(
  line: Record<string, unknown>,
  sessionId: string,
  fallbackTs: string | undefined,
): SessionRelationshipRecord[] {
  const rows: SessionRelationshipRecord[] = [];
  const forkSessionId = lineStringField(line, ['forkSessionId', 'fork_session_id']);
  if (forkSessionId !== undefined && forkSessionId !== sessionId) {
    rows.push(buildExplicitClaudeRelationship(line, sessionId, forkSessionId, 'fork', fallbackTs));
  }
  const continuedFromSessionId = lineStringField(line, [
    'continuedFromSessionId',
    'continued_from_session_id',
  ]);
  if (continuedFromSessionId !== undefined && continuedFromSessionId !== sessionId) {
    rows.push(
      buildExplicitClaudeRelationship(
        line,
        sessionId,
        continuedFromSessionId,
        'continuation',
        fallbackTs,
      ),
    );
  }
  return rows;
}

function buildExplicitClaudeRelationship(
  line: Record<string, unknown>,
  sessionId: string,
  relatedSessionId: string,
  relationshipType: 'fork' | 'continuation',
  fallbackTs: string | undefined,
): SessionRelationshipRecord {
  const row: SessionRelationshipRecord = {
    v: 1,
    source: 'claude-code',
    sessionId,
    relatedSessionId,
    relationshipType,
  };
  const ts = lineStringField(line, ['timestamp', 'ts']) ?? fallbackTs;
  if (ts !== undefined) row.ts = ts;
  const sourceSessionId = lineStringField(line, ['sourceSessionId', 'source_session_id']);
  if (sourceSessionId !== undefined) row.sourceSessionId = sourceSessionId;
  const sourceVersion = lineStringField(line, ['version', 'sourceVersion', 'source_version']);
  if (sourceVersion !== undefined) row.sourceVersion = sourceVersion;
  return row;
}

function recordExplicitRelationshipEvidence(
  evidence: ClaudeRelationshipEvidence,
  line: Record<string, unknown>,
): void {
  const continuation = lineStringField(line, ['continuedFromSessionId', 'continued_from_session_id']);
  if (continuation !== undefined) {
    evidence.explicitContinuationTargetSessionIds = appendUnique(
      evidence.explicitContinuationTargetSessionIds,
      continuation,
    );
  }
  const fork = lineStringField(line, ['forkSessionId', 'fork_session_id']);
  if (fork !== undefined) {
    evidence.explicitForkTargetSessionIds = appendUnique(
      evidence.explicitForkTargetSessionIds,
      fork,
    );
  }
}

function appendUnique(values: string[] | undefined, value: string): string[] {
  if (values === undefined) return [value];
  if (!values.includes(value)) values.push(value);
  return values;
}

function relationshipKey(row: SessionRelationshipRecord): string {
  return [
    row.source,
    row.sessionId,
    row.relationshipType,
    row.relatedSessionId ?? '',
    row.agentId ?? '',
    row.parentToolUseId ?? '',
  ].join('|');
}

function hasRelationship(rows: SessionRelationshipRecord[], row: SessionRelationshipRecord): boolean {
  const key = relationshipKey(row);
  return rows.some((existing) => relationshipKey(existing) === key);
}

function buildClaudeSystemToolResultEvent(
  line: Record<string, unknown>,
  counters: Map<string, number>,
  eventIndex: number,
): ToolResultEventRecord | undefined {
  const sessionId = lineStringField(line, ['sessionId', 'session_id']);
  const toolUseId = lineStringField(line, [
    'parent_tool_use_id',
    'parentToolUseId',
    'parentToolUseID',
    'tool_use_id',
    'toolUseId',
  ]);
  const agentId = lineStringField(line, ['agent_id', 'agentId']);
  const subagentSessionId = lineStringField(line, [
    'subagent_session_id',
    'subagentSessionId',
  ]);
  if (sessionId === undefined || toolUseId === undefined) return undefined;
  if (agentId === undefined && subagentSessionId === undefined) return undefined;
  const callIndex = counters.get(toolUseId) ?? 0;
  counters.set(toolUseId, callIndex + 1);
  const status = claudeSystemEventStatus(line);
  const record: ToolResultEventRecord = {
    v: 1,
    source: 'claude-code',
    sessionId,
    toolUseId,
    callIndex,
    eventIndex,
    status,
    eventSource: 'subagent_notification',
  };
  const ts = lineStringField(line, ['timestamp', 'ts']);
  if (ts !== undefined) record.ts = ts;
  if (agentId !== undefined) record.agentId = agentId;
  if (subagentSessionId !== undefined) record.subagentSessionId = subagentSessionId;
  if (status === 'errored') record.isError = true;
  const content = firstPresent(line, ['content', 'output', 'result', 'message']);
  const measured = measureToolResult(content);
  if (measured.length !== undefined) record.contentLength = measured.length;
  if (measured.hash !== undefined) record.contentHash = measured.hash;
  return record;
}

function claudeSystemEventStatus(line: Record<string, unknown>): ToolResultStatus {
  if (line['is_error'] === true || line['isError'] === true) return 'errored';
  const status = normalizeToolResultStatus(
    lineStringField(line, ['status', 'state', 'result', 'terminal_status', 'terminalStatus']),
  );
  if (status !== undefined) return status;
  if (line['success'] === true) return 'completed';
  if (line['success'] === false) return 'errored';
  return 'unknown';
}

function normalizeToolResultStatus(value: string | undefined): ToolResultStatus | undefined {
  if (value === undefined) return undefined;
  const normalized = value.toLowerCase().replace(/[-\s]/g, '_');
  if (
    normalized === 'completed' ||
    normalized === 'complete' ||
    normalized === 'success' ||
    normalized === 'succeeded' ||
    normalized === 'done'
  ) {
    return 'completed';
  }
  if (
    normalized === 'error' ||
    normalized === 'errored' ||
    normalized === 'failed' ||
    normalized === 'failure'
  ) {
    return 'errored';
  }
  if (
    normalized === 'running' ||
    normalized === 'in_progress' ||
    normalized === 'queued' ||
    normalized === 'pending' ||
    normalized === 'started'
  ) {
    return 'running';
  }
  if (
    normalized === 'cancelled' ||
    normalized === 'canceled' ||
    normalized === 'aborted'
  ) {
    return 'cancelled';
  }
  return undefined;
}

function firstPresent(obj: Record<string, unknown>, keys: ReadonlyArray<string>): unknown {
  for (const key of keys) {
    if (Object.prototype.hasOwnProperty.call(obj, key)) return obj[key];
  }
  return undefined;
}

function lineStringField(
  obj: Record<string, unknown>,
  keys: ReadonlyArray<string>,
): string | undefined {
  for (const key of keys) {
    const value = obj[key];
    if (typeof value === 'string' && value.length > 0) return value;
  }
  return undefined;
}

// Walk a user line's tool_result blocks and emit one ToolResultEventRecord
// per block. Status follows from is_error: errored vs completed; `running` /
// `cancelled` are reserved for future progress / queue events that Claude
// historical logs don't expose by default. Returns the next eventIndex.
function collectToolResultEvents(
  line: UserLine,
  out: ToolResultEventRecord[],
  counters: Map<string, number>,
  startIndex: number,
): number {
  let nextIndex = startIndex;
  const sessionId = line.sessionId;
  if (typeof sessionId !== 'string' || sessionId.length === 0) return nextIndex;
  const body = line.message?.content;
  if (!Array.isArray(body)) return nextIndex;
  const messageId = typeof line.uuid === 'string' ? line.uuid : undefined;
  const ts = typeof line.timestamp === 'string' ? line.timestamp : undefined;
  for (const block of body) {
    if (!block || typeof block !== 'object' || block.type !== 'tool_result') continue;
    const tr = block as {
      tool_use_id?: string;
      content?: unknown;
      is_error?: boolean;
    };
    if (typeof tr.tool_use_id !== 'string' || tr.tool_use_id.length === 0) continue;
    const callIndex = counters.get(tr.tool_use_id) ?? 0;
    counters.set(tr.tool_use_id, callIndex + 1);
    const isError = tr.is_error === true;
    const record: ToolResultEventRecord = {
      v: 1,
      source: 'claude-code',
      sessionId,
      toolUseId: tr.tool_use_id,
      callIndex,
      eventIndex: nextIndex++,
      status: isError ? 'errored' : 'completed',
      eventSource: 'tool_result',
    };
    if (messageId !== undefined) record.messageId = messageId;
    if (ts !== undefined) record.ts = ts;
    if (isError) record.isError = true;
    const measured = measureToolResult(tr.content);
    if (measured.length !== undefined) record.contentLength = measured.length;
    if (measured.hash !== undefined) record.contentHash = measured.hash;
    const meta = extractReplacementMetaFromToolResult(block);
    if (meta) {
      if (meta.replacedTools && meta.replacedTools.length > 0) {
        record.replacedTools = meta.replacedTools.slice();
      }
      if (typeof meta.collapsedCalls === 'number' && meta.collapsedCalls > 0) {
        record.collapsedCalls = meta.collapsedCalls;
      }
    }
    out.push(record);
  }
  return nextIndex;
}

function measureToolResult(content: unknown): { length?: number; hash?: string } {
  // Hash the canonical JSON serialization. For pure strings we hash the bytes
  // directly so callers can compare a tool_result to its raw text.
  if (typeof content === 'string') {
    return { length: content.length, hash: contentHash(content) };
  }
  if (content === undefined || content === null) {
    return {};
  }
  try {
    const serialized = JSON.stringify(content);
    if (typeof serialized !== 'string') return {};
    return { length: serialized.length, hash: contentHash(serialized) };
  } catch {
    return {};
  }
}

// Walk the turns we just emitted and record one `subagent` relationship row
// per distinct (subagent invocation) we see. Keyed by agentId so re-emitting
// the same invocation across multiple turns produces a single row. The row
// lives on the *child* session (which, in Claude's case, is the same file
// session id) and points at `parentAgentId` as `relatedSessionId` for nested
// invocations, or at the file's session id for first-level subagents.
function collectSubagentRelationships(
  turns: TurnRecord[],
  out: SessionRelationshipRecord[],
): void {
  const seen = new Set<string>();
  for (const t of turns) {
    const sub = t.subagent;
    if (!sub || !sub.isSidechain) continue;
    const agentId = sub.agentId;
    if (!agentId) continue;
    if (seen.has(agentId)) continue;
    seen.add(agentId);
    const row: SessionRelationshipRecord = {
      v: 1,
      source: 'native-claude',
      sessionId: t.sessionId,
      relationshipType: 'subagent',
      agentId,
    };
    // For first-level subagents, parentAgentId is set to the file's
    // sessionId; for nested ones it's the enclosing invocation's agentId.
    // Either way it's the right value to put on `relatedSessionId` so
    // consumers can join child -> parent.
    if (sub.parentAgentId !== undefined) row.relatedSessionId = sub.parentAgentId;
    if (sub.parentToolUseId !== undefined) row.parentToolUseId = sub.parentToolUseId;
    if (sub.subagentType !== undefined) row.subagentType = sub.subagentType;
    if (sub.description !== undefined) row.description = sub.description;
    if (typeof t.ts === 'string' && t.ts.length > 0) row.ts = t.ts;
    out.push(row);
  }
}

// ---------------------------------------------------------------------------
// Fork / continuation helpers.
//
// Single-file parsers can't see across session files, so they collect evidence
// (in-log session id, version, first parentUuid, /resume markers, all uuids
// touched) and surface it on the parse result. `reconcileClaudeSessionRelationships`
// takes the per-file evidence from a multi-file pass and emits or upgrades
// `fork` / `continuation` rows.
//
// Reconciliation strategy: append, don't mutate. `relationshipIdHash` keys on
// `relationshipType`, so a `root` row already in the ledger and a later
// `continuation` row for the same session id produce *different* hashes —
// both rows coexist after a follow-up reconciliation pass. Consumers that
// care about "is this session a child of another?" prefer the more specific
// row (fork / continuation > root) when both are present for a session id.
// This keeps the ledger append-only and re-ingest idempotent.
// ---------------------------------------------------------------------------

function deriveFileSessionId(options: ParseOptions): string | undefined {
  if (typeof options.fileSessionId === 'string' && options.fileSessionId.length > 0) {
    return options.fileSessionId;
  }
  if (typeof options.sessionPath === 'string' && options.sessionPath.length > 0) {
    const base = basename(options.sessionPath, '.jsonl');
    return base.length > 0 ? base : undefined;
  }
  return undefined;
}

function newEvidence(fileSessionId?: string): ClaudeRelationshipEvidence {
  const ev: ClaudeRelationshipEvidence = {
    inLogSessionIds: [],
    seenUuids: [],
    hasResumeMarker: false,
  };
  if (fileSessionId !== undefined) ev.fileSessionId = fileSessionId;
  return ev;
}

// Tracks whether the parser has already observed any non-sidechain user line.
// Carried alongside `ClaudeRelationshipEvidence` so we only consider the very
// first user line's `parentUuid` as the cross-file continuation signal.
const evidenceUserSeen = new WeakSet<ClaudeRelationshipEvidence>();

function recordEvidenceFromLine(
  ev: ClaudeRelationshipEvidence,
  line: {
    type?: string;
    sessionId?: string;
    version?: string;
    uuid?: string;
    parentUuid?: string | null;
    isSidechain?: boolean;
    timestamp?: string;
  },
): void {
  if (typeof line.uuid === 'string' && line.uuid.length > 0) {
    ev.seenUuids.push(line.uuid);
  }
  if (typeof line.sessionId === 'string' && line.sessionId.length > 0) {
    if (!ev.inLogSessionIds.includes(line.sessionId)) ev.inLogSessionIds.push(line.sessionId);
    if (
      ev.firstTs === undefined &&
      typeof line.timestamp === 'string' &&
      line.timestamp.length > 0
    ) {
      ev.firstTs = line.timestamp;
    }
  }
  if (
    ev.sourceVersion === undefined &&
    typeof line.version === 'string' &&
    line.version.length > 0
  ) {
    ev.sourceVersion = line.version;
  }
  // Cross-file continuation signal: only the very first non-sidechain user
  // line carries it. Assistant `parentUuid` always points inside the same
  // file (the prior user line), so it would never resolve cross-file.
  // Subsequent user lines in the same session also point inside the file.
  // Sidechain user lines must not arm the gate, otherwise a leading
  // sidechain prompt would block the first main-thread user line from
  // setting `firstParentUuid` and we'd lose cross-file continuation.
  if (line.type === 'user' && line.isSidechain !== true && !evidenceUserSeen.has(ev)) {
    evidenceUserSeen.add(ev);
    if (typeof line.parentUuid === 'string' && line.parentUuid.length > 0) {
      ev.firstParentUuid = line.parentUuid;
    }
  }
}

// Detect a slash-command continuation marker on a user line. Claude historical
// logs surface `/resume [sessionId]` and `/continue [sessionId]` as plain user
// text when the harness records the command verbatim. We treat either as a
// `continuation` signal; when an argument is present and looks like a session
// token, we use it as `relatedSessionId` on the emitted continuation row.
function recordResumeMarker(ev: ClaudeRelationshipEvidence, line: UserLine): void {
  const text = extractPlainUserText(line);
  if (typeof text !== 'string' || text.length === 0) return;
  const trimmed = text.trim();
  const match = trimmed.match(/^\/(resume|continue)(?:\s+(\S+))?/i);
  if (!match) return;
  ev.hasResumeMarker = true;
  if (
    ev.resumeTargetSessionId === undefined &&
    typeof match[2] === 'string' &&
    match[2].length > 0
  ) {
    ev.resumeTargetSessionId = match[2];
  }
}

// When the file carries a `/resume` marker, emit a local `continuation` row
// even without cross-file knowledge. The row is the file's own session id;
// `relatedSessionId` is the resumed-from id when the marker carried one.
// Cross-file reconciliation later may add a second row with a stronger
// `relatedSessionId` once it can resolve `firstParentUuid` — both rows have
// distinct hashes and coexist (see strategy doc above).
function emitLocalContinuationFromResume(
  out: SessionRelationshipRecord[],
  ev: ClaudeRelationshipEvidence,
): void {
  if (!ev.hasResumeMarker) return;
  if (ev.fileSessionId === undefined) return;
  const row: SessionRelationshipRecord = {
    v: 1,
    source: 'claude-code',
    sessionId: ev.fileSessionId,
    relationshipType: 'continuation',
  };
  if (ev.resumeTargetSessionId !== undefined) row.relatedSessionId = ev.resumeTargetSessionId;
  if (ev.firstTs !== undefined) row.ts = ev.firstTs;
  if (hasRelationship(out, row)) return;
  applyEvidenceProvenance(row, ev);
  out.push(row);
}

// Stamp every emitted relationship row with the in-log `sourceSessionId` (when
// it differs from the file's session id) and the first observed `version`.
// Run *after* all rows are collected so subagent / continuation rows added
// later in the parse pipeline get the same provenance as roots.
function annotateRelationshipsWithEvidence(
  rows: SessionRelationshipRecord[],
  ev: ClaudeRelationshipEvidence,
): void {
  for (const r of rows) applyEvidenceProvenance(r, ev);
}

function applyEvidenceProvenance(
  row: SessionRelationshipRecord,
  ev: ClaudeRelationshipEvidence,
): void {
  if (row.sourceSessionId === undefined) {
    const foreign = pickForeignSessionId(ev);
    if (foreign !== undefined) row.sourceSessionId = foreign;
  }
  if (row.sourceVersion === undefined && ev.sourceVersion !== undefined) {
    row.sourceVersion = ev.sourceVersion;
  }
}

function pickForeignSessionId(ev: ClaudeRelationshipEvidence): string | undefined {
  if (ev.fileSessionId === undefined) return undefined;
  for (const id of ev.inLogSessionIds) {
    if (id !== ev.fileSessionId) return id;
  }
  return undefined;
}

// ---------------------------------------------------------------------------
// Cross-file reconciliation.
// ---------------------------------------------------------------------------

export interface ReconcileClaudeRelationshipsInput {
  evidence: ClaudeRelationshipEvidence;
}

// Walk a set of per-file evidences and emit `fork` / `continuation` rows
// captured by cross-file matches. Output is *additional* relationship rows
// to append alongside the per-file output of `parseClaudeSession`. Idempotent:
// re-running with the same evidence produces the same rows, and the existing
// `relationshipIdHash` dedup folds duplicates at write time.
//
// Rules:
//   - Continuation: file F's `firstParentUuid` lives in another file G's
//     `seenUuids`, *and* the two files have distinct session ids. Emit a
//     `continuation` row keyed on F.fileSessionId, `relatedSessionId =
//     G.fileSessionId`. Skipped when F already carried a local `/resume`
//     marker that named G — the parse pass already emitted that row.
//   - Fork: two files share the same `sourceSessionId`, neither is a
//     continuation of the other, and neither's session id equals the shared
//     `sourceSessionId`. Each file gets a `fork` row with `relatedSessionId`
//     set to the shared `sourceSessionId`.
export function reconcileClaudeSessionRelationships(
  inputs: ReconcileClaudeRelationshipsInput[],
): SessionRelationshipRecord[] {
  const out: SessionRelationshipRecord[] = [];
  const usable = inputs.filter((i) => i.evidence.fileSessionId !== undefined);
  if (usable.length === 0) return out;

  // uuid -> fileSessionId mapping for cross-file `firstParentUuid` resolution.
  const uuidToFileSession = new Map<string, string>();
  for (const i of usable) {
    const sid = i.evidence.fileSessionId!;
    for (const u of i.evidence.seenUuids) {
      if (!uuidToFileSession.has(u)) uuidToFileSession.set(u, sid);
    }
  }

  // Track which (sessionId, parentSessionId) pairs got a continuation row so
  // fork detection can skip them.
  const continuationOf = new Map<string, string>();

  for (const i of usable) {
    const ev = i.evidence;
    const sid = ev.fileSessionId!;
    if (ev.firstParentUuid === undefined) continue;
    const parentSid = uuidToFileSession.get(ev.firstParentUuid);
    if (!parentSid) continue;
    if (parentSid === sid) continue;
    continuationOf.set(sid, parentSid);
    // Skip when the local parser already emitted a continuation row pointing
    // at exactly this parent — avoids a duplicate hash collision at write
    // time (the dedup index would silently drop one anyway).
    if (ev.hasResumeMarker && ev.resumeTargetSessionId === parentSid) continue;
    if (hasExplicitTarget(ev.explicitContinuationTargetSessionIds, parentSid)) continue;
    const row: SessionRelationshipRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: sid,
      relatedSessionId: parentSid,
      relationshipType: 'continuation',
    };
    if (ev.firstTs !== undefined) row.ts = ev.firstTs;
    applyEvidenceProvenance(row, ev);
    out.push(row);
  }

  // Fork: group files by foreign sourceSessionId and emit one row per file
  // when the group has 2+ members and the file isn't a strict continuation
  // of another file in the same group.
  const bySourceSession = new Map<string, ClaudeRelationshipEvidence[]>();
  for (const i of usable) {
    const ev = i.evidence;
    const foreign = pickForeignSessionId(ev);
    if (foreign === undefined) continue;
    if (foreign === ev.fileSessionId) continue;
    const bucket = bySourceSession.get(foreign);
    if (bucket) bucket.push(ev);
    else bySourceSession.set(foreign, [ev]);
  }
  for (const [foreign, group] of bySourceSession) {
    if (group.length < 2) continue;
    for (const ev of group) {
      const sid = ev.fileSessionId!;
      // A file that's a continuation of another file in the group is not a
      // fork from the shared source — it's a linear successor.
      const parent = continuationOf.get(sid);
      if (parent !== undefined && group.some((g) => g.fileSessionId === parent)) continue;
      if (hasExplicitTarget(ev.explicitForkTargetSessionIds, foreign)) continue;
      const row: SessionRelationshipRecord = {
        v: 1,
        source: 'claude-code',
        sessionId: sid,
        relatedSessionId: foreign,
        relationshipType: 'fork',
        sourceSessionId: foreign,
      };
      if (ev.firstTs !== undefined) row.ts = ev.firstTs;
      if (ev.sourceVersion !== undefined) row.sourceVersion = ev.sourceVersion;
      out.push(row);
    }
  }

  return out;
}

function hasExplicitTarget(targets: string[] | undefined, sessionId: string): boolean {
  return targets?.includes(sessionId) === true;
}

// Mark tool_result events whose toolUseId resolved to a subagent invocation
// with the matching `agentId` so `ToolResultEventRecord` rows can be joined
// to the spawned subagent without re-walking the chain.
function annotateSpawnEvents(
  events: ToolResultEventRecord[],
  turns: TurnRecord[],
): void {
  if (events.length === 0) return;
  const agentByParentToolUse = new Map<string, string>();
  for (const t of turns) {
    const sub = t.subagent;
    if (!sub || !sub.isSidechain) continue;
    if (sub.parentToolUseId && sub.agentId) {
      // Only the first occurrence wins; all turns of one invocation share the
      // same (parentToolUseId, agentId) pair, so order doesn't matter.
      if (!agentByParentToolUse.has(sub.parentToolUseId)) {
        agentByParentToolUse.set(sub.parentToolUseId, sub.agentId);
      }
    }
  }
  if (agentByParentToolUse.size === 0) return;
  for (const ev of events) {
    const agentId = agentByParentToolUse.get(ev.toolUseId);
    if (agentId) ev.agentId = agentId;
  }
}

function annotateCompactionEvents(
  events: CompactionEvent[],
  turns: TurnRecord[],
): void {
  if (events.length === 0) return;
  const byMessageId = new Map<string, TurnRecord>();
  for (const t of turns) byMessageId.set(t.messageId, t);
  for (const ev of events) {
    if (ev.precedingMessageId) {
      const t = byMessageId.get(ev.precedingMessageId);
      if (t) {
        ev.tokensBeforeCompact = t.usage.cacheRead;
      }
    }
  }
}

function pickTarget(name: string, input: Record<string, unknown>): string | undefined {
  const s = (k: string): string | undefined => {
    const v = input[k];
    return typeof v === 'string' ? v : undefined;
  };
  switch (name) {
    case 'Read':
    case 'Edit':
    case 'Write':
    case 'NotebookEdit':
      return s('file_path');
    case 'Bash':
      return s('command');
    case 'Grep':
      return s('pattern');
    case 'Glob':
      return s('pattern');
    case 'WebFetch':
      return s('url');
    case 'Agent':
    case 'Task':
      return s('subagent_type') ?? s('description');
    default:
      return s('file_path') ?? s('path') ?? s('url') ?? s('command');
  }
}

function extractFilesTouched(toolCalls: ToolCall[]): string[] {
  const files = new Set<string>();
  for (const tc of toolCalls) {
    if (!tc.target) continue;
    if (tc.name === 'Read' || tc.name === 'Edit' || tc.name === 'Write' || tc.name === 'NotebookEdit') {
      files.add(tc.target);
    }
  }
  return [...files];
}

function resolveSubagent(
  w: WorkingRecord,
  nodesByUuid?: Map<string, LineNode>,
  invocationCache?: Map<string, InvocationInfo | null>,
): Subagent | undefined {
  if (!w.isSidechain) return undefined;
  const sub: Subagent = { isSidechain: true };
  if (!nodesByUuid || !w.firstAssistantUuid) return sub;
  const info = resolveInvocation(w.firstAssistantUuid, nodesByUuid, invocationCache);
  if (!info) return sub;
  sub.agentId = info.rootUuid;
  if (info.parentToolUseId !== undefined) {
    sub.parentToolUseId = info.parentToolUseId;
  }
  if (info.subagentType !== undefined) sub.subagentType = info.subagentType;
  if (info.description !== undefined) sub.description = info.description;
  if (info.parentAgentId !== undefined) {
    sub.parentAgentId = info.parentAgentId;
  } else {
    // First-level subagent: parent is the main thread. Use the session id as a
    // stable anchor so callers can build parent→child trees without a null
    // sentinel.
    sub.parentAgentId = w.sessionId;
  }
  return sub;
}

interface InvocationInfo {
  rootUuid: string;
  parentToolUseId?: string;
  subagentType?: string;
  description?: string;
  parentAgentId?: string;
}

// Walk the parentUuid chain starting from `startUuid` looking for the user
// line that is the root of a subagent invocation: a user message whose
// immediate parent is an assistant line carrying an Agent/Task tool_use block.
// Returns undefined if no such boundary is found before the chain runs out
// (e.g. partial/incremental data, or `startUuid` belongs to the main thread).
// `cache` memoizes results per startUuid so the recursive parent-invocation
// resolution doesn't re-walk the outer chain once per inner turn.
function resolveInvocation(
  startUuid: string,
  nodes: Map<string, LineNode>,
  cache?: Map<string, InvocationInfo | null>,
  depth = 0,
): InvocationInfo | undefined {
  // Cycle / pathological-data guard; real chains are shallow.
  if (depth > 64) return undefined;
  if (cache) {
    const cached = cache.get(startUuid);
    if (cached !== undefined) return cached ?? undefined;
  }
  let node = nodes.get(startUuid);
  // Guard against parentUuid cycles (A→B→A) in malformed JSONL — the depth
  // guard only covers recursion, not the while-loop walk.
  const visited = new Set<string>();
  while (node) {
    if (visited.has(node.uuid)) break;
    visited.add(node.uuid);
    const parent = node.parentUuid ? nodes.get(node.parentUuid) : undefined;
    if (!parent) break;
    if (
      node.kind === 'user' &&
      parent.kind === 'assistant' &&
      parent.agentToolUse &&
      !(node.toolResultIds && node.toolResultIds.has(parent.agentToolUse.id))
    ) {
      const out: InvocationInfo = { rootUuid: node.uuid };
      if (parent.agentToolUse.id) out.parentToolUseId = parent.agentToolUse.id;
      if (parent.agentToolUse.subagentType !== undefined) out.subagentType = parent.agentToolUse.subagentType;
      if (parent.agentToolUse.description !== undefined) out.description = parent.agentToolUse.description;
      if (parent.isSidechain) {
        const parentInvocation = resolveInvocation(parent.uuid, nodes, cache, depth + 1);
        if (parentInvocation) out.parentAgentId = parentInvocation.rootUuid;
      }
      if (cache) cache.set(startUuid, out);
      return out;
    }
    node = parent;
  }
  if (cache) cache.set(startUuid, null);
  return undefined;
}

function extractPlainUserText(line: UserLine): string | undefined {
  const body = line.message?.content;
  if (typeof body === 'string') {
    return body.length > 0 ? body : undefined;
  }
  if (!Array.isArray(body)) return undefined;
  const parts: string[] = [];
  for (const block of body) {
    if (!block || typeof block !== 'object') continue;
    if (block.type === 'text') {
      const b = block as { text?: string };
      if (typeof b.text === 'string' && b.text.length > 0) parts.push(b.text);
    }
  }
  return parts.length > 0 ? parts.join('\n') : undefined;
}

function collectErroredToolUseIds(line: UserLine, into: Set<string>): void {
  const body = line.message?.content;
  if (!Array.isArray(body)) return;
  for (const block of body) {
    if (!block || typeof block !== 'object' || block.type !== 'tool_result') continue;
    const tr = block as { tool_use_id?: string; is_error?: boolean };
    if (tr.is_error === true && typeof tr.tool_use_id === 'string') {
      into.add(tr.tool_use_id);
    }
  }
}

export interface ReplacementMeta {
  replacedTools?: string[];
  collapsedCalls?: number;
}

// Extract `_meta.replaces` / `_meta.collapsedCalls` from a tool_result block.
// Replacement tools (e.g. relaywash) ship these annotations on the tool_result
// `_meta` field, and on Anthropic's API the `_meta` is also surfaced as
// nested fields on the structured `content` array entries. We accept either
// shape: top-level `_meta` on the block, or a `_meta` nested anywhere inside
// `content` when content is structured.
function extractReplacementMetaFromToolResult(block: unknown): ReplacementMeta | undefined {
  if (!block || typeof block !== 'object') return undefined;
  const meta =
    pickReplacementMeta((block as Record<string, unknown>)['_meta']) ??
    findNestedReplacementMeta((block as Record<string, unknown>)['content']);
  return meta;
}

function pickReplacementMeta(raw: unknown): ReplacementMeta | undefined {
  if (!raw || typeof raw !== 'object') return undefined;
  const obj = raw as Record<string, unknown>;
  const out: ReplacementMeta = {};
  const replaces = obj['replaces'];
  if (Array.isArray(replaces)) {
    const names = replaces.filter((v): v is string => typeof v === 'string' && v.length > 0);
    if (names.length > 0) out.replacedTools = names;
  }
  const collapsed = obj['collapsedCalls'];
  if (typeof collapsed === 'number' && Number.isFinite(collapsed) && collapsed > 0) {
    out.collapsedCalls = Math.floor(collapsed);
  }
  if (out.replacedTools === undefined && out.collapsedCalls === undefined) return undefined;
  return out;
}

function findNestedReplacementMeta(content: unknown): ReplacementMeta | undefined {
  if (!Array.isArray(content)) return undefined;
  for (const entry of content) {
    if (!entry || typeof entry !== 'object') continue;
    const meta = pickReplacementMeta((entry as Record<string, unknown>)['_meta']);
    if (meta) return meta;
  }
  return undefined;
}

function collectReplacementMeta(
  line: UserLine,
  into: Map<string, ReplacementMeta>,
): void {
  const body = line.message?.content;
  if (!Array.isArray(body)) return;
  for (const block of body) {
    if (!block || typeof block !== 'object' || (block as { type?: string }).type !== 'tool_result') continue;
    const tr = block as { tool_use_id?: string };
    if (typeof tr.tool_use_id !== 'string' || tr.tool_use_id.length === 0) continue;
    const meta = extractReplacementMetaFromToolResult(block);
    if (meta) into.set(tr.tool_use_id, meta);
  }
}

function applyClassification(
  record: TurnRecord,
  w: WorkingRecord,
  userTextByMessageId: Map<string, string>,
  erroredToolUseIds: Set<string>,
): void {
  const userText = userTextByMessageId.get(w.messageId) ?? '';
  const assistantText = extractAssistantTextForClassification(w.blocks);
  const text = [userText, assistantText].filter((s) => s.length > 0).join('\n');
  const hasFailedTool = record.toolCalls.some((tc) => erroredToolUseIds.has(tc.id));
  const result = classifyActivity({
    toolCalls: record.toolCalls,
    text,
    hasFailedTool,
    reasoningTokens: record.usage.reasoning,
  });
  record.activity = result.activity;
  record.retries = result.retries;
  record.hasEdits = result.hasEdits;
}

function extractAssistantTextForClassification(blocks: ContentBlock[]): string {
  const parts: string[] = [];
  for (const b of blocks) {
    if (!b || typeof b !== 'object') continue;
    if (b.type === 'text') {
      const tb = b as { text?: string };
      if (typeof tb.text === 'string' && tb.text.length > 0) parts.push(tb.text);
    }
  }
  return parts.join('\n');
}

export interface ParseIncrementalOptions extends ParseOptions {
  startOffset?: number;
  // The most recent user prompt text seen before `startOffset`. Classification
  // uses the user prompt for keyword refinement; when `endOffset` backs up to
  // an incomplete assistant line, the prompt that preceded it is before the
  // resume point and won't be re-read — so callers must carry it forward.
  lastUserText?: string;
}

export interface ParseIncrementalResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  events: CompactionEvent[];
  // Execution graph — see ParseResult for shape. Both arrays follow
  // the same endOffset dedup rule the rest of the incremental result uses:
  // any record whose source line lives at or past endOffset is deferred to
  // the next pass so we don't double-emit on resume.
  relationships: SessionRelationshipRecord[];
  toolResultEvents: ToolResultEventRecord[];
  endOffset: number;
  // Carry forward to the next incremental call; see `lastUserText` option.
  lastUserText: string;
  // Per-user-turn block info between assistant turns. Filtered by
  // `endOffset` like content/events so the next incremental pass re-reads any
  // bytes past the cursor without double-emitting.
  userTurns: UserTurnRecord[];
  // Per-file relationship evidence. Reflects everything the parser saw
  // in *this* pass (including the prescanned prefix when resuming). Cumulative
  // — callers that drive multi-pass ingest can use the latest call's
  // `evidence` when feeding `reconcileClaudeSessionRelationships`.
  evidence: ClaudeRelationshipEvidence;
}

export async function parseClaudeSessionIncremental(
  filePath: string,
  options: ParseIncrementalOptions = {},
): Promise<ParseIncrementalResult> {
  const startOffset = options.startOffset ?? 0;
  const contentMode = options.contentMode ?? 'off';
  const captureContent = contentMode === 'full';
  const handle = await open(filePath, 'r');
  let buf: Buffer;
  let size: number;
  try {
    const st = await handle.stat();
    size = st.size;
    if (startOffset >= size) {
      return {
        turns: [],
        content: [],
        events: [],
        relationships: [],
        toolResultEvents: [],
        endOffset: startOffset,
        lastUserText: options.lastUserText ?? '',
        userTurns: [],
        evidence: newEvidence(deriveFileSessionId(options)),
      };
    }
    const length = size - startOffset;
    buf = Buffer.allocUnsafe(length);
    await handle.read(buf, 0, length, startOffset);
  } finally {
    await handle.close();
  }

  const tokenCounter = await createUserTurnTokenCounter(options.tokenizer);
  const working = new Map<string, WorkingRecord>();
  const order: string[] = [];
  const nodesByUuid = new Map<string, LineNode>();
  const invocationCache = new Map<string, InvocationInfo | null>();
  // When resuming mid-file, populate nodesByUuid from the already-ingested
  // prefix so new sidechain turns can still resolve their invocation root via
  // parentUuid chains that point back before startOffset. Without this,
  // subagent tree fields come up empty on the primary incremental ingest path.
  // Also captures the last assistant messageId before the resume point so
  // user turns landing in this pass can record `precedingMessageId` even
  // when their preceding assistant was already ingested previously.
  // Per-file relationship evidence. Seeded from the prescan when
  // resuming so cross-file reconciliation sees a consistent view regardless
  // of whether the call started at offset 0 or partway through.
  const fileSessionId = deriveFileSessionId(options);
  const evidence = newEvidence(fileSessionId);
  const toolResultCounters = new Map<string, number>();
  let nextEventIndex = 0;
  let prescanLastAssistantMid: string | undefined;
  if (startOffset > 0) {
    const prescan = await prescanNodes(
      filePath,
      startOffset,
      nodesByUuid,
      evidence,
      toolResultCounters,
    );
    prescanLastAssistantMid = prescan.lastAssistantMessageId;
    nextEventIndex = prescan.nextEventIndex;
  }
  const messageIdFirstOffset = new Map<string, number>();
  const userTextByMessageId = new Map<string, string>();
  const erroredToolUseIds = new Set<string>();
  const replacementMetaByToolUseId = new Map<string, ReplacementMeta>();
  const events: Array<{ offset: number; event: CompactionEvent }> = [];
  // Seeded from the prescan so user turns whose preceding assistant turn
  // lives before `startOffset` still get a `precedingMessageId`.
  let lastAssistantMessageId: string | undefined = prescanLastAssistantMid;
  // Seed from the prior call so an in-progress turn whose user prompt lives
  // before `startOffset` still classifies against that prompt on resume.
  let currentUserText = options.lastUserText ?? '';
  // User content tagged with the byte offset of its line so we can (a) drop
  // records past endOffset and (b) interleave them with assistant content by
  // source-order at emit time.
  const pendingUserContent: Array<{ offset: number; record: ContentRecord }> = [];
  // Execution graph — same endOffset-deferred shape as content/events.
  // Tool-result events are tagged with the offset of the user line that
  // carried them so we can drop any past endOffset on this pass and re-emit
  // them when the next call resumes from there.
  const pendingToolResultEvents: Array<{
    offset: number;
    record: ToolResultEventRecord;
  }> = [];
  const pendingRelationships: Array<{
    offset: number;
    record: SessionRelationshipRecord;
  }> = [];
  const seenRootSessionIds = new Set<string>();
  const seenExplicitRelationshipIds = new Set<string>();
  // Per-user-turn records tagged with their line offset so we can drop any
  // that fall past endOffset (avoiding double-emission on resume), mirroring
  // the content/event handling.
  const pendingUserTurns: Array<{ offset: number; record: UserTurnRecord }> = [];
  let pendingUserTurnInc: UserTurnRecord | undefined;
  // Offset of the line that first set `evidence.hasResumeMarker` in this pass.
  // -1 means "set by the prescan" (i.e. before startOffset, definitely emit).
  // A non-negative value lets us defer the local continuation row when the
  // resume marker fell past `endOffset` (resumed pass would re-read the line
  // and re-emit otherwise).
  let resumeMarkerOffset = evidence.hasResumeMarker ? -1 : Number.POSITIVE_INFINITY;

  let p = 0;
  let cursorOffset = startOffset; // position just past the last complete \n
  while (p < buf.length) {
    const nlIdx = buf.indexOf(0x0a, p);
    if (nlIdx === -1) break;
    const lineStartOffset = startOffset + p;
    const lineEndOffset = startOffset + nlIdx + 1;
    const text = buf.subarray(p, nlIdx).toString('utf8').trim();
    p = nlIdx + 1;
    cursorOffset = lineEndOffset;
    if (!text) continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      continue;
    }
    if (!parsed || typeof parsed !== 'object') continue;
    const rec = parsed as Record<string, unknown>;
    if (rec.type === 'assistant') {
      const line = rec as unknown as AssistantLine;
      const msgId =
        line.message && typeof line.message.id === 'string' ? line.message.id : undefined;
      // Link any pending user-turn record to this assistant turn — first time
      // we see a new messageId only, so multi-line assistant messages don't
      // shadow the link.
      if (msgId && pendingUserTurnInc && !messageIdFirstOffset.has(msgId)) {
        pendingUserTurnInc.followingMessageId = msgId;
        pendingUserTurnInc = undefined;
      }
      if (msgId && !messageIdFirstOffset.has(msgId)) {
        messageIdFirstOffset.set(msgId, lineStartOffset);
      }
      if (msgId && !userTextByMessageId.has(msgId)) {
        userTextByMessageId.set(msgId, currentUserText);
      }
      if (msgId) lastAssistantMessageId = msgId;
      if (typeof line.sessionId === 'string' && line.sessionId.length > 0) {
        recordRootIncremental(
          pendingRelationships,
          seenRootSessionIds,
          line.sessionId,
          line.timestamp,
          lineStartOffset,
          fileSessionId,
        );
        collectExplicitClaudeRelationshipsIncremental(
          rec,
          evidence,
          pendingRelationships,
          seenExplicitRelationshipIds,
          fileSessionId ?? line.sessionId,
          line.timestamp,
          lineStartOffset,
        );
      }
      recordEvidenceFromLine(evidence, line);
      ingestAssistant(line, working, order, nodesByUuid);
    } else if (rec.type === 'user') {
      const ul = rec as unknown as UserLine;
      registerUserNode(ul, nodesByUuid);
      const prompt = extractPlainUserText(ul);
      if (prompt) currentUserText = prompt;
      collectErroredToolUseIds(ul, erroredToolUseIds);
      collectReplacementMeta(ul, replacementMetaByToolUseId);
      if (typeof ul.sessionId === 'string' && ul.sessionId.length > 0) {
        recordRootIncremental(
          pendingRelationships,
          seenRootSessionIds,
          ul.sessionId,
          ul.timestamp,
          lineStartOffset,
          fileSessionId,
        );
        collectExplicitClaudeRelationshipsIncremental(
          rec,
          evidence,
          pendingRelationships,
          seenExplicitRelationshipIds,
          fileSessionId ?? ul.sessionId,
          ul.timestamp,
          lineStartOffset,
        );
      }
      recordEvidenceFromLine(evidence, ul);
      const hadResumeBefore = evidence.hasResumeMarker;
      recordResumeMarker(evidence, ul);
      if (!hadResumeBefore && evidence.hasResumeMarker) {
        resumeMarkerOffset = lineStartOffset;
      }
      const harvested: ToolResultEventRecord[] = [];
      nextEventIndex = collectToolResultEvents(
        ul,
        harvested,
        toolResultCounters,
        nextEventIndex,
      );
      for (const ev of harvested) {
        pendingToolResultEvents.push({ offset: lineStartOffset, record: ev });
      }
      const userTurn = buildUserTurnRecord(ul, lastAssistantMessageId, tokenCounter);
      if (userTurn) {
        pendingUserTurns.push({ offset: lineStartOffset, record: userTurn });
        pendingUserTurnInc = userTurn;
      }
      if (captureContent) {
        for (const c of extractUserContent(ul)) {
          pendingUserContent.push({ offset: lineStartOffset, record: c });
        }
      }
    } else if (rec.type === 'system') {
      if (rec['subtype'] === 'compact_boundary') {
        const sl = rec as { sessionId?: string; timestamp?: string };
        const sessionId = sl.sessionId ?? '';
        const ts = sl.timestamp ?? '';
        if (sessionId) {
          const ev: CompactionEvent = {
            v: 1,
            source: 'claude-code',
            sessionId,
            ts,
          };
          if (lastAssistantMessageId) ev.precedingMessageId = lastAssistantMessageId;
          events.push({ offset: lineStartOffset, event: ev });
        }
      }
      const systemEvent = buildClaudeSystemToolResultEvent(
        rec,
        toolResultCounters,
        nextEventIndex,
      );
      if (systemEvent) {
        pendingToolResultEvents.push({ offset: lineStartOffset, record: systemEvent });
        nextEventIndex++;
      }
    }
  }

  // Determine end offset: the byte position of the earliest in-progress messageId,
  // or `cursorOffset` (= pos after last complete newline) if all messages are complete.
  let earliestIncompleteOffset: number | undefined;
  for (const id of order) {
    const w = working.get(id);
    if (!w) continue;
    if (w.stopReason === undefined) {
      const firstOff = messageIdFirstOffset.get(id);
      if (firstOff !== undefined) {
        if (earliestIncompleteOffset === undefined || firstOff < earliestIncompleteOffset) {
          earliestIncompleteOffset = firstOff;
        }
      }
    }
  }
  const endOffset = earliestIncompleteOffset ?? cursorOffset;

  const turns: TurnRecord[] = [];
  const assistantPending: Array<{ offset: number; sub: number; record: ContentRecord }> = [];
  for (let i = 0; i < order.length; i++) {
    const id = order[i]!;
    const w = working.get(id);
    if (!w) continue;
    if (w.stopReason === undefined) continue; // defer in-progress messages
    const toolCalls = extractToolCalls(w.blocks, erroredToolUseIds, replacementMetaByToolUseId);
    const filesTouched = extractFilesTouched(toolCalls);
    const subagent = resolveSubagent(w, nodesByUuid, invocationCache);
    const record: TurnRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: w.sessionId,
      messageId: w.messageId,
      turnIndex: i,
      ts: w.firstTs,
      model: w.model,
      usage: w.usage,
      toolCalls,
    };
    if (options.sessionPath !== undefined) record.sessionPath = options.sessionPath;
    if (w.cwd !== undefined) {
      const resolved = resolveProject(w.cwd);
      record.project = resolved.project;
      if (resolved.projectKey !== undefined) record.projectKey = resolved.projectKey;
    }
    if (filesTouched.length > 0) record.filesTouched = filesTouched;
    if (subagent) record.subagent = subagent;
    record.stopReason = w.stopReason;
    record.fidelity = buildClaudeFidelity(w.usageCoverage);
    applyClassification(record, w, userTextByMessageId, erroredToolUseIds);
    turns.push(record);
    if (captureContent) {
      const msgOffset = messageIdFirstOffset.get(w.messageId) ?? 0;
      extractAssistantContent(w).forEach((r, sub) => {
        assistantPending.push({ offset: msgOffset, sub: sub + 1, record: r });
      });
    }
  }

  let content: ContentRecord[] = [];
  if (captureContent) {
    const merged: Array<{ offset: number; sub: number; record: ContentRecord }> = [];
    for (const u of pendingUserContent) {
      if (u.offset < endOffset) merged.push({ offset: u.offset, sub: 0, record: u.record });
    }
    // Filter assistant content by the same endOffset boundary. TurnRecords
    // past endOffset are still emitted (appendTurns dedups by messageId), but
    // appendContent has no dedup, so content emitted past endOffset would be
    // re-emitted and duplicated when the next incremental call resumes from
    // endOffset and re-processes the same bytes.
    for (const a of assistantPending) {
      if (a.offset < endOffset) merged.push(a);
    }
    merged.sort((a, b) => a.offset - b.offset || a.sub - b.sub);
    content = merged.map((m) => m.record);
  }

  // Only emit compaction events whose bytes fall before endOffset — mirrors
  // the content-dedup rule. appendCompactions does its own dedup by id hash,
  // but we still don't want to emit an event past endOffset and then re-emit
  // it on the next incremental pass.
  const emittedEvents: CompactionEvent[] = [];
  for (const e of events) {
    if (e.offset < endOffset) emittedEvents.push(e.event);
  }
  annotateCompactionEvents(emittedEvents, turns);

  // Execution graph. Mirror the same endOffset-defer rule so we don't
  // double-emit on resume. We only annotate spawn agentIds onto events that
  // were actually emitted.
  const emittedRelationships: SessionRelationshipRecord[] = [];
  for (const r of pendingRelationships) {
    if (r.offset < endOffset) emittedRelationships.push(r.record);
  }
  // Subagent rows are derived from the turns we just emitted in this pass —
  // they share an offset boundary with their parent assistant line, so the
  // turn-level endOffset filter already handled it.
  collectSubagentRelationships(turns, emittedRelationships);
  // Local /resume continuation. Only emit when the marker line was
  // before endOffset so a resumed pass can't double-emit. The dedup index
  // would catch the duplicate at write time, but suppressing here keeps the
  // contract symmetric with how content / event rows are handled.
  if (resumeMarkerOffset < endOffset) {
    emitLocalContinuationFromResume(emittedRelationships, evidence);
  }
  // Provenance stamp goes last so every relationship row this pass emits
  // (root, subagent, continuation) gets `sourceSessionId` / `sourceVersion`
  // populated when the evidence carries them.
  annotateRelationshipsWithEvidence(emittedRelationships, evidence);
  const emittedToolResultEvents: ToolResultEventRecord[] = [];
  for (const ev of pendingToolResultEvents) {
    if (ev.offset < endOffset) emittedToolResultEvents.push(ev.record);
  }
  annotateSpawnEvents(emittedToolResultEvents, turns);

  // Emit user turns whose bytes fall before endOffset, same dedup discipline
  // as content/events. Trailing user turns past endOffset will be re-read on
  // the next incremental call.
  const emittedUserTurns: UserTurnRecord[] = [];
  for (const u of pendingUserTurns) {
    if (u.offset < endOffset) emittedUserTurns.push(u.record);
  }

  return {
    turns,
    content,
    events: emittedEvents,
    relationships: emittedRelationships,
    toolResultEvents: emittedToolResultEvents,
    endOffset,
    lastUserText: currentUserText,
    userTurns: emittedUserTurns,
    evidence,
  };
}

function recordRootIncremental(
  out: Array<{ offset: number; record: SessionRelationshipRecord }>,
  seen: Set<string>,
  sessionId: string,
  ts: string | undefined,
  offset: number,
  fileSessionId: string | undefined,
): void {
  const canonical = fileSessionId ?? sessionId;
  if (seen.has(canonical)) return;
  seen.add(canonical);
  const row: SessionRelationshipRecord = {
    v: 1,
    source: 'claude-code',
    sessionId: canonical,
    relationshipType: 'root',
  };
  if (typeof ts === 'string' && ts.length > 0) row.ts = ts;
  out.push({ offset, record: row });
}
