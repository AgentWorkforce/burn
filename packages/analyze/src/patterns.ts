import type { CompactionEvent, ToolCall, TurnRecord, UserTurnRecord } from '@relayburn/reader';

import { costForTurn, costForUsage } from './cost.js';
import type { PricingTable } from './pricing.js';

// Retry loops: ≥ 3 strictly-consecutive tool calls that share (toolName,
// argsHash) and all carry tool_result.is_error=true. The flattened tool-call
// stream is what matters — a different tool (or different args) interleaved
// between two candidates breaks the streak, even within the same turn.
export interface RetryLoop {
  sessionId: string;
  tool: string;
  target: string | undefined;
  argsHash: string;
  attempts: number;
  startTurnIndex: number;
  endTurnIndex: number;
  // Sum of per-turn cost across every turn that contributed a retry call.
  // A turn that contributes the retry AND some other work will count once —
  // the blast radius of a retry-bearing turn isn't decomposable without
  // content-level attribution.
  cost: number;
}

// Consecutive tool failures: ≥ 3 consecutive errored tool results using
// DISTINCT (toolName, argsHash) keys. Same-key streaks are retry loops and
// emit separately; this detector catches "agent is stuck" where it tries
// different things and everything fails.
export interface FailureRun {
  sessionId: string;
  length: number;
  startTurnIndex: number;
  endTurnIndex: number;
  toolsInvolved: string[];
  cost: number;
}

export interface CompactionLoss {
  sessionId: string;
  ts: string;
  precedingMessageId: string | undefined;
  tokensBeforeCompact: number;
  // Cost of the cacheRead that was dead-weight on the pre-compaction turn.
  // Priced at the preceding turn's model rate, which is the rate at which the
  // user paid to carry that cache on the last turn before it evaporated.
  cacheLostCost: number;
}

export interface EditRevertCycle {
  sessionId: string;
  filePath: string;
  firstEditTurnIndex: number;
  revertTurnIndex: number;
  spanTurns: number;
  // Sum of per-turn cost for the two anchor turns. With only ToolCall-level
  // hashes we can't cleanly separate the revert work from the rest of those
  // turns — reported as a rough upper bound, good enough for ranking.
  cost: number;
}

// OpenCode skill recall non-deduplication: the same skill({name}) is called
// N ≥ 2 times in a session. OpenCode does not deduplicate skill tool results,
// so each call re-injects the full SKILL.md content into context.
export interface SkillRecallDup {
  sessionId: string;
  skillName: string;
  callCount: number;
  firstTurnIndex: number;
  lastTurnIndex: number;
  // Sum of per-turn cost across every turn that invoked this skill.
  cost: number;
}

// OpenCode skill pruning protection: skill tool results are listed in
// PRUNE_PROTECTED_TOOLS and are never evicted during compaction. This tracks
// each skill call and how many turns it rode in the cache after being added.
// Only emitted for source === 'opencode' sessions.
export interface SkillPruningProtection {
  sessionId: string;
  skillName: string;
  invokedTurnIndex: number;
  // How many subsequent turns still carried cacheRead tokens (the skill
  // content was still cached). This is a lower bound — the content may have
  // persisted longer but we can't prove it without content-sidecar sizes.
  ridingTurns: number;
  lastCachedTurnIndex: number;
  // Sum of per-turn cost for the invoke turn plus every riding turn.
  cost: number;
}

// OpenCode system prompt / skill catalog bloat: the first turn's
// cacheCreate5m carries the entire cached prefix (system prompt + skill
// catalog + first user message). By subtracting the first user message size
// (from content sidecar / user-turn blocks), we get the fixed prefix tax
// that rides in cache on every turn. Only emitted for source === 'opencode'.
export interface SystemPromptTax {
  sessionId: string;
  firstTurnCacheCreate: number;
  firstUserMessageTokens: number;
  estimatedSystemPromptTokens: number;
  // How many turns in the session carried cacheRead (the prefix was still cached).
  ridingTurns: number;
  // Total cost of carrying the prefix across all riding turns.
  totalCost: number;
}

// Per-session rollup mentioned in the issue discussion. Downstream commands
// (`burn compare --worst`, health grading, etc.) can read this shape without
// re-running the full detector suite.
export interface SessionPatternSummary {
  sessionId: string;
  retryLoopCount: number;
  failureRunCount: number;
  consecutiveFailureMax: number;
  compactionCount: number;
  editRevertCount: number;
  skillRecallDupCount: number;
  skillPruningProtectionCount: number;
  systemPromptTaxCount: number;
  totalRetries: number;
  totalPatternCost: number;
}

