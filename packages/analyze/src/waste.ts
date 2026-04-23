import type { ContentRecord, ToolCall, TurnRecord } from '@relayburn/reader';

import type { ModelCost, PricingTable } from './pricing.js';

const PER_MILLION = 1_000_000;
const CHARS_PER_TOKEN = 4;

export interface ToolAttribution {
  toolUseId: string;
  toolName: string;
  target: string | undefined;
  argsHash: string;
  sessionId: string;
  emitTurnIndex: number;
  emitTs: string;
  model: string;
  project: string | undefined;
  projectKey: string | undefined;
  subagentType: string | undefined;
  resultTokens: number;
  resultBytesEstimated: boolean;
  initialCost: number;
  initialTokens: number;
  persistenceCost: number;
  persistenceTokens: number;
  ridingTurns: number;
  totalCost: number;
}

export interface SessionWasteTotals {
  sessionId: string;
  grandCost: number;
  attributedCost: number;
  unattributedCost: number;
  attributionMethod: 'sized' | 'even-split';
}

export interface WasteResult {
  attributions: ToolAttribution[];
  sessionTotals: SessionWasteTotals[];
  grandTotal: number;
  attributedTotal: number;
  unattributedTotal: number;
}

export interface AttributeWasteOptions {
  pricing: PricingTable;
  // sessionId -> ContentRecord[] in source order
  contentBySession?: Map<string, ContentRecord[]>;
}

interface PerTurnContent {
  // tool_result text by toolUseId for this turn's user message
  toolResultText: Map<string, string>;
}

export function attributeWaste(
  turns: TurnRecord[],
  opts: AttributeWasteOptions,
): WasteResult {
  const { pricing, contentBySession } = opts;
  const bySession = new Map<string, TurnRecord[]>();
  for (const t of turns) {
    let list = bySession.get(t.sessionId);
    if (!list) {
      list = [];
      bySession.set(t.sessionId, list);
    }
    list.push(t);
  }

  const attributions: ToolAttribution[] = [];
  const sessionTotals: SessionWasteTotals[] = [];
  let grandTotal = 0;
  let attributedTotal = 0;

  for (const [sessionId, sessionTurns] of bySession) {
    sessionTurns.sort((a, b) => a.turnIndex - b.turnIndex);

    const sessionContent = contentBySession?.get(sessionId);
    const toolResultsByTurnTs = sessionContent
      ? indexToolResults(sessionContent, sessionTurns)
      : null;

    const sessionResult = attributeSession(
      sessionTurns,
      pricing,
      toolResultsByTurnTs,
    );

    let sessionGrand = 0;
    for (const t of sessionTurns) {
      const cost = costForTurnLocal(t, pricing);
      if (cost !== null) sessionGrand += cost;
    }

    let sessionAttributed = 0;
    for (const a of sessionResult.attributions) {
      sessionAttributed += a.totalCost;
    }
    const sessionUnattributed = sessionGrand - sessionAttributed;

    attributions.push(...sessionResult.attributions);
    sessionTotals.push({
      sessionId,
      grandCost: sessionGrand,
      attributedCost: sessionAttributed,
      unattributedCost: sessionUnattributed,
      attributionMethod: sessionResult.method,
    });
    grandTotal += sessionGrand;
    attributedTotal += sessionAttributed;
  }

  return {
    attributions,
    sessionTotals,
    grandTotal,
    attributedTotal,
    unattributedTotal: grandTotal - attributedTotal,
  };
}

interface SessionAttribution {
  attributions: ToolAttribution[];
  method: 'sized' | 'even-split';
}

function attributeSession(
  turns: TurnRecord[],
  pricing: PricingTable,
  toolResultsByTurnTs: Map<number, PerTurnContent> | null,
): SessionAttribution {
  if (turns.length === 0) return { attributions: [], method: 'even-split' };

  // Build tool-result size index by toolUseId across the full session.
  // Tool results land in the user turn that follows the assistant turn that
  // emitted the tool_use; we index ALL of them and look up by toolUseId.
  const sizeByToolUseId = new Map<string, number>();
  if (toolResultsByTurnTs) {
    for (const perTurn of toolResultsByTurnTs.values()) {
      for (const [toolUseId, text] of perTurn.toolResultText) {
        const tokens = estimateTokens(text);
        sizeByToolUseId.set(toolUseId, tokens);
      }
    }
  }

  const haveAnySizes = sizeByToolUseId.size > 0;
  const method: 'sized' | 'even-split' = haveAnySizes ? 'sized' : 'even-split';

  const attributions: ToolAttribution[] = [];

  for (let i = 0; i < turns.length; i++) {
    const turn = turns[i]!;
    if (turn.toolCalls.length === 0) continue;
    const rate = lookupRate(turn.model, pricing);
    if (!rate) continue;

    const next = turns[i + 1];
    const tail = turns.slice(i + 2);

    if (haveAnySizes) {
      attributeToolCallsSized(
        turn,
        next,
        tail,
        sizeByToolUseId,
        rate,
        attributions,
      );
    } else {
      attributeToolCallsEvenSplit(
        turn,
        next,
        tail,
        rate,
        attributions,
      );
    }
  }

  return { attributions, method };
}

