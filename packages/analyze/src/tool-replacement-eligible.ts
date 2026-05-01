// Tool-replacement-eligible detector.
//
// Find vanilla call sequences in the session log that map to a known
// relaywash (https://github.com/AgentWorkforce/wash) replacement tool. Each
// finding answers "you'd save N tokens by installing relaywash" with a
// concrete, attributable estimate.
//
// The detector reads only `TurnRecord.toolCalls` (no content sidecar, no
// tool-result events) so it runs on any slice with `hasToolCalls` coverage.
// Token-saving estimates are conservative per-occurrence flat rates — the
// real numbers will land via `_meta.replaces` on the relaywash side once
// that annotation is wired in (issue #219). Until then, the flat rates are
// rough-but-defensible: they'd be off by 2× either direction at worst.
//
// Detected patterns (highest-confidence first):
//
//   - search-sequence: Glob → Grep → Read (in that order) within one turn.
//     Replacement: `relaywash__Search` collapses three round-trips into one
//     condensed result. Flagged when ≥3 such sequences appear in a session.
//
//   - edit-cluster: ≥3 single-edit calls to the same file within 5
//     consecutive turns. Replacement: `relaywash__Edit` (batched) folds
//     N point edits into one call. Each "extra" edit beyond the first is
//     counted as savings.
//
//   - bash-git-state: `git status`, `git diff`, `git log` invocations.
//     Replacement: `relaywash__GitState` returns a structured summary
//     instead of raw text.
//
//   - bash-test-run: `pnpm test`, `npm test`, `pytest`, `jest`, etc.
//     Replacement: `relaywash__TestRun` returns just pass/fail counts plus
//     the first failure detail, instead of full test output.
//
//   - bash-gh-pr: `gh pr <verb>` and `gh api`.
//     Replacement: `relaywash__GhPR` returns a structured PR summary.

import {
  normalizeToolName,
  parseBashCommand,
  type SourceKind,
  type ToolCall,
  type TurnRecord,
} from '@relayburn/reader';

import { lookupModelRate } from './cost.js';
import type { WasteAction, WasteFinding, WasteSeverity } from './findings.js';
import type { PricingTable } from './pricing.js';

export type ToolReplacementCategory =
  | 'search-sequence'
  | 'edit-cluster'
  | 'bash-git-state'
  | 'bash-test-run'
  | 'bash-gh-pr';

export interface ToolReplacementEligibleFinding {
  source: SourceKind;
  sessionId: string;
  category: ToolReplacementCategory;
  // The relaywash tool name that would have applied (e.g. `relaywash__Search`).
  replacementTool: string;
  // Number of vanilla calls (or sequences, for search-sequence) observed.
  occurrenceCount: number;
  // Estimated tokens that would be saved per session if the replacement
  // were installed. Flat per-occurrence rates until issue #219 lands.
  estimatedTokensSaved: number;
  // USD savings, priced at the session's dominant model's input rate.
  // Zero when no priced model is available.
  estimatedUsdSaved: number;
  // First few turn indexes where the pattern fired. Bounded so the JSON
  // output stays compact.
  sampleTurnIndexes: number[];
  // Free-form evidence — file paths for edit-cluster, distinct bash verbs
  // for the bash-* categories, empty for search-sequence.
  evidence: string[];
}

export interface DetectToolReplacementEligibleOptions {
  pricing: PricingTable;
}

// Per-occurrence token-savings estimates. Conservative ballparks until
// `_meta.replaces` annotations from relaywash supply real numbers (issue
// #219). The detector emits these alongside the actual occurrence counts so
// downstream consumers can re-price with their own rates if needed.

// A search sequence (Glob + Grep + Read) typically materializes ~3000 tokens
// of intermediate output across three round-trips. `relaywash__Search`
// returns a single condensed block of ~500 tokens.
const SAVINGS_PER_SEARCH_SEQUENCE = 2500;