export interface PatternsResult {
  retryLoops: RetryLoop[];
  failureRuns: FailureRun[];
  compactions: CompactionLoss[];
  editReverts: EditRevertCycle[];
  skillRecallDups: SkillRecallDup[];
  skillPruningProtection: SkillPruningProtection[];
  systemPromptTaxes: SystemPromptTax[];
  sessionSummaries: SessionPatternSummary[];
}

export interface DetectPatternsOptions {
  pricing: PricingTable;
  compactions?: CompactionEvent[];
  // sessionId -> UserTurnRecord[] in source order. Used to estimate the
  // first user message size for the system-prompt-tax detector.
  userTurnsBySession?: Map<string, UserTurnRecord[]> | undefined;
}

const MIN_RETRY_LEN = 3;
const MIN_FAILURE_RUN_LEN = 3;

export function detectPatterns(
  turns: TurnRecord[],
  opts: DetectPatternsOptions,
): PatternsResult {
  const bySession = groupBySession(turns);

  const retryLoops: RetryLoop[] = [];
  const failureRuns: FailureRun[] = [];
  const editReverts: EditRevertCycle[] = [];
  const skillRecallDups: SkillRecallDup[] = [];
  const skillPruningProtection: SkillPruningProtection[] = [];
  const systemPromptTaxes: SystemPromptTax[] = [];

  for (const [sessionId, sessionTurns] of bySession) {
    sessionTurns.sort((a, b) => a.turnIndex - b.turnIndex);
    retryLoops.push(...detectRetryLoopsForSession(sessionId, sessionTurns, opts.pricing));
    failureRuns.push(
      ...detectFailureRunsForSession(sessionId, sessionTurns, opts.pricing),
    );
    editReverts.push(...detectEditRevertsForSession(sessionId, sessionTurns, opts.pricing));
    skillRecallDups.push(...detectSkillRecallDupsForSession(sessionId, sessionTurns, opts.pricing));
    skillPruningProtection.push(...detectSkillPruningProtectionForSession(sessionId, sessionTurns, opts.pricing, opts.compactions));
    systemPromptTaxes.push(...detectSystemPromptTaxForSession(sessionId, sessionTurns, opts.pricing, opts.userTurnsBySession?.get(sessionId)));
  }

  const compactions = opts.compactions
    ? detectCompactionLosses(opts.compactions, turns, opts.pricing)
    : [];

  return {
    retryLoops,
    failureRuns,
    compactions,
    editReverts,
    skillRecallDups,
    skillPruningProtection,
    systemPromptTaxes,
    sessionSummaries: buildSummaries(retryLoops, failureRuns, compactions, editReverts, skillRecallDups, skillPruningProtection, systemPromptTaxes),
  };
}

function groupBySession(turns: TurnRecord[]): Map<string, TurnRecord[]> {
  const by = new Map<string, TurnRecord[]>();
  for (const t of turns) {
    let list = by.get(t.sessionId);
    if (!list) {
      list = [];
      by.set(t.sessionId, list);
    }
    list.push(t);
  }
  return by;
}

interface ToolCallRef {
  turn: TurnRecord;
  call: ToolCall;
}

function flattenToolCalls(turns: TurnRecord[]): ToolCallRef[] {
  const out: ToolCallRef[] = [];
  for (const turn of turns) {
    for (const call of turn.toolCalls) out.push({ turn, call });
  }
  return out;
}

function sumCostForTurns(turns: TurnRecord[], pricing: PricingTable): number {
  let sum = 0;
  for (const t of turns) {
    const c = costForTurn(t, pricing);
    if (c) sum += c.total;
  }
  return sum;
}