function attributeToolCallsSized(
  turn: TurnRecord,
  next: TurnRecord | undefined,
  tail: TurnRecord[],
  sizeByToolUseId: Map<string, number>,
  rate: ModelCost,
  attributions: ToolAttribution[],
): void {
  const subagentType = turn.subagent?.type;
  for (const tc of turn.toolCalls) {
    const sizeTokens = sizeByToolUseId.get(tc.id) ?? 0;
    const initial = computeInitialCostSized(sizeTokens, next, rate);
    const persistence = computePersistenceCostSized(sizeTokens, tail, turn.model, attributions, rate);

    const total = initial.cost + persistence.cost;
    attributions.push({
      toolUseId: tc.id,
      toolName: tc.name,
      target: tc.target,
      argsHash: tc.argsHash,
      sessionId: turn.sessionId,
      emitTurnIndex: turn.turnIndex,
      emitTs: turn.ts,
      model: turn.model,
      project: turn.project,
      projectKey: turn.projectKey,
      subagentType: tc.name === 'Agent' || tc.name === 'Task' ? subagentType ?? tc.target : undefined,
      resultTokens: sizeTokens,
      resultBytesEstimated: true,
      initialCost: initial.cost,
      initialTokens: initial.tokens,
      persistenceCost: persistence.cost,
      persistenceTokens: persistence.tokens,
      ridingTurns: persistence.ridingTurns,
      totalCost: total,
    });
  }
}

interface InitialResult {
  cost: number;
  tokens: number;
}

function computeInitialCostSized(
  sizeTokens: number,
  next: TurnRecord | undefined,
  rate: ModelCost,
): InitialResult {
  if (!next || sizeTokens === 0) return { cost: 0, tokens: 0 };
  // The tool_result enters context at the next turn. The next turn pays for it
  // as either fresh `input` or `cacheCreate` (depending on the prefix-cache
  // boundary the SDK chose). We use the next turn's actual mix to weight.
  const newContent = next.usage.input + next.usage.cacheCreate5m + next.usage.cacheCreate1h;
  if (newContent === 0) return { cost: 0, tokens: 0 };
  const tokens = Math.min(sizeTokens, newContent);
  const inputShare = newContent === 0 ? 1 : next.usage.input / newContent;
  const createShare = 1 - inputShare;
  const perTokenPrice = inputShare * rate.input + createShare * rate.cacheWrite;
  return { cost: (tokens / PER_MILLION) * perTokenPrice, tokens };
}

interface PersistenceResult {
  cost: number;
  tokens: number;
  ridingTurns: number;
}

function computePersistenceCostSized(
  sizeTokens: number,
  tail: TurnRecord[],
  emitModel: string,
  _attributions: ToolAttribution[],
  rate: ModelCost,
): PersistenceResult {
  if (sizeTokens === 0 || tail.length === 0) {
    return { cost: 0, tokens: 0, ridingTurns: 0 };
  }
  let cost = 0;
  let tokens = 0;
  let ridingTurns = 0;
  for (const t of tail) {
    // If cacheRead drops below the result's size, the content has aged out.
    // We use this as a conservative eviction signal.
    if (t.usage.cacheRead < sizeTokens) break;
    const turnRate = t.model === emitModel ? rate : null;
    const cacheReadPrice = turnRate ? turnRate.cacheRead : rate.cacheRead;
    cost += (sizeTokens / PER_MILLION) * cacheReadPrice;
    tokens += sizeTokens;
    ridingTurns++;
  }
  return { cost, tokens, ridingTurns };
}

