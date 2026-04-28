import type {
  CompactionEvent,
  ContentRecord,
  ContentToolResult,
  SourceKind,
  ToolCall,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';
import { countRetries, normalizeToolName } from '@relayburn/reader';

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
  // First-line error signature shared across the retried tool_results, when
  // content-sidecar data is available and at least one matching tool_result
  // was captured. If the signatures across attempts diverge, this is the
  // first attempt's signature suffixed with " (signatures diverged)". Absent
  // when no content was supplied or none of the retry tool_uses had a
  // matching tool_result in the sidecar.
  errorSignature?: string;
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
  // One entry per distinct tool involved in the run, when content-sidecar
  // data is available. `firstLine` is the first non-empty line of the first
  // tool_result observed for that tool. Tools whose tool_results aren't in
  // the sidecar are omitted. Absent when no content was supplied or no
  // signatures could be extracted.
  errorSignatures?: Array<{ tool: string; firstLine: string }>;
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
  // Aggregated work performed in the compacted window (between the previous
  // compact_boundary or session start and this one), populated when content
  // sidecar data is available. `files` lists distinct paths touched by edit
  // tools across the window; the *Count fields tally normalized tool calls
  // (`Bash`, `Edit`/`Write`, `Read`). Absent when content was not supplied.
  lostWork?: {
    files: string[];
    bashCount: number;
    editCount: number;
    readCount: number;
  };
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
  // Truncated `old_string`/`new_string` strings for the first edit and the
  // reverting edit, when content-sidecar data is available. Each string is
  // capped at SAMPLE_PREVIEW_MAX_CHARS (with an ellipsis suffix on overflow)
  // so reports stay scannable. Absent when content was not supplied or the
  // matching tool_use entries were not captured.
  samplePreview?: {
    firstEdit: { old: string; new: string };
    revert: { old: string; new: string };
  };
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

// Edit-heavy session: edit-tool count >> read-tool count. A session that
// mostly writes without first reading the surrounding context correlates with
// careless editing. Complementary to edit-revert — content-hash catches the
// clean A→B→A revert; ratio catches the fuzzy "many small edits, not enough
// reads" case.
//
// Cross-harness via `normalizeToolName`: Claude `Read`/`Edit`/`Write`/...,
// OpenCode `read`/`edit`/`write`, Codex `read_file`/`apply_patch`. Codex's
// `apply_patch` may bundle multiple files per call, so the same threshold
// flags Codex more conservatively than it would if we counted files — known
// per-harness tunable, see #167.
export interface EditHeavySession {
  source: SourceKind;
  sessionId: string;
  readCount: number;
  editCount: number;
  // editCount / readCount; +Infinity when reads === 0
  ratio: number;
  // Sum of edit→bash→edit retries (from `countRetries`) across the session's
  // turns. A high retry count alongside a high ratio is the strongest signal.
  likelyRetries: number;
  // Sum of per-turn cost across turns containing an edit-tool call.
  cost: number;
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
  editHeavyCount: number;
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
  editHeavySessions: EditHeavySession[];
  sessionSummaries: SessionPatternSummary[];
}

export interface DetectPatternsOptions {
  pricing: PricingTable;
  compactions?: CompactionEvent[];
  // sessionId -> UserTurnRecord[] in source order. Used to estimate the
  // first user message size for the system-prompt-tax detector.
  userTurnsBySession?: Map<string, UserTurnRecord[]> | undefined;
  // sessionId -> ContentRecord[] in source order. When supplied, the four
  // waste-pattern detectors enrich their output with content-derived fields
  // (error signatures, compacted-window summaries, edit previews). Detectors
  // run identically without it — only the enrichment fields are absent.
  contentBySession?: Map<string, ContentRecord[]> | undefined;
}

const MIN_RETRY_LEN = 3;
const MIN_FAILURE_RUN_LEN = 3;

// Edit revert sample previews are truncated to this length per field. Long
// `old_string`/`new_string` blocks are common (whole functions, JSON blobs);
// 200 chars is enough to identify what was thrashed without bloating reports
// or content-sidecar query payloads.
const SAMPLE_PREVIEW_MAX_CHARS = 200;

// edits / reads above this and editCount ≥ MIN flags the session.
// Threshold ported from codeburn (Claude-derived). Codex `apply_patch` bundles
// multiple files per call, so this same threshold flags Codex more
// conservatively than a file-level count would — documented in #167 as a
// known per-harness tunable.
const EDIT_HEAVY_RATIO = 4;
const EDIT_HEAVY_MIN_EDITS = 5;

// Tools whose normalized name (post `normalizeToolName`) counts as a "read of
// file content" for the purposes of the read:edit ratio. Grep / Glob / LS
// don't count — they discover files but don't read content. Bash is excluded
// for the same reason: the model may `cat` via shell, but identifying that
// from arguments is fragile and would inflate the read count for unrelated
// shell calls. Keeping the set narrow matches the codeburn intent ("did the
// model read the file before editing it?").
const READ_TOOLS = new Set(['Read', 'NotebookRead']);
const EDIT_TOOLS = new Set(['Edit', 'Write', 'NotebookEdit', 'MultiEdit']);

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
  const editHeavySessions: EditHeavySession[] = [];

  for (const [sessionId, sessionTurns] of bySession) {
    sessionTurns.sort((a, b) => a.turnIndex - b.turnIndex);
    const contentIndex = buildContentIndex(opts.contentBySession?.get(sessionId));
    retryLoops.push(...detectRetryLoopsForSession(sessionId, sessionTurns, opts.pricing, contentIndex));
    failureRuns.push(
      ...detectFailureRunsForSession(sessionId, sessionTurns, opts.pricing, contentIndex),
    );
    editReverts.push(...detectEditRevertsForSession(sessionId, sessionTurns, opts.pricing, contentIndex));
    skillRecallDups.push(...detectSkillRecallDupsForSession(sessionId, sessionTurns, opts.pricing));
    skillPruningProtection.push(...detectSkillPruningProtectionForSession(sessionId, sessionTurns, opts.pricing));
    systemPromptTaxes.push(...detectSystemPromptTaxForSession(sessionId, sessionTurns, opts.pricing, opts.userTurnsBySession?.get(sessionId)));
    editHeavySessions.push(...detectEditHeavyForSession(sessionId, sessionTurns, opts.pricing));
  }

  const compactions = opts.compactions
    ? detectCompactionLosses(opts.compactions, turns, opts.pricing, opts.contentBySession)
    : [];

  return {
    retryLoops,
    failureRuns,
    compactions,
    editReverts,
    skillRecallDups,
    skillPruningProtection,
    systemPromptTaxes,
    editHeavySessions,
    sessionSummaries: buildSummaries(
      retryLoops,
      failureRuns,
      compactions,
      editReverts,
      skillRecallDups,
      skillPruningProtection,
      systemPromptTaxes,
      editHeavySessions,
    ),
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

// Per-session lookup over the content sidecar. `toolResults` is keyed by
// `toolUseId`, mirroring the join the four enrichments need; `toolUses`
// gives us the original tool-use input record (used for edit-revert sample
// previews where we need `old_string`/`new_string`). `null` is returned by
// `buildContentIndex` when no content was supplied so detectors can skip
// enrichment work entirely with a single null check.
interface ContentIndex {
  toolResults: Map<string, ContentToolResult>;
  toolUses: Map<string, Record<string, unknown>>;
}

function buildContentIndex(records: ContentRecord[] | undefined): ContentIndex | null {
  if (!records || records.length === 0) return null;
  const toolResults = new Map<string, ContentToolResult>();
  const toolUses = new Map<string, Record<string, unknown>>();
  for (const r of records) {
    if (r.kind === 'tool_result' && r.toolResult) {
      // Keep the first observation per toolUseId. A retry replays the same
      // tool_use id only when the harness reissues it; in practice the
      // detectors care about the result actually returned for the call.
      if (!toolResults.has(r.toolResult.toolUseId)) {
        toolResults.set(r.toolResult.toolUseId, r.toolResult);
      }
    } else if (r.kind === 'tool_use' && r.toolUse) {
      if (!toolUses.has(r.toolUse.id)) {
        toolUses.set(r.toolUse.id, r.toolUse.input);
      }
    }
  }
  return { toolResults, toolUses };
}

// Stringify a tool_result content block to plain text for signature
// extraction. Mirrors `stringifyToolResult` in waste.ts (kept in-module to
// avoid a public export of an internal helper); structured blocks fall back
// to JSON so an `is_error` payload still has *something* readable.
function stringifyToolResultContent(content: unknown): string {
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

// Maximum characters of an error signature we surface. Long stack traces or
// formatted outputs are common; the user wants the leading line, not the
// whole dump. Matches the spec ("first error line — or first N chars").
const ERROR_SIGNATURE_MAX_CHARS = 240;

function extractErrorSignature(toolResult: ContentToolResult | undefined): string | undefined {
  if (!toolResult) return undefined;
  const text = stringifyToolResultContent(toolResult.content);
  if (!text) return undefined;
  // First non-empty line. Some tool errors prefix with blank lines or shells
  // emit the prompt before the error.
  for (const rawLine of text.split('\n')) {
    const line = rawLine.trim();
    if (!line) continue;
    if (line.length <= ERROR_SIGNATURE_MAX_CHARS) return line;
    return line.slice(0, ERROR_SIGNATURE_MAX_CHARS - 1) + '…';
  }
  return undefined;
}

function truncateForPreview(s: string): string {
  if (s.length <= SAMPLE_PREVIEW_MAX_CHARS) return s;
  return s.slice(0, SAMPLE_PREVIEW_MAX_CHARS - 1) + '…';
}

// Pull `old_string`/`new_string` (or `content` for `Write`) out of a captured
// tool_use input. Returns empty strings for fields the input doesn't carry —
// a `Write` doesn't have `old_string` and we don't want to fail the whole
// preview just because one half is absent.
function extractEditPreview(input: Record<string, unknown> | undefined): { old: string; new: string } | undefined {
  if (!input) return undefined;
  const oldRaw = input['old_string'];
  const newRaw = input['new_string'];
  const contentRaw = input['content'];
  const oldStr = typeof oldRaw === 'string' ? oldRaw : '';
  let newStr = typeof newRaw === 'string' ? newRaw : '';
  if (!newStr && typeof contentRaw === 'string') newStr = contentRaw;
  if (!oldStr && !newStr) return undefined;
  return {
    old: truncateForPreview(oldStr),
    new: truncateForPreview(newStr),
  };
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
  contentIndex: ContentIndex | null,
): RetryLoop[] {
  const flat = flattenToolCalls(turns);
  const loops: RetryLoop[] = [];
  let streak: ToolCallRef[] = [];

  const commit = (): void => {
    if (streak.length < MIN_RETRY_LEN) return;
    const first = streak[0]!;
    const last = streak[streak.length - 1]!;
    const contributingTurns = dedupTurns(streak.map((r) => r.turn));
    const loop: RetryLoop = {
      sessionId,
      tool: first.call.name,
      target: first.call.target,
      argsHash: first.call.argsHash,
      attempts: streak.length,
      startTurnIndex: first.turn.turnIndex,
      endTurnIndex: last.turn.turnIndex,
      cost: sumCostForTurns(contributingTurns, pricing),
    };
    const sig = retryLoopSignature(streak, contentIndex);
    if (sig !== undefined) loop.errorSignature = sig;
    loops.push(loop);
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

function retryLoopSignature(
  streak: ToolCallRef[],
  contentIndex: ContentIndex | null,
): string | undefined {
  if (!contentIndex) return undefined;
  let firstSig: string | undefined;
  let diverged = false;
  for (const ref of streak) {
    const result = contentIndex.toolResults.get(ref.call.id);
    const sig = extractErrorSignature(result);
    if (!sig) continue;
    if (firstSig === undefined) {
      firstSig = sig;
      continue;
    }
    if (sig !== firstSig) {
      diverged = true;
      break;
    }
  }
  if (firstSig === undefined) return undefined;
  return diverged ? `${firstSig} (signatures diverged)` : firstSig;
}

function detectFailureRunsForSession(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
  contentIndex: ContentIndex | null,
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
    const run: FailureRun = {
      sessionId,
      length: streak.length,
      startTurnIndex: first.turn.turnIndex,
      endTurnIndex: last.turn.turnIndex,
      toolsInvolved: tools,
      cost: sumCostForTurns(contributingTurns, pricing),
    };
    const sigs = failureRunSignatures(streak, contentIndex);
    if (sigs.length > 0) run.errorSignatures = sigs;
    runs.push(run);
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

function failureRunSignatures(
  streak: ToolCallRef[],
  contentIndex: ContentIndex | null,
): Array<{ tool: string; firstLine: string }> {
  if (!contentIndex) return [];
  // One entry per *distinct* tool, in first-seen order. We use the first
  // tool_result we observe for each tool — that's the "what blew up first"
  // signature the user wants to read in a report. Subsequent failures for
  // the same tool may have different signatures; if the user wants those
  // they can `burn diagnose <session>`.
  const out: Array<{ tool: string; firstLine: string }> = [];
  const seen = new Set<string>();
  for (const ref of streak) {
    if (seen.has(ref.call.name)) continue;
    const result = contentIndex.toolResults.get(ref.call.id);
    const sig = extractErrorSignature(result);
    if (!sig) continue;
    out.push({ tool: ref.call.name, firstLine: sig });
    seen.add(ref.call.name);
  }
  return out;
}

function detectEditRevertsForSession(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
  contentIndex: ContentIndex | null,
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
    toolUseId: string;
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
      toolUseId: call.id,
    };
    const history = byFile.get(call.target) ?? [];
    if (slot.postHash !== undefined) {
      const matchIdx = history.findIndex((prior) => prior.preHash === slot.postHash);
      if (matchIdx >= 0) {
        const first = history[matchIdx]!;
        const cycle: EditRevertCycle = {
          sessionId,
          filePath: call.target,
          firstEditTurnIndex: first.turn.turnIndex,
          revertTurnIndex: ref.turn.turnIndex,
          spanTurns: ref.turn.turnIndex - first.turn.turnIndex,
          cost: sumCostForTurns(dedupTurns([first.turn, ref.turn]), pricing),
        };
        if (contentIndex) {
          const firstEdit = extractEditPreview(contentIndex.toolUses.get(first.toolUseId));
          const revert = extractEditPreview(contentIndex.toolUses.get(slot.toolUseId));
          if (firstEdit && revert) {
            cycle.samplePreview = { firstEdit, revert };
          }
        }
        cycles.push(cycle);
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
  contentBySession: Map<string, ContentRecord[]> | undefined,
): CompactionLoss[] {
  const turnByMessageId = new Map<string, TurnRecord>();
  for (const t of turns) turnByMessageId.set(t.messageId, t);

  // Group events by session in arrival order so we can bound each event's
  // "compacted window" by its previous boundary. Events in `events` are not
  // guaranteed to be sorted, so we sort by ts (falling back to source order)
  // before walking. Turns within a session are pre-sorted by `turnIndex`,
  // which mirrors source order.
  const eventsBySession = new Map<string, CompactionEvent[]>();
  for (const e of events) {
    const list = eventsBySession.get(e.sessionId) ?? [];
    list.push(e);
    eventsBySession.set(e.sessionId, list);
  }
  for (const list of eventsBySession.values()) {
    list.sort((a, b) => a.ts.localeCompare(b.ts));
  }

  // Sort turns by session, then turnIndex, so we can bisect by ts.
  const turnsBySession = new Map<string, TurnRecord[]>();
  for (const t of turns) {
    const list = turnsBySession.get(t.sessionId) ?? [];
    list.push(t);
    turnsBySession.set(t.sessionId, list);
  }
  for (const list of turnsBySession.values()) {
    list.sort((a, b) => a.turnIndex - b.turnIndex);
  }

  // Track the previous boundary ts per session for `lostWork` window math.
  const prevBoundaryTs = new Map<string, string>();

  const out: CompactionLoss[] = [];
  for (const sessionEvents of eventsBySession.values()) {
    for (const e of sessionEvents) {
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
      const loss: CompactionLoss = {
        sessionId: e.sessionId,
        ts: e.ts,
        precedingMessageId: e.precedingMessageId,
        tokensBeforeCompact: tokens,
        cacheLostCost,
      };
      // Gate on content-sidecar presence — `lostWork` is the "with content"
      // enrichment, even though the aggregate uses `TurnRecord.toolCalls`
      // (which carries the same shape as the sidecar's tool_use records and
      // is the more reliable join key for windowing). Without content
      // capture, we honor the spec's graceful-degradation contract.
      if (contentBySession?.get(e.sessionId)) {
        const sessionTurns = turnsBySession.get(e.sessionId) ?? [];
        const windowStart = prevBoundaryTs.get(e.sessionId);
        loss.lostWork = summarizeCompactedWindow(
          sessionTurns,
          windowStart,
          e.ts,
        );
      }
      out.push(loss);
      prevBoundaryTs.set(e.sessionId, e.ts);
    }
  }
  return out;
}

// Aggregate the work performed in the compacted window: distinct files
// touched (from edit/write tool calls) and counts per normalized tool
// category. The window is `(windowStart, boundaryTs]` — exclusive on the
// previous boundary, inclusive on the current one — so a turn that produced
// the boundary itself counts as work that lost its cache.
function summarizeCompactedWindow(
  sessionTurns: TurnRecord[],
  windowStart: string | undefined,
  boundaryTs: string,
): { files: string[]; bashCount: number; editCount: number; readCount: number } {
  let bashCount = 0;
  let editCount = 0;
  let readCount = 0;
  const files = new Set<string>();
  for (const t of sessionTurns) {
    if (windowStart !== undefined && t.ts <= windowStart) continue;
    if (t.ts > boundaryTs) continue;
    for (const call of t.toolCalls) {
      const name = normalizeToolName(call.name);
      if (name === 'Bash') bashCount++;
      else if (EDIT_TOOLS.has(name)) {
        editCount++;
        if (call.target) files.add(call.target);
      } else if (READ_TOOLS.has(name)) readCount++;
    }
  }
  return {
    files: Array.from(files).sort(),
    bashCount,
    editCount,
    readCount,
  };
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
): SkillPruningProtection[] {
  // Only relevant for OpenCode sessions.
  if (turns.length === 0 || turns[0]!.source !== 'opencode') return [];

  // Skill tool results are prune-protected, so compaction boundaries don't
  // bound the riding-turn count — the content survives across them.
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
  // Skip the first turn — its cost is the cacheCreate, not the riding tax,
  // and on a resumed session it may already have cacheRead > 0 which would
  // otherwise inflate the count.
  let ridingTurns = 0;
  let totalCost = 0;
  for (const t of turns) {
    if (t === firstTurn) continue;
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

function detectEditHeavyForSession(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
): EditHeavySession[] {
  if (turns.length === 0) return [];

  let readCount = 0;
  let editCount = 0;
  let likelyRetries = 0;
  const editTurns: TurnRecord[] = [];

  for (const t of turns) {
    let turnHasEdit = false;
    for (const call of t.toolCalls) {
      const name = normalizeToolName(call.name);
      if (READ_TOOLS.has(name)) readCount++;
      else if (EDIT_TOOLS.has(name)) {
        editCount++;
        turnHasEdit = true;
      }
    }
    if (turnHasEdit) editTurns.push(t);
    likelyRetries += countRetries(t.toolCalls);
  }

  if (editCount < EDIT_HEAVY_MIN_EDITS) return [];
  const ratio = readCount === 0 ? Number.POSITIVE_INFINITY : editCount / readCount;
  if (ratio <= EDIT_HEAVY_RATIO) return [];

  return [{
    source: turns[0]!.source,
    sessionId,
    readCount,
    editCount,
    ratio,
    likelyRetries,
    cost: sumCostForTurns(dedupTurns(editTurns), pricing),
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
  editHeavySessions: EditHeavySession[],
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
        editHeavyCount: 0,
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
  for (const e of editHeavySessions) {
    const row = get(e.sessionId);
    row.editHeavyCount++;
    // Cost is recorded on the row but not added to totalPatternCost — the
    // edit-bearing turns also feed into edit-revert and retry-loop costs, and
    // adding them again would double-count the same dollars.
  }
  return [...by.values()].sort((a, b) => b.totalPatternCost - a.totalPatternCost);
}