function detectRetryLoopsForSession(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
): RetryLoop[] {
  const flat = flattenToolCalls(turns);
  const loops: RetryLoop[] = [];
  let streak: ToolCallRef[] = [];

  const commit = (): void => {
    if (streak.length < MIN_RETRY_LEN) return;
    const first = streak[0]!;
    const last = streak[streak.length - 1]!;
    const contributingTurns = dedupTurns(streak.map((r) => r.turn));
    loops.push({
      sessionId,
      tool: first.call.name,
      target: first.call.target,
      argsHash: first.call.argsHash,
      attempts: streak.length,
      startTurnIndex: first.turn.turnIndex,
      endTurnIndex: last.turn.turnIndex,
      cost: sumCostForTurns(contributingTurns, pricing),
    });
  };

  for (const ref of flat) {
    const isErrored = ref.call.isError === true;
    if (!isErrored) {
      commit();
      streak = [];
      continue;
    }
    if (streak.length === 0) {
      streak = [ref];
      continue;
    }
    const head = streak[0]!.call;
    if (head.name === ref.call.name && head.argsHash === ref.call.argsHash) {
      streak.push(ref);
    } else {
      commit();
      streak = [ref];
    }
  }
  commit();
  return loops;
}

function detectFailureRunsForSession(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
): FailureRun[] {
  const flat = flattenToolCalls(turns);
  const runs: FailureRun[] = [];
  let streak: ToolCallRef[] = [];

  const commit = (): void => {
    if (streak.length < MIN_FAILURE_RUN_LEN) return;
    const keys = new Set(streak.map((r) => `${r.call.name}|${r.call.argsHash}`));
    // A same-(tool,args) run is a retry loop, not a distinct-failure run.
    // Keep them orthogonal so the two detectors never double-report the same
    // sequence.
    if (keys.size < 2) return;
    const first = streak[0]!;
    const last = streak[streak.length - 1]!;
    const tools = Array.from(new Set(streak.map((r) => r.call.name)));
    const contributingTurns = dedupTurns(streak.map((r) => r.turn));
    runs.push({
      sessionId,
      length: streak.length,
      startTurnIndex: first.turn.turnIndex,
      endTurnIndex: last.turn.turnIndex,
      toolsInvolved: tools,
      cost: sumCostForTurns(contributingTurns, pricing),
    });
  };

  for (const ref of flat) {
    if (ref.call.isError === true) {
      streak.push(ref);
    } else {
      commit();
      streak = [];
    }
  }
  commit();
  return runs;
}

function detectEditRevertsForSession(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
): EditRevertCycle[] {
  // For each file, scan in turn order. Every edit contributes a (preHash?,
  // postHash?) and is added to that file's history. We detect a revert when
  // a later edit's postHash matches ANY earlier edit's preHash on the same
  // file — the file state has returned to a previously-visited pre-state,
  // erasing the intermediate work. We then reset the file's history: a new
  // A→B→A starting from the revert should be detectable independently.
  interface EditSlot {
    preHash: string | undefined;
    postHash: string | undefined;
    turn: TurnRecord;
  }
  const byFile = new Map<string, EditSlot[]>();
  const cycles: EditRevertCycle[] = [];

  const flat = flattenToolCalls(turns);
  for (const ref of flat) {
    const call = ref.call;
    if (!call.target) continue;
    if (call.name !== 'Edit' && call.name !== 'Write' && call.name !== 'NotebookEdit') continue;
    // Failed edits don't actually change file state — skip so a noop error
    // doesn't count as a pre/post anchor.
    if (call.isError === true) continue;
    const slot: EditSlot = {
      preHash: call.editPreHash,
      postHash: call.editPostHash,
      turn: ref.turn,
    };
    const history = byFile.get(call.target) ?? [];
    if (slot.postHash !== undefined) {
      const matchIdx = history.findIndex((prior) => prior.preHash === slot.postHash);
      if (matchIdx >= 0) {
        const first = history[matchIdx]!;
        cycles.push({
          sessionId,
          filePath: call.target,
          firstEditTurnIndex: first.turn.turnIndex,
          revertTurnIndex: ref.turn.turnIndex,
          spanTurns: ref.turn.turnIndex - first.turn.turnIndex,
          cost: sumCostForTurns(dedupTurns([first.turn, ref.turn]), pricing),
        });
        // Reset: the cycle is closed; any subsequent work on this file is a
        // fresh sequence, not part of the just-reported cycle.
        byFile.set(call.target, []);
        continue;
      }
    }
    history.push(slot);
    byFile.set(call.target, history);
  }
  return cycles;
}