// Each "extra" edit in a cluster carries its own input echo (the surrounding
// file context) and tool_result confirmation. Batched relaywash edits collapse
// N edits into one call; we count savings as (N-1) × per-edit overhead.
const SAVINGS_PER_EXTRA_EDIT_IN_CLUSTER = 400;

// `git status` / `git diff` / `git log` outputs vary widely. The relaywash
// replacement returns a structured summary instead of full text.
const SAVINGS_PER_GIT_STATE_CALL = 800;

// `pnpm test` / `pytest` / `jest` outputs full test summaries. The
// relaywash replacement returns just pass/fail counts + first failure detail.
const SAVINGS_PER_TEST_RUN_CALL = 1200;

// `gh pr view` / `gh api` typically return JSON blobs. relaywash returns
// a structured summary of the pieces an agent actually uses.
const SAVINGS_PER_GH_PR_CALL = 600;

// Minimum search sequences before we surface a finding. One or two might
// be incidental; ≥3 indicates a habit.
const SEARCH_SEQUENCE_MIN_PER_SESSION = 3;

// Edit-cluster detection window.
const EDIT_CLUSTER_MIN = 3;
const EDIT_CLUSTER_TURN_WINDOW = 5;

// Severity tiers shared with the rest of the WasteFinding family. Mirrors
// findings.ts so heterogeneous lists rank consistently.
const SEVERITY_HIGH_USD = 0.5;
const SEVERITY_WARN_USD = 0.05;

function severityFromUsd(usd: number): WasteSeverity {
  if (usd >= SEVERITY_HIGH_USD) return 'high';
  if (usd >= SEVERITY_WARN_USD) return 'warn';
  return 'info';
}

const RELAYWASH_REPO_URL = 'https://github.com/AgentWorkforce/wash';

// Cross-harness tool-name buckets. Glob/Grep/Read/Edit use `normalizeToolName`
// to fold Codex (`read_file`, `apply_patch`) and OpenCode (lowercase)
// variants. Bash detection keys off the raw name so we don't mis-route
// `parseBashCommand` to non-Bash tools.
const BASH_RAW_NAMES = new Set(['Bash', 'bash', 'exec_command', 'shell']);

export function detectToolReplacementEligible(
  turns: TurnRecord[],
  opts: DetectToolReplacementEligibleOptions,
): ToolReplacementEligibleFinding[] {
  const out: ToolReplacementEligibleFinding[] = [];
  const bySession = new Map<string, TurnRecord[]>();
  for (const t of turns) {
    let list = bySession.get(t.sessionId);
    if (!list) {
      list = [];
      bySession.set(t.sessionId, list);
    }
    list.push(t);
  }
  for (const [sessionId, sessionTurns] of bySession) {
    sessionTurns.sort((a, b) => a.turnIndex - b.turnIndex);
    out.push(...detectForSession(sessionId, sessionTurns, opts.pricing));
  }
  out.sort((a, b) => b.estimatedUsdSaved - a.estimatedUsdSaved || b.estimatedTokensSaved - a.estimatedTokensSaved);
  return out;
}