function attributeToolCallsEvenSplit(
  turn: TurnRecord,
  next: TurnRecord | undefined,
  tail: TurnRecord[],
  rate: ModelCost,
  attributions: ToolAttribution[],
): void {
  const subagentType = turn.subagent?.type;
  const k = turn.toolCalls.length;
  // Initial: even-split next turn's (input + cacheCreate) cost across this
  // turn's tool calls. This matches the per-tool-name behavior of by-tool but
  // attributes per tool_use_id so we can group by file/argsHash later.
  let initialPerCall = 0;
  let initialTokensPerCall = 0;
  if (next && k > 0) {
    const inputCost = (next.usage.input / PER_MILLION) * rate.input;
    const createCost =
      ((next.usage.cacheCreate5m + next.usage.cacheCreate1h) / PER_MILLION) * rate.cacheWrite;
    initialPerCall = (inputCost + createCost) / k;
    initialTokensPerCall =
      (next.usage.input + next.usage.cacheCreate5m + next.usage.cacheCreate1h) / k;
  }

  // Persistence in even-split mode is approximate. We don't know the
  // per-tool-result size, so we attribute this turn's cacheRead share equally
  // across all tool calls observed so far. To keep totals additive, each tool
  // call gets `cacheRead_T / total_in_flight_at_T` per ride-along turn. But
  // computing total_in_flight requires a session-wide pass — we do it after.
  // For now record placeholder zeros; even-split persistence is filled in by
  // the caller using a second pass. To avoid that complexity we DO a second
  // pass right here when we see the full attributions list.

  for (const tc of turn.toolCalls) {
    attributions.push({
      toolUseId: tc.id,
      toolName: tc.name,
      target: tc.target,
      argsHash: tc.argsHash,
      sessionId: turn.sessionId,
      emitTurnIndex: turn.turnIndex,
      emitTs: turn.ts,
      model: turn.model,
      project: turn.project,
      projectKey: turn.projectKey,
      subagentType:
        tc.name === 'Agent' || tc.name === 'Task' ? subagentType ?? tc.target : undefined,
      resultTokens: 0,
      resultBytesEstimated: false,
      initialCost: initialPerCall,
      initialTokens: initialTokensPerCall,
      // Persistence is not allocated in even-split mode (would over-attribute
      // baseline overhead like system prompt and CLAUDE.md). Initial only is
      // a strict subset of by-tool's totals.
      persistenceCost: 0,
      persistenceTokens: 0,
      ridingTurns: 0,
      totalCost: initialPerCall,
    });
  }

  // tail unused in even-split mode (see comment above)
  void tail;
}

function indexToolResults(
  content: ContentRecord[],
  turns: TurnRecord[],
): Map<number, PerTurnContent> {
  // Group tool_results by their nearest preceding assistant message in
  // chronological order. We bucket by the assistant turn's turnIndex so we can
  // associate results with the tool_uses that requested them.
  const byTurn = new Map<number, PerTurnContent>();

  // Build a simple map from toolUseId -> assistant turnIndex.
  const turnIndexByToolUseId = new Map<string, number>();
  for (const t of turns) {
    for (const tc of t.toolCalls) {
      turnIndexByToolUseId.set(tc.id, t.turnIndex);
    }
  }

  for (const c of content) {
    if (c.kind !== 'tool_result' || !c.toolResult) continue;
    const idx = turnIndexByToolUseId.get(c.toolResult.toolUseId);
    if (idx === undefined) continue;
    let bucket = byTurn.get(idx);
    if (!bucket) {
      bucket = { toolResultText: new Map() };
      byTurn.set(idx, bucket);
    }
    const text = stringifyToolResult(c.toolResult.content);
    bucket.toolResultText.set(c.toolResult.toolUseId, text);
  }
  return byTurn;
}

function stringifyToolResult(content: unknown): string {
  if (typeof content === 'string') return content;
  if (content === null || content === undefined) return '';
  if (Array.isArray(content)) {
    const parts: string[] = [];
    for (const block of content) {
      if (block && typeof block === 'object') {
        const b = block as { type?: string; text?: string };
        if (b.type === 'text' && typeof b.text === 'string') {
          parts.push(b.text);
        } else {
          parts.push(JSON.stringify(block));
        }
      } else if (typeof block === 'string') {
        parts.push(block);
      }
    }
    return parts.join('\n');
  }
  return JSON.stringify(content);
}

function estimateTokens(text: string): number {
  // Standard chars-per-token heuristic. Real tokenizers vary, but Anthropic's
  // BPE averages ~3.5-4 chars/token for English. We use 4 to stay slightly
  // conservative (under-estimate slightly rather than over-attribute cost).
  return Math.max(0, Math.ceil(text.length / CHARS_PER_TOKEN));
}