function detectCompactionLosses(
  events: CompactionEvent[],
  turns: TurnRecord[],
  pricing: PricingTable,
): CompactionLoss[] {
  const turnByMessageId = new Map<string, TurnRecord>();
  for (const t of turns) turnByMessageId.set(t.messageId, t);

  const out: CompactionLoss[] = [];
  for (const e of events) {
    const tokens = e.tokensBeforeCompact ?? 0;
    let cacheLostCost = 0;
    if (tokens > 0 && e.precedingMessageId) {
      const preceding = turnByMessageId.get(e.precedingMessageId);
      if (preceding) {
        const priced = costForUsage(
          {
            input: 0,
            output: 0,
            reasoning: 0,
            cacheRead: tokens,
            cacheCreate5m: 0,
            cacheCreate1h: 0,
          },
          preceding.model,
          pricing,
        );
        if (priced) cacheLostCost = priced.total;
      }
    }
    out.push({
      sessionId: e.sessionId,
      ts: e.ts,
      precedingMessageId: e.precedingMessageId,
      tokensBeforeCompact: tokens,
      cacheLostCost,
    });
  }
  return out;
}

function detectSkillRecallDupsForSession(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
): SkillRecallDup[] {
  // Only relevant for OpenCode sessions.
  if (turns.length === 0 || turns[0]!.source !== 'opencode') return [];

  const byName = new Map<string, ToolCallRef[]>();
  const flat = flattenToolCalls(turns);
  for (const ref of flat) {
    if (ref.call.name !== 'skill' || !ref.call.skillName) continue;
    const list = byName.get(ref.call.skillName) ?? [];
    list.push(ref);
    byName.set(ref.call.skillName, list);
  }

  const out: SkillRecallDup[] = [];
  for (const [skillName, refs] of byName) {
    if (refs.length < 2) continue;
    const first = refs[0]!;
    const last = refs[refs.length - 1]!;
    const contributingTurns = dedupTurns(refs.map((r) => r.turn));
    out.push({
      sessionId,
      skillName,
      callCount: refs.length,
      firstTurnIndex: first.turn.turnIndex,
      lastTurnIndex: last.turn.turnIndex,
      cost: sumCostForTurns(contributingTurns, pricing),
    });
  }
  return out;
}

function detectSkillPruningProtectionForSession(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
  compactions: CompactionEvent[] | undefined,
): SkillPruningProtection[] {
  // Only relevant for OpenCode sessions.
  if (turns.length === 0 || turns[0]!.source !== 'opencode') return [];

  const compactionTurnIndexes = new Set<number>();
  if (compactions) {
    const turnByMessageId = new Map<string, TurnRecord>();
    for (const t of turns) turnByMessageId.set(t.messageId, t);
    for (const c of compactions) {
      if (c.precedingMessageId) {
        const preceding = turnByMessageId.get(c.precedingMessageId);
        if (preceding) compactionTurnIndexes.add(preceding.turnIndex);
      }
    }
  }

  const out: SkillPruningProtection[] = [];
  const flat = flattenToolCalls(turns);
  for (const ref of flat) {
    if (ref.call.name !== 'skill' || !ref.call.skillName) continue;
    const invokeIndex = ref.turn.turnIndex;

    // Count how many subsequent turns still carried cacheRead tokens.
    // Without content-sidecar sizes we can't prove exact eviction, but
    // any cacheRead > 0 after the invoke means the skill content was
    // still riding in the cache (it's prune-protected so it won't be
    // evicted by compaction).
    let ridingTurns = 0;
    let lastCachedTurnIndex = invokeIndex;
    let ridingCost = 0;
    for (const t of turns) {
      if (t.turnIndex <= invokeIndex) continue;
      if (t.usage.cacheRead > 0) {
        ridingTurns++;
        lastCachedTurnIndex = t.turnIndex;
        const c = costForTurn(t, pricing);
        if (c) ridingCost += c.total;
      }
    }

    if (ridingTurns === 0) continue;

    const invokeCost = (() => {
      const c = costForTurn(ref.turn, pricing);
      return c ? c.total : 0;
    })();

    out.push({
      sessionId,
      skillName: ref.call.skillName,
      invokedTurnIndex: invokeIndex,
      ridingTurns,
      lastCachedTurnIndex,
      cost: invokeCost + ridingCost,
    });
  }
  return out;
}