function detectForSession(
  sessionId: string,
  turns: TurnRecord[],
  pricing: PricingTable,
): ToolReplacementEligibleFinding[] {
  if (turns.length === 0) return [];
  const source = turns[0]!.source;
  const inputRate = pickInputRate(turns, pricing);
  const out: ToolReplacementEligibleFinding[] = [];

  // Search sequences.
  const searchSequenceTurns: number[] = [];
  for (const t of turns) {
    if (turnHasSearchSequence(t.toolCalls)) searchSequenceTurns.push(t.turnIndex);
  }
  if (searchSequenceTurns.length >= SEARCH_SEQUENCE_MIN_PER_SESSION) {
    const tokens = searchSequenceTurns.length * SAVINGS_PER_SEARCH_SEQUENCE;
    out.push({
      source,
      sessionId,
      category: 'search-sequence',
      replacementTool: 'relaywash__Search',
      occurrenceCount: searchSequenceTurns.length,
      estimatedTokensSaved: tokens,
      estimatedUsdSaved: priceTokens(tokens, inputRate),
      sampleTurnIndexes: searchSequenceTurns.slice(0, 5),
      evidence: [],
    });
  }

  // Edit clusters: per-file sliding window over edit-bearing turns.
  for (const cluster of detectEditClusters(turns)) {
    const extras = cluster.editCount - 1;
    const tokens = extras * SAVINGS_PER_EXTRA_EDIT_IN_CLUSTER;
    out.push({
      source,
      sessionId,
      category: 'edit-cluster',
      replacementTool: 'relaywash__Edit',
      occurrenceCount: cluster.editCount,
      estimatedTokensSaved: tokens,
      estimatedUsdSaved: priceTokens(tokens, inputRate),
      sampleTurnIndexes: cluster.turnIndexes.slice(0, 5),
      evidence: [cluster.filePath],
    });
  }

  // Bash sub-verb matches.
  const gitState: BashHit[] = [];
  const testRun: BashHit[] = [];
  const ghPr: BashHit[] = [];
  for (const t of turns) {
    for (const call of t.toolCalls) {
      if (!BASH_RAW_NAMES.has(call.name) || !call.target) continue;
      const parsed = parseBashCommand(call.target);
      if (!parsed) continue;
      if (matchesGitState(parsed)) {
        gitState.push({ verb: parsed.normalized, turnIndex: t.turnIndex });
      } else if (matchesTestRun(parsed)) {
        testRun.push({ verb: parsed.normalized, turnIndex: t.turnIndex });
      } else if (matchesGhPr(parsed)) {
        ghPr.push({ verb: parsed.normalized, turnIndex: t.turnIndex });
      }
    }
  }
  if (gitState.length > 0) {
    out.push(buildBashFinding(source, sessionId, 'bash-git-state', 'relaywash__GitState', gitState, SAVINGS_PER_GIT_STATE_CALL, inputRate));
  }
  if (testRun.length > 0) {
    out.push(buildBashFinding(source, sessionId, 'bash-test-run', 'relaywash__TestRun', testRun, SAVINGS_PER_TEST_RUN_CALL, inputRate));
  }
  if (ghPr.length > 0) {
    out.push(buildBashFinding(source, sessionId, 'bash-gh-pr', 'relaywash__GhPR', ghPr, SAVINGS_PER_GH_PR_CALL, inputRate));
  }

  return out;
}

interface BashHit {
  verb: string;
  turnIndex: number;
}

function buildBashFinding(
  source: SourceKind,
  sessionId: string,
  category: ToolReplacementCategory,
  replacementTool: string,
  hits: BashHit[],
  savingsPerCall: number,
  inputRate: number,
): ToolReplacementEligibleFinding {
  const tokens = hits.length * savingsPerCall;
  return {
    source,
    sessionId,
    category,
    replacementTool,
    occurrenceCount: hits.length,
    estimatedTokensSaved: tokens,
    estimatedUsdSaved: priceTokens(tokens, inputRate),
    sampleTurnIndexes: dedupNumbers(hits.map((h) => h.turnIndex)).slice(0, 5),
    evidence: dedupStrings(hits.map((h) => h.verb)),
  };
}

// True iff the turn's tool calls contain Glob → Grep → Read in that order
// (with arbitrary other calls allowed in between). The relaywash replacement
// pattern matches any session where the agent stitches discovery + filtering +
// reading by hand, so a strict "back-to-back" requirement would miss the
// real case where a Glob is followed by a Bash echo before the Grep lands.
function turnHasSearchSequence(calls: ToolCall[]): boolean {
  let stage: 'glob' | 'grep' | 'read' = 'glob';
  for (const call of calls) {
    const name = normalizeToolName(call.name);
    if (stage === 'glob' && name === 'Glob') {
      stage = 'grep';
    } else if (stage === 'grep' && name === 'Grep') {
      stage = 'read';
    } else if (stage === 'read' && name === 'Read') {
      return true;
    }
  }
  return false;
}

