import type { ContentRecord, TurnRecord } from '@relayburn/reader';

// Quality signals for the "was this work good enough that a cheaper model
// could have done it" question. Two orthogonal detectors, per the decision
// in issue #6: outcome inference (agentsview) + one-shot rate (codeburn).
//
// Design choices that stuck from the issue discussion:
// - No prompt storage required — both signals work from session metadata and
//   tool-call patterns alone. Content (last assistant text) is used *only*
//   to downgrade confidence; never required.
// - Computed lazily at query time, not persisted in the ledger. Upgrading
//   the rules later doesn't require a rebuild.
// - Confidence is explicit on every classification so downstream consumers
//   can filter out low-confidence signals rather than treat them as noise.

export type OutcomeLabel = 'completed' | 'abandoned' | 'errored' | 'unknown';
export type OutcomeConfidence = 'high' | 'medium' | 'low';

export interface SessionOutcome {
  sessionId: string;
  outcome: OutcomeLabel;
  confidence: OutcomeConfidence;
  isRecent: boolean;
  // Why this classification fired. Short identifier so callers can filter/
  // aggregate by reason without re-parsing strings.
  reason:
    | 'automated'
    | 'single-exchange'
    | 'too-short'
    | 'recent'
    | 'user-ended'
    | 'user-ended-long'
    | 'failure-streak'
    | 'give-up'
    | 'assistant-ended'
    | 'unknown-ending'
    | 'empty';
}

export interface OneShotMetrics {
  sessionId: string;
  editTurns: number;
  oneShotTurns: number;
  // `oneShotTurns / editTurns` when editTurns > 0, else undefined. Callers
  // decide what to display for zero-edit sessions (NaN vs "—").
  oneShotRate: number | undefined;
  // Total retries across all turns in the session. Useful alongside the rate
  // as a raw volume signal — a rate of 0.5 with 2 edits reads very different
  // from a rate of 0.5 with 40 edits.
  totalRetries: number;
}

export interface QualityResult {
  outcomes: SessionOutcome[];
  oneShot: OneShotMetrics[];
}

export interface ComputeQualityOptions {
  // Optional: content sidecar records. When provided, give-up phrase
  // matching on the last assistant text downgrades assistant-ended sessions
  // from 'completed/medium' to 'completed/low'. Without content, the
  // give-up downgrade is skipped — the classifier still runs.
  contentBySession?: Map<string, ContentRecord[]>;
  // Clock override for tests. Defaults to `Date.now()`.
  now?: number;
}

// Phrases observed in agentsview's give-up heuristic plus additions from
// real Claude/Codex sessions. Kept case-insensitive.
const GIVE_UP_PATTERNS = [
  "i'm unable to",
  'i am unable to',
  "i can't proceed",
  'i cannot proceed',
  "i don't have access",
  'i cannot access',
  'unable to verify',
  "doesn't appear to exist",
];

const RECENT_WINDOW_MS = 10 * 60 * 1000;
const SHORT_CONVERSATION_THRESHOLD = 3;
const LONG_CONVERSATION_THRESHOLD = 10;
const FAILURE_STREAK_THRESHOLD = 3;

export function computeQuality(
  turns: TurnRecord[],
  opts: ComputeQualityOptions = {},
): QualityResult {
  const bySession = new Map<string, TurnRecord[]>();
  for (const t of turns) {
    let list = bySession.get(t.sessionId);
    if (!list) {
      list = [];
      bySession.set(t.sessionId, list);
    }
    list.push(t);
  }

  const outcomes: SessionOutcome[] = [];
  const oneShot: OneShotMetrics[] = [];
  const now = opts.now ?? Date.now();

  for (const [sessionId, sessionTurns] of bySession) {
    sessionTurns.sort((a, b) => a.turnIndex - b.turnIndex);
    outcomes.push(inferOutcome(sessionId, sessionTurns, opts.contentBySession, now));
    oneShot.push(computeOneShotRate(sessionId, sessionTurns));
  }

  return { outcomes, oneShot };
}