function lookupRate(model: string, pricing: PricingTable): ModelCost | undefined {
  const direct = pricing[model];
  if (direct) return direct;
  const i = model.indexOf('/');
  if (i >= 0) {
    const stripped = pricing[model.slice(i + 1)];
    if (stripped) return stripped;
  }
  return undefined;
}

function costForTurnLocal(turn: TurnRecord, pricing: PricingTable): number | null {
  const rate = lookupRate(turn.model, pricing);
  if (!rate) return null;
  const u = turn.usage;
  return (
    (u.input / PER_MILLION) * rate.input +
    (u.output / PER_MILLION) * rate.output +
    (u.reasoning / PER_MILLION) * rate.output +
    (u.cacheRead / PER_MILLION) * rate.cacheRead +
    ((u.cacheCreate5m + u.cacheCreate1h) / PER_MILLION) * rate.cacheWrite
  );
}

export interface FileAggregation {
  path: string;
  toolCallCount: number;
  initialTokens: number;
  persistenceTokens: number;
  ridingTurns: number;
  totalCost: number;
  firstEmitTs: string;
  firstEmitTurnIndex: number;
}

export interface BashAggregation {
  argsHash: string;
  command: string | undefined;
  callCount: number;
  totalCost: number;
  initialTokens: number;
  persistenceTokens: number;
}

export interface SubagentAggregation {
  subagentType: string;
  callCount: number;
  totalCost: number;
  initialTokens: number;
  persistenceTokens: number;
}

const FILE_TOOLS = new Set(['Read', 'Edit', 'Write', 'NotebookEdit']);

export function aggregateByFile(attributions: ToolAttribution[]): FileAggregation[] {
  const byPath = new Map<string, FileAggregation>();
  for (const a of attributions) {
    if (!FILE_TOOLS.has(a.toolName) || !a.target) continue;
    let row = byPath.get(a.target);
    if (!row) {
      row = {
        path: a.target,
        toolCallCount: 0,
        initialTokens: 0,
        persistenceTokens: 0,
        ridingTurns: 0,
        totalCost: 0,
        firstEmitTs: a.emitTs,
        firstEmitTurnIndex: a.emitTurnIndex,
      };
      byPath.set(a.target, row);
    }
    row.toolCallCount++;
    row.initialTokens += a.initialTokens;
    row.persistenceTokens += a.persistenceTokens;
    row.ridingTurns += a.ridingTurns;
    row.totalCost += a.totalCost;
    if (a.emitTs < row.firstEmitTs) {
      row.firstEmitTs = a.emitTs;
      row.firstEmitTurnIndex = a.emitTurnIndex;
    }
  }
  return [...byPath.values()].sort((a, b) => b.totalCost - a.totalCost);
}

export function aggregateByBash(attributions: ToolAttribution[]): BashAggregation[] {
  const byHash = new Map<string, BashAggregation>();
  for (const a of attributions) {
    if (a.toolName !== 'Bash') continue;
    let row = byHash.get(a.argsHash);
    if (!row) {
      row = {
        argsHash: a.argsHash,
        command: a.target,
        callCount: 0,
        totalCost: 0,
        initialTokens: 0,
        persistenceTokens: 0,
      };
      byHash.set(a.argsHash, row);
    }
    row.callCount++;
    row.totalCost += a.totalCost;
    row.initialTokens += a.initialTokens;
    row.persistenceTokens += a.persistenceTokens;
  }
  return [...byHash.values()].sort((a, b) => b.totalCost - a.totalCost);
}

export function aggregateBySubagent(attributions: ToolAttribution[]): SubagentAggregation[] {
  const byType = new Map<string, SubagentAggregation>();
  for (const a of attributions) {
    if (a.toolName !== 'Agent' && a.toolName !== 'Task') continue;
    const key = a.subagentType ?? '(unknown)';
    let row = byType.get(key);
    if (!row) {
      row = {
        subagentType: key,
        callCount: 0,
        totalCost: 0,
        initialTokens: 0,
        persistenceTokens: 0,
      };
      byType.set(key, row);
    }
    row.callCount++;
    row.totalCost += a.totalCost;
    row.initialTokens += a.initialTokens;
    row.persistenceTokens += a.persistenceTokens;
  }
  return [...byType.values()].sort((a, b) => b.totalCost - a.totalCost);
}