interface EditCluster {
  filePath: string;
  editCount: number;
  turnIndexes: number[];
}

// Per-file sliding window over edit calls. We collect every edit-bearing
// (file, turnIndex) pair, then for each file flag any window of
// EDIT_CLUSTER_TURN_WINDOW consecutive turns that holds ≥ EDIT_CLUSTER_MIN
// edits. We emit at most one cluster per file per session — once a file
// trips the threshold, any later edits are part of the same finding.
function detectEditClusters(turns: TurnRecord[]): EditCluster[] {
  const byFile = new Map<string, number[]>();
  for (const t of turns) {
    for (const call of t.toolCalls) {
      const name = normalizeToolName(call.name);
      if (name !== 'Edit' && name !== 'Write' && name !== 'NotebookEdit') continue;
      if (!call.target) continue;
      // Failed edits are still candidates — relaywash's batched replacement
      // would have applied identically. We do not de-dup the same turn
      // emitting two edits to the same file; both contribute to the cluster.
      let list = byFile.get(call.target);
      if (!list) {
        list = [];
        byFile.set(call.target, list);
      }
      list.push(t.turnIndex);
    }
  }
  const out: EditCluster[] = [];
  for (const [filePath, turnIndexes] of byFile) {
    if (turnIndexes.length < EDIT_CLUSTER_MIN) continue;
    turnIndexes.sort((a, b) => a - b);
    let bestCount = 0;
    let bestWindow: number[] = [];
    for (let i = 0; i < turnIndexes.length; i++) {
      const start = turnIndexes[i]!;
      const window: number[] = [];
      for (let j = i; j < turnIndexes.length; j++) {
        if (turnIndexes[j]! - start > EDIT_CLUSTER_TURN_WINDOW) break;
        window.push(turnIndexes[j]!);
      }
      if (window.length > bestCount) {
        bestCount = window.length;
        bestWindow = window;
      }
    }
    if (bestCount >= EDIT_CLUSTER_MIN) {
      out.push({ filePath, editCount: bestCount, turnIndexes: bestWindow });
    }
  }
  return out;
}

function matchesGitState(parsed: { binary: string; subcommand?: string }): boolean {
  if (parsed.binary !== 'git') return false;
  return parsed.subcommand === 'status' || parsed.subcommand === 'diff' || parsed.subcommand === 'log';
}

function matchesTestRun(parsed: { binary: string; subcommand?: string }): boolean {
  if (parsed.binary === 'pytest' || parsed.binary === 'jest' || parsed.binary === 'vitest') return true;
  if (parsed.binary === 'cargo' && parsed.subcommand === 'test') return true;
  if (parsed.binary === 'go' && parsed.subcommand === 'test') return true;
  if ((parsed.binary === 'pnpm' || parsed.binary === 'npm' || parsed.binary === 'yarn' || parsed.binary === 'bun') && parsed.subcommand) {
    // `pnpm test`, `pnpm test:ts`, `pnpm run test`, etc. PACKAGE_RUNNERS
    // unwrap `run` already, so we see the script name in `subcommand`.
    return parsed.subcommand === 'test' || parsed.subcommand.startsWith('test:') || parsed.subcommand.startsWith('test ');
  }
  return false;
}

function matchesGhPr(parsed: { binary: string; subcommand?: string }): boolean {
  if (parsed.binary !== 'gh') return false;
  if (!parsed.subcommand) return false;
  // TWO_PART_SUBCOMMANDS folds `pr <verb>` into a single subcommand, so
  // `gh pr view` parses to subcommand="pr view". `gh api` is single-part.
  return parsed.subcommand === 'api' || parsed.subcommand.startsWith('pr');
}

// Pick a representative input rate (USD per token) for the session. The
// dominant model wins; ties go to the first-seen model. Falls back to 0 when
// no priced model is available — the finding still emits, just with $0.
function pickInputRate(turns: TurnRecord[], pricing: PricingTable): number {
  const counts = new Map<string, number>();
  for (const t of turns) counts.set(t.model, (counts.get(t.model) ?? 0) + 1);
  let best: string | undefined;
  let bestCount = -1;
  for (const [model, count] of counts) {
    if (count > bestCount) {
      best = model;
      bestCount = count;
    }
  }
  if (!best) return 0;
  const rate = lookupModelRate(best, pricing);
  if (!rate) return 0;
  return rate.input / 1_000_000;
}

