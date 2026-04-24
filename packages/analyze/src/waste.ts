import type { ContentRecord, TurnRecord } from '@relayburn/reader';

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
  const sizeByToolUseId = new Map<string, number>();
  if (toolResultsByTurnTs) {
    for (const perTurn of toolResultsByTurnTs.values()) {
      for (const [toolUseId, text] of perTurn.toolResultText) {
        sizeByToolUseId.set(toolUseId, estimateTokens(text));
      }
    }
  }

  const haveAnySizes = sizeByToolUseId.size > 0;
  const method: 'sized' | 'even-split' = haveAnySizes ? 'sized' : 'even-split';

  const attributions: ToolAttribution[] = [];
  // Attributions emitted at the immediately-prior turn that have not yet been
  // charged initial cost. They will be charged at this iteration using the
  // current (paying) turn's model rate and (input + cacheCreate) mix.
  let pendingInitial: ToolAttribution[] = [];
  // Attributions whose initial cost has already been paid; eligible to ride
  // along (persistence) on subsequent turns until the cacheRead eviction
  // signal drops them.
  const ridingActive: ToolAttribution[] = [];

  for (const turn of turns) {
    const turnRate = lookupRate(turn.model, pricing);

    // 1) Initial cost: this turn pays for tool_results emitted on the previous
    //    turn (they enter context now as fresh `input` and/or `cacheCreate`).
    //    Use THIS turn's rate and (input/cacheCreate) mix — not the emit turn's.
    if (pendingInitial.length > 0 && turnRate) {
      const newContent =
        turn.usage.input + turn.usage.cacheCreate5m + turn.usage.cacheCreate1h;
      if (newContent > 0) {
        const inputShare = turn.usage.input / newContent;
        const createShare = 1 - inputShare;
        const perTokenPrice = inputShare * turnRate.input + createShare * turnRate.cacheWrite;
        if (haveAnySizes) {
          const siblingTotal = pendingInitial.reduce((s, a) => s + a.resultTokens, 0);
          if (siblingTotal > 0) {
            // Cap the sibling group at what turn N+1 actually paid for new
            // content — otherwise multiple tool_results entering on the same
            // turn could over-attribute past the actual paid total.
            const cap = Math.min(siblingTotal, newContent);
            for (const a of pendingInitial) {
              const tokens = (a.resultTokens / siblingTotal) * cap;
              const cost = (tokens / PER_MILLION) * perTokenPrice;
              a.initialCost = cost;
              a.initialTokens = tokens;
              a.totalCost += cost;
            }
          }
        } else {
          // Even-split mode: with no per-result sizes, divide this turn's
          // (input + cacheCreate) cost evenly across the prior emit's tool
          // calls. This is a strict subset of by-tool's totals.
          const k = pendingInitial.length;
          const tokensPerCall = newContent / k;
          const costPerCall =
            ((turn.usage.input / PER_MILLION) * turnRate.input +
              ((turn.usage.cacheCreate5m + turn.usage.cacheCreate1h) / PER_MILLION) *
                turnRate.cacheWrite) /
            k;
          for (const a of pendingInitial) {
            a.initialTokens = tokensPerCall;
            a.initialCost = costPerCall;
            a.totalCost += costPerCall;
          }
        }
      }
    }

    // 2) Persistence cost: every still-cached prior tool_result rides along
    //    in this turn's cacheRead. Allocate cacheRead proportionally by size
    //    so the sum across active results never exceeds the actual cacheRead
    //    tokens. Eviction signal: a result is dropped from the active set
    //    once the turn's cacheRead falls below that single result's size.
    if (haveAnySizes && ridingActive.length > 0 && turnRate && turn.usage.cacheRead > 0) {
      const stillCached: ToolAttribution[] = [];
      for (const a of ridingActive) {
        if (a.resultTokens > 0 && turn.usage.cacheRead >= a.resultTokens) {
          stillCached.push(a);
        }
      }
      if (stillCached.length > 0) {
        const activeTotal = stillCached.reduce((s, a) => s + a.resultTokens, 0);
        const allocatable = Math.min(turn.usage.cacheRead, activeTotal);
        for (const a of stillCached) {
          const tokens = (a.resultTokens / activeTotal) * allocatable;
          const cost = (tokens / PER_MILLION) * turnRate.cacheRead;
          a.persistenceTokens += tokens;
          a.persistenceCost += cost;
          a.totalCost += cost;
          a.ridingTurns += 1;
        }
      }
    }

    // 3) Promote yesterday's pendingInitial into the riding-active set, then
    //    emit attributions for this turn's own tool_uses (they'll pay initial
    //    next iteration).
    if (pendingInitial.length > 0) {
      ridingActive.push(...pendingInitial);
      pendingInitial = [];
    }
    if (turn.toolCalls.length > 0) {
      for (const tc of turn.toolCalls) {
        const sizeTokens = sizeByToolUseId.get(tc.id) ?? 0;
        const a: ToolAttribution = {
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
          // For Agent/Task spawns, identify the *spawned* subagent. The
          // spawning tool call's own input carries `subagent_type`, which
          // `pickTarget` already resolves into `tc.target`. Don't reach for
          // `turn.subagent` here — that describes the invocation this turn
          // belongs to (the parent), not what it's spawning.
          subagentType:
            tc.name === 'Agent' || tc.name === 'Task' ? tc.target : undefined,
          resultTokens: sizeTokens,
          resultBytesEstimated: haveAnySizes,
          initialCost: 0,
          initialTokens: 0,
          persistenceCost: 0,
          persistenceTokens: 0,
          ridingTurns: 0,
          totalCost: 0,
        };
        attributions.push(a);
        pendingInitial.push(a);
      }
    }
  }

  return { attributions, method };
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