function detectSystemPromptTaxForSession(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
  userTurns: UserTurnRecord[] | undefined,
): SystemPromptTax[] {
  // Only relevant for OpenCode sessions.
  if (turns.length === 0 || turns[0]!.source !== 'opencode') return [];

  const firstTurn = turns[0]!;
  const firstCacheCreate = firstTurn.usage.cacheCreate5m + firstTurn.usage.cacheCreate1h;
  if (firstCacheCreate === 0) return [];

  // Estimate first user message tokens from user-turn blocks.
  // The first user turn (before the first assistant message) carries the
  // user's initial prompt. Its approxTokens is what we subtract.
  let firstUserTokens = 0;
  if (userTurns && userTurns.length > 0) {
    // The first user turn is the one with no precedingMessageId (or the
    // earliest one). Sum all blocks from the first user turn.
    const firstUserTurn = userTurns[0]!;
    for (const block of firstUserTurn.blocks) {
      firstUserTokens += block.approxTokens;
    }
  }

  // If we couldn't get user-turn data, fall back to using the first turn's
  // input tokens as a rough upper bound (includes system prompt + user msg).
  // We can't separate them, so skip the estimate rather than guess.
  if (firstUserTokens === 0) return [];

  const systemPromptTokens = Math.max(0, firstCacheCreate - firstUserTokens);
  if (systemPromptTokens === 0) return [];

  // Count how many subsequent turns carried cacheRead (the prefix was cached).
  let ridingTurns = 0;
  let totalCost = 0;
  for (const t of turns) {
    if (t.usage.cacheRead > 0) {
      ridingTurns++;
      const c = costForTurn(t, pricing);
      if (c) totalCost += c.total;
    }
  }

  if (ridingTurns === 0) return [];

  return [{
    sessionId,
    firstTurnCacheCreate: firstCacheCreate,
    firstUserMessageTokens: firstUserTokens,
    estimatedSystemPromptTokens: systemPromptTokens,
    ridingTurns,
    totalCost,
  }];
}

function dedupTurns(turns: TurnRecord[]): TurnRecord[] {
  const seen = new Set<string>();
  const out: TurnRecord[] = [];
  for (const t of turns) {
    const key = `${t.sessionId}|${t.messageId}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(t);
  }
  return out;
}

function buildSummaries(
  retryLoops: RetryLoop[],
  failureRuns: FailureRun[],
  compactions: CompactionLoss[],
  editReverts: EditRevertCycle[],
  skillRecallDups: SkillRecallDup[],
  skillPruningProtection: SkillPruningProtection[],
  systemPromptTaxes: SystemPromptTax[],
): SessionPatternSummary[] {
  const by = new Map<string, SessionPatternSummary>();
  const get = (sessionId: string): SessionPatternSummary => {
    let row = by.get(sessionId);
    if (!row) {
      row = {
        sessionId,
        retryLoopCount: 0,
        failureRunCount: 0,
        consecutiveFailureMax: 0,
        compactionCount: 0,
        editRevertCount: 0,
        skillRecallDupCount: 0,
        skillPruningProtectionCount: 0,
        systemPromptTaxCount: 0,
        totalRetries: 0,
        totalPatternCost: 0,
      };
      by.set(sessionId, row);
    }
    return row;
  };
  for (const r of retryLoops) {
    const row = get(r.sessionId);
    row.retryLoopCount++;
    row.totalRetries += r.attempts;
    row.totalPatternCost += r.cost;
  }
  for (const f of failureRuns) {
    const row = get(f.sessionId);
    row.failureRunCount++;
    if (f.length > row.consecutiveFailureMax) row.consecutiveFailureMax = f.length;
    row.totalPatternCost += f.cost;
  }
  for (const c of compactions) {
    const row = get(c.sessionId);
    row.compactionCount++;
    row.totalPatternCost += c.cacheLostCost;
  }
  for (const e of editReverts) {
    const row = get(e.sessionId);
    row.editRevertCount++;
    row.totalPatternCost += e.cost;
  }
  for (const s of skillRecallDups) {
    const row = get(s.sessionId);
    row.skillRecallDupCount++;
    row.totalPatternCost += s.cost;
  }
  for (const s of skillPruningProtection) {
    const row = get(s.sessionId);
    row.skillPruningProtectionCount++;
    row.totalPatternCost += s.cost;
  }
  for (const s of systemPromptTaxes) {
    const row = get(s.sessionId);
    row.systemPromptTaxCount++;
    row.totalPatternCost += s.totalCost;
  }
  return [...by.values()].sort((a, b) => b.totalPatternCost - a.totalPatternCost);
}