function priceTokens(tokens: number, ratePerToken: number): number {
  if (!ratePerToken || tokens <= 0) return 0;
  return tokens * ratePerToken;
}

function dedupNumbers(xs: number[]): number[] {
  const seen = new Set<number>();
  const out: number[] = [];
  for (const x of xs) {
    if (seen.has(x)) continue;
    seen.add(x);
    out.push(x);
  }
  return out.sort((a, b) => a - b);
}

function dedupStrings(xs: string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const x of xs) {
    if (seen.has(x)) continue;
    seen.add(x);
    out.push(x);
  }
  return out;
}

// ---------------------------------------------------------------------------
// WasteFinding adapter
// ---------------------------------------------------------------------------

function fmtUsd(n: number): string {
  return `$${n.toFixed(4)}`;
}

const CATEGORY_TITLES: Record<ToolReplacementCategory, string> = {
  'search-sequence': 'Glob → Grep → Read sequence',
  'edit-cluster': 'Edit cluster on a single file',
  'bash-git-state': 'Vanilla git state via Bash',
  'bash-test-run': 'Vanilla test run via Bash',
  'bash-gh-pr': 'Vanilla gh pr / gh api via Bash',
};

const CATEGORY_REASONS: Record<ToolReplacementCategory, string> = {
  'search-sequence':
    'Discovery + filtering + reading three separate tools in one turn lands a lot of intermediate ' +
    'output in context. relaywash__Search collapses the round-trip into a single condensed result.',
  'edit-cluster':
    'A burst of single edits on one file echoes the surrounding context on every call. ' +
    'relaywash__Edit batches N point edits into one round-trip.',
  'bash-git-state':
    'git status / diff / log dump unbounded raw text. relaywash__GitState returns a structured ' +
    'summary tailored to what the agent actually uses.',
  'bash-test-run':
    'Test runners dump full per-suite output. relaywash__TestRun returns just pass/fail counts ' +
    'plus the first failure detail.',
  'bash-gh-pr':
    'gh pr view / gh api return raw JSON blobs. relaywash__GhPR returns a structured PR summary.',
};

export function toolReplacementEligibleToFinding(
  finding: ToolReplacementEligibleFinding,
): WasteFinding {
  const evidence = finding.evidence.length > 0
    ? ` Evidence: ${finding.evidence.slice(0, 3).join(', ')}${finding.evidence.length > 3 ? `, +${finding.evidence.length - 3} more` : ''}.`
    : '';
  const action: WasteAction = {
    type: 'command',
    label: `Install relaywash to enable ${finding.replacementTool}`,
    text: `# See ${RELAYWASH_REPO_URL} for installation. Replacement: ${finding.replacementTool}`,
  };
  return {
    kind: 'tool-replacement-eligible',
    severity: severityFromUsd(finding.estimatedUsdSaved),
    sessionId: finding.sessionId,
    title:
      `${CATEGORY_TITLES[finding.category]}: ${finding.occurrenceCount}× — replace with ${finding.replacementTool}`,
    detail:
      `${CATEGORY_REASONS[finding.category]} ` +
      `Observed ${finding.occurrenceCount} occurrence(s) in this ${finding.source} session. ` +
      `Estimated savings: ${finding.estimatedTokensSaved.toLocaleString()} tokens ` +
      `(${fmtUsd(finding.estimatedUsdSaved)} at this session's input rate).${evidence} ` +
      `See ${RELAYWASH_REPO_URL}.`,
    estimatedSavings: {
      tokensPerSession: finding.estimatedTokensSaved,
      usdPerSession: finding.estimatedUsdSaved,
    },
    actions: [action],
  };
}