export function inferOutcome(
  sessionId: string,
  turns: TurnRecord[],
  contentBySession: Map<string, ContentRecord[]> | undefined,
  nowMs: number,
): SessionOutcome {
  if (turns.length === 0) {
    return {
      sessionId,
      outcome: 'unknown',
      confidence: 'low',
      isRecent: false,
      reason: 'empty',
    };
  }

  // Recency: classifier should not mark a still-active session as abandoned.
  const last = turns[turns.length - 1]!;
  const lastMs = Date.parse(last.ts);
  const isRecent = Number.isFinite(lastMs) && nowMs - lastMs < RECENT_WINDOW_MS;

  const messageCount = turns.length;
  const endedRole = endingRole(turns);
  const failureStreak = trailingFailureStreak(turns);

  // A single assistant turn that reached end_turn is almost always an
  // intentional one-shot exchange (user asked, assistant answered — e.g.
  // "hi → hello", or a single tool-mediated round trip that produces two
  // assistant turns). Treat these as completed at medium confidence rather
  // than falling through to "too-short/unknown". TurnRecord counts assistant
  // turns only, so messageCount <= 2 covers both shapes.
  if (messageCount <= 2 && endedRole === 'assistant') {
    return {
      sessionId,
      outcome: 'completed',
      confidence: 'medium',
      isRecent,
      reason: 'single-exchange',
    };
  }
  if (messageCount < SHORT_CONVERSATION_THRESHOLD) {
    return {
      sessionId,
      outcome: 'unknown',
      confidence: 'low',
      isRecent,
      reason: 'too-short',
    };
  }
  if (isRecent) {
    return {
      sessionId,
      outcome: 'unknown',
      confidence: 'low',
      isRecent: true,
      reason: 'recent',
    };
  }

  if (endedRole === 'user') {
    // Long user-ended sessions are overwhelmingly abandoned (user walked
    // away mid-reply); short ones are ambiguous enough to keep at medium.
    const high = messageCount >= LONG_CONVERSATION_THRESHOLD;
    return {
      sessionId,
      outcome: 'abandoned',
      confidence: high ? 'high' : 'medium',
      isRecent: false,
      reason: high ? 'user-ended-long' : 'user-ended',
    };
  }

  if (failureStreak >= FAILURE_STREAK_THRESHOLD) {
    return {
      sessionId,
      outcome: 'errored',
      confidence: 'medium',
      isRecent: false,
      reason: 'failure-streak',
    };
  }

  if (endedRole === 'unknown') {
    // Source doesn't record stop reason (e.g. Codex) — we can't distinguish
    // a natural stop from a mid-tool-call abandonment. Default to completed
    // at low confidence rather than misclassifying every such session as
    // abandoned.
    return {
      sessionId,
      outcome: 'completed',
      confidence: 'low',
      isRecent: false,
      reason: 'unknown-ending',
    };
  }

  // Assistant-ended successfully — default completed. Give-up phrase in the
  // last assistant text downgrades confidence (but doesn't change the label;
  // we still don't know if the user would have agreed it was done).
  const gaveUp = contentBySession ? detectGiveUp(contentBySession.get(sessionId)) : false;
  return {
    sessionId,
    outcome: 'completed',
    confidence: gaveUp ? 'low' : 'medium',
    isRecent: false,
    reason: gaveUp ? 'give-up' : 'assistant-ended',
  };
}

export function computeOneShotRate(
  sessionId: string,
  turns: TurnRecord[],
): OneShotMetrics {
  let editTurns = 0;
  let oneShotTurns = 0;
  let totalRetries = 0;
  for (const t of turns) {
    // Sidechain (subagent) turns are a different cost-attribution universe;
    // their retry counts don't belong in the parent session's rate.
    if (t.subagent?.isSidechain) continue;
    if (!t.hasEdits) continue;
    editTurns++;
    totalRetries += t.retries ?? 0;
    if ((t.retries ?? 0) === 0) oneShotTurns++;
  }
  return {
    sessionId,
    editTurns,
    oneShotTurns,
    oneShotRate: editTurns > 0 ? oneShotTurns / editTurns : undefined,
    totalRetries,
  };
}

function endingRole(turns: TurnRecord[]): 'user' | 'assistant' | 'unknown' {
  // TurnRecord represents assistant turns; a ToolUse turn is followed by a
  // user tool_result (which may or may not prompt another assistant turn).
  // We infer "ended-with-assistant" when the final turn reached a natural
  // stop (`end_turn`) — i.e. it wasn't still waiting for a tool_result.
  // A non-'end_turn' stop reason means user-ended (session died after a
  // tool_use, before the assistant had a chance to respond). When the
  // source doesn't record stopReason at all (e.g. Codex), return 'unknown'
  // so the caller can avoid the false-negative "abandoned" classification.
  const last = turns[turns.length - 1]!;
  if (last.stopReason === undefined) return 'unknown';
  return last.stopReason === 'end_turn' ? 'assistant' : 'user';
}

function trailingFailureStreak(turns: TurnRecord[]): number {
  // Count trailing consecutive tool calls with isError=true in turn order.
  // Mirrors the detectPatterns consecutive-failure signal but scoped to the
  // tail of the session: a session can recover from mid-session failures
  // (→ still completed) and only the trailing state matters for outcome.
  let streak = 0;
  for (let i = turns.length - 1; i >= 0; i--) {
    const calls = turns[i]!.toolCalls;
    if (calls.length === 0) break;
    // All tool calls in this turn must be errored to count toward the
    // streak. A single success in the trailing turn breaks it — the
    // agent is not strictly stuck.
    const allErrored = calls.every((c) => c.isError === true);
    if (!allErrored) break;
    streak += calls.length;
  }
  return streak;
}

function detectGiveUp(records: ContentRecord[] | undefined): boolean {
  if (!records || records.length === 0) return false;
  // Find last assistant text record.
  for (let i = records.length - 1; i >= 0; i--) {
    const r = records[i]!;
    if (r.role === 'assistant' && r.kind === 'text' && typeof r.text === 'string') {
      const haystack = r.text.toLowerCase();
      return GIVE_UP_PATTERNS.some((p) => haystack.includes(p));
    }
  }
  return false;
}
