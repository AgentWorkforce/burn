import {
  aggregateByBash,
  aggregateByBashVerb,
  aggregateByFile,
  aggregateBySubagent,
  attributeHotspots,
  cancellationRunToFinding,
  compactionLossToFinding,
  detectGhostSurface,
  detectPatterns,
  detectToolCallPatterns,
  detectToolOutputBloat,
  editHeavyToFinding,
  editRevertToFinding,
  buildGhostSurfaceInputs,
  failureRunToFinding,
  ghostSurfaceToFinding,
  loadClaudeSettings,
  loadPricing,
  projectClaudeSettingsPath,
  retryLoopToFinding,
  skillPruningProtectionToFinding,
  skillRecallDupToFinding,
  sortFindings,
  summarizeFidelity,
  systemPromptTaxToFinding,
  toolCallPatternToFinding,
  toolOutputBloatToFinding,
  userClaudeSettingsPath,
  type BashAggregation,
  type BashVerbAggregation,
  type FidelitySummary,
  type FileAggregation,
  type GhostSurfaceFinding,
  type LoadedClaudeSettings,
  type PatternsResult,
  type SubagentAggregation,
  type ToolCallPatternFinding,
  type ToolOutputBloat,
  type WasteFinding,
  type HotspotsResult,
} from '@relayburn/analyze';
import {
  queryAll,
  queryCompactions,
  queryToolResultEvents,
  queryUserTurns,
  readContent,
  type EnrichedTurn,
  type Query,
} from '@relayburn/ledger';
import type {
  ContentRecord,
  Coverage,
  SourceKind,
  ToolResultEventRecord,
  UserTurnRecord,
} from '@relayburn/reader';
import { parseBashCommand } from '@relayburn/reader';

import { ingestAll } from '@relayburn/ingest';
import { formatInt, formatUsd, parseSinceArg, table } from '../format.js';
import type { ParsedArgs } from '../args.js';
import { filterTurnsByProvider, parseProviderFilter } from '../provider.js';
import { withProgress } from '../progress.js';
import { runHotspotsSession } from './hotspots-session.js';

const DEFAULT_TOP_N = 10;
const PATTERN_KINDS = ['retries', 'failures', 'cancellations', 'compaction', 'reverts', 'edit-heavy', 'opencode-skill-recall', 'opencode-skill-pruning', 'opencode-system-prompt', 'ghost-surface', 'tool-output-bloat', 'tool-call-pattern'] as const;
type PatternKind = (typeof PATTERN_KINDS)[number];

// When even-split sessions reach this fraction of the matched set, the
// attribution caveat is promoted from a footer note to a top banner and
// every dollar table heading is suffixed with "(approximate)". Below this
// fraction the current footer note is preserved.
const EVEN_SPLIT_DEGRADED_THRESHOLD = 0.5;

export function isAttributionDegraded(
  result: HotspotsResult,
  threshold: number = EVEN_SPLIT_DEGRADED_THRESHOLD,
): boolean {
  if (result.sessionTotals.length === 0) return false;
  const evenSplit = result.sessionTotals.filter(
    (s) => s.attributionMethod === 'even-split',
  ).length;
  return evenSplit / result.sessionTotals.length >= threshold;
}

// Coverage flags a turn must carry to participate in `attributeHotspots` and the
// matching aggregators. A turn missing either flag has no chronology we can
// allocate cost against (no per-call records, or no result-side bytes to
// allocate the next-turn input delta over). Records without `fidelity` (older
// ledger writers, foreign sources) are treated as best-effort full and pass
// the gate.
export const ATTRIBUTION_REQUIRED: ReadonlyArray<keyof Coverage> = [
  'hasToolCalls',
  'hasToolResultEvents',
];

// Returns `true` if the turn carries every coverage flag in `required`.
// Records without `fidelity` (older ledger writers, foreign sources) are
// treated as best-effort full — they pass regardless of `required`.
export function turnPassesCoverage(
  turn: Pick<EnrichedTurn, 'fidelity'>,
  required: ReadonlyArray<keyof Coverage>,
): boolean {
  const f = turn.fidelity;
  if (!f) return true;
  for (const key of required) {
    if (!f.coverage[key]) return false;
  }
  return true;
}

export interface CoverageGapBreakdown {
  // sourceKind -> set of missing-coverage flags observed on excluded turns
  // from that source. Used to render "codex (per-turn, missing tool-result
  // events), opencode (per-session-aggregate)"-style messages without
  // hand-rolling source-specific copy at every call site.
  sources: Map<SourceKind, { missing: Set<keyof Coverage>; granularities: Set<string>; count: number }>;
}

export function describeExcluded(
  excluded: ReadonlyArray<Pick<EnrichedTurn, 'source' | 'fidelity'>>,
  required: ReadonlyArray<keyof Coverage>,
): CoverageGapBreakdown {
  const sources = new Map<SourceKind, { missing: Set<keyof Coverage>; granularities: Set<string>; count: number }>();
  for (const t of excluded) {
    let row = sources.get(t.source);
    if (!row) {
      row = { missing: new Set(), granularities: new Set(), count: 0 };
      sources.set(t.source, row);
    }
    row.count++;
    if (t.fidelity) {
      row.granularities.add(t.fidelity.granularity);
      for (const key of required) {
        if (!t.fidelity.coverage[key]) row.missing.add(key);
      }
    }
  }
  return { sources };
}

export function fmtCoverageKey(key: keyof Coverage): string {
  // `hasToolResultEvents` -> "tool-result events". Keeps the messaging
  // talking about *what's missing* rather than parroting field names.
  switch (key) {
    case 'hasToolCalls':
      return 'tool-call records';
    case 'hasToolResultEvents':
      return 'tool-result events';
    case 'hasSessionRelationships':
      return 'session relationships';
    case 'hasRawContent':
      return 'raw content';
    case 'hasInputTokens':
      return 'input tokens';
    case 'hasOutputTokens':
      return 'output tokens';
    case 'hasReasoningTokens':
      return 'reasoning tokens';
    case 'hasCacheReadTokens':
      return 'cacheRead tokens';
    case 'hasCacheCreateTokens':
      return 'cacheCreate tokens';
  }
}

function renderSourceClause(
  source: SourceKind,
  row: { missing: Set<keyof Coverage>; granularities: Set<string>; count: number },
): string {
  const grans = [...row.granularities].sort();
  const missing = [...row.missing].map(fmtCoverageKey);
  const parts: string[] = [];
  if (grans.length > 0) parts.push(grans.join('+'));
  if (missing.length > 0) parts.push(`missing ${missing.join(', ')}`);
  if (parts.length === 0) return source;
  return `${source} (${parts.join(', ')})`;
}

export function renderSourcesClause(breakdown: CoverageGapBreakdown): string {
  const rows: string[] = [];
  for (const [source, row] of breakdown.sources) {
    rows.push(renderSourceClause(source, row));
  }
  return rows.join('; ');
}

export async function runHotspots(args: ParsedArgs): Promise<number> {
  if (typeof args.flags['session'] === 'string') {
    return runHotspotsSession(args, args.flags['session']);
  }
  if (args.flags['session'] === true) {
    return runHotspotsSession(args);
  }

  const q: Query = {};
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);
  if (typeof args.flags['project'] === 'string') q.project = args.flags['project'];
  if (typeof args.flags['workflow'] === 'string') q.enrichment = { workflowId: args.flags['workflow'] };
  const providerFilter = parseProviderFilter(args.flags['provider']);
  if (providerFilter instanceof Error) {
    process.stderr.write(providerFilter.message);
    return 2;
  }

  await withProgress('ingesting latest sessions', (task) =>
    ingestAll({
      onProgress: (message) => task.update(`ingest: ${message}`),
      onWarn: (body) => task.warn(body),
    }),
  );
  const pricing = await withProgress('loading pricing snapshot', async (task) => {
    const loaded = await loadPricing();
    task.succeed('loaded pricing snapshot');
    return loaded;
  });
  const queriedTurns = await withProgress('reading ledger turns', async (task) => {
    const rows = await queryAll(q);
    task.succeed(`read ${formatInt(rows.length)} turn${rows.length === 1 ? '' : 's'}`);
    return rows;
  });
  const turns = filterTurnsByProvider(queriedTurns, providerFilter);

  // `--findings` is the unified-render flag for `--patterns`; passing it
  // standalone (without `--patterns`) is taken as `--patterns --findings`.
  // The flag is meaningless under default attribution mode, and a silent
  // ignore would surprise users.
  const patternsFlag =
    args.flags['patterns'] ?? (args.flags['findings'] === true ? true : undefined);
  if (patternsFlag !== undefined) {
    const selected = resolvePatternSelection(patternsFlag);
    const sessionIds = new Set(turns.map((t) => t.sessionId));
    const compactions = selected.has('compaction')
      ? (
          await withProgress('reading compaction events', async (task) => {
            const rows = await queryCompactions(q);
            task.succeed(
              `read ${formatInt(rows.length)} compaction event${rows.length === 1 ? '' : 's'}`,
            );
            return rows;
          })
        ).filter((c) => sessionIds.has(c.sessionId))
      : [];
    return runPatternsMode(args, turns, pricing, compactions, selected, { query: q });
  }

  return runHotspotsAttribution(args, turns, pricing, {
    // Bind `q` so the bulk user-turn pass narrows by `since`/`source` during
    // streaming rather than buffering the entire historical ledger first.
    loadUserTurnsBySession: (ids) =>
      withProgress('reading user turns for attribution', async (task) => {
        const out = await bulkUserTurnsBySession(ids, q);
        task.succeed(
          `read user turns for ${formatInt(out.size)} session${out.size === 1 ? '' : 's'}`,
        );
        return out;
      }),
  });
}

// Exposed for tests so they can drive the orchestration with fixture turns
// and a mocked content/userTurns loader. Production callers go through
// `runHotspots`, which fetches both via the ledger.
//
// Bulk-shaped (Set → Map) rather than per-session to keep the production
// `queryUserTurns` path to a single ledger pass: the per-session form
// `queryUserTurns({sessionId})` streams the entire ledger.jsonl on every
// call, which on a 7-day slice with hundreds of sessions adds minutes of
// silent disk I/O after `read N turns`.
export interface HotspotsAttributionDeps {
  loadContentBySession?: (
    sessionIds: Set<string>,
  ) => Promise<Map<string, ContentRecord[]>>;
  loadUserTurnsBySession?: (
    sessionIds: Set<string>,
  ) => Promise<Map<string, UserTurnRecord[]>>;
}

export async function runHotspotsAttribution(
  args: ParsedArgs,
  turns: EnrichedTurn[],
  pricing: Awaited<ReturnType<typeof loadPricing>>,
  deps: HotspotsAttributionDeps = {},
): Promise<number> {
  const total = turns.length;
  const eligible: EnrichedTurn[] = [];
  const excluded: EnrichedTurn[] = [];
  for (const t of turns) {
    if (turnPassesCoverage(t, ATTRIBUTION_REQUIRED)) eligible.push(t);
    else excluded.push(t);
  }

  const fidelityAll = summarizeFidelity(turns);

  // Refusal: nothing to analyze. Exit non-zero with a message that names
  // both the missing prerequisites and the source kinds responsible. This
  // mirrors the "hard-fail with a clear message" wording.
  if (total > 0 && eligible.length === 0) {
    const breakdown = describeExcluded(excluded, ATTRIBUTION_REQUIRED);
    const sourcesClause = renderSourcesClause(breakdown);
    const message =
      `burn hotspots: ${total}/${total} turns lack tool-call/tool-result coverage required for hotspots attribution. ` +
      `Sources: ${sourcesClause}. No hotspots analysis was performed.`;
    if (args.flags['json'] === true) {
      process.stdout.write(
        JSON.stringify(
          {
            turnsAnalyzed: 0,
            grandTotal: 0,
            attributedTotal: 0,
            unattributedTotal: 0,
            attributionDegraded: false,
            sessions: [],
            files: [],
            bashVerbs: [],
            bash: [],
            subagents: [],
            fidelity: {
              analyzed: 0,
              excluded: total,
              summary: fidelityAll,
              refused: true,
            },
            refusalReason: message,
          },
          null,
          2,
        ) + '\n',
      );
    }
    process.stderr.write(message + '\n');
    return 2;
  }

  const loadContent = deps.loadContentBySession ?? defaultLoadContentBySession;
  const loadUserTurns = deps.loadUserTurnsBySession ?? defaultLoadUserTurnsBySession;

  const sessionIds = new Set(eligible.map((t) => t.sessionId));
  const userTurnsBySession = await loadUserTurns(sessionIds);
  const contentBySession = await loadContent(sessionIds);

  const result = attributeHotspots(eligible, {
    pricing,
    contentBySession,
    userTurnsBySession,
  });
  const files = aggregateByFile(result.attributions);
  const bashVerbs = aggregateByBashVerb(result.attributions, parseBashCommand);
  const bashes = aggregateByBash(result.attributions);
  const subagents = aggregateBySubagent(result.attributions);
  const degraded = isAttributionDegraded(result);

  const coverageNotice =
    excluded.length > 0
      ? formatCoverageNotice(eligible.length, total, describeExcluded(excluded, ATTRIBUTION_REQUIRED))
      : undefined;

  if (args.flags['json'] === true) {
    process.stdout.write(
      JSON.stringify(
        {
          turnsAnalyzed: eligible.length,
          grandTotal: result.grandTotal,
          attributedTotal: result.attributedTotal,
          unattributedTotal: result.unattributedTotal,
          attributionDegraded: degraded,
          sessions: result.sessionTotals,
          files,
          bashVerbs,
          bash: bashes,
          subagents,
          fidelity: {
            analyzed: eligible.length,
            excluded: excluded.length,
            summary: fidelityAll,
            refused: false,
          },
        },
        null,
        2,
      ) + '\n',
    );
    return 0;
  }

  const showAll = args.flags['all'] === true;
  const limit = showAll ? Number.POSITIVE_INFINITY : DEFAULT_TOP_N;

  const reportInput: FormatHotspotsReportInput = {
    turnsAnalyzed: eligible.length,
    result,
    files,
    bashVerbs,
    bashes,
    subagents,
    limit,
    degraded,
  };
  if (coverageNotice !== undefined) reportInput.coverageNotice = coverageNotice;
  process.stdout.write(formatHotspotsReport(reportInput));
  return 0;
}

// Render each source with its own missing-fields clause, since one source
// might be missing tool-result events and another might be a session
// aggregate. Joining with " and " reads naturally for ≤ 2 sources and
// doesn't get too clumsy beyond that.
function renderInlineSourceClauses(breakdown: CoverageGapBreakdown): string[] {
  const out: string[] = [];
  for (const [source, row] of breakdown.sources) {
    const grans = [...row.granularities].sort();
    const missing = [...row.missing].map(fmtCoverageKey);
    const inner: string[] = [];
    if (missing.length > 0) inner.push(`missing ${missing.join(', ')}`);
    if (grans.length > 0) inner.push(`${grans.join('+')} granularity`);
    if (inner.length === 0) out.push(source);
    else out.push(`${inner.join(', ')} (${source})`);
  }
  return out;
}

export function formatCoverageNotice(
  analyzed: number,
  total: number,
  breakdown: CoverageGapBreakdown,
): string {
  const excluded = total - analyzed;
  const sourceClauses = renderInlineSourceClauses(breakdown);
  return `analyzed ${formatInt(analyzed)} of ${formatInt(total)} turns; ${formatInt(excluded)} excluded for ${sourceClauses.join(' and ')}`;
}

interface FormatHotspotsReportInput {
  turnsAnalyzed: number;
  result: HotspotsResult;
  files: FileAggregation[];
  bashVerbs?: BashVerbAggregation[];
  bashes: BashAggregation[];
  subagents: SubagentAggregation[];
  limit: number;
  degraded: boolean;
  coverageNotice?: string;
}

export function formatHotspotsReport(input: FormatHotspotsReportInput): string {
  const { turnsAnalyzed, result, files, bashVerbs = [], bashes, subagents, limit, degraded, coverageNotice } = input;
  const evenSplitSessions = result.sessionTotals.filter(
    (s) => s.attributionMethod === 'even-split',
  );

  const out: string[] = [];
  out.push('');
  out.push(`turns analyzed: ${formatInt(turnsAnalyzed)}`);
  if (coverageNotice) out.push(coverageNotice);
  out.push(`session grand total: ${formatUsd(result.grandTotal)}`);

  if (degraded) {
    // Banner above the tables. Numbers are formatted with thousands
    // separators since at degraded scale they're often 5-6 digits.
    const total = result.sessionTotals.length;
    const ev = evenSplitSessions.length;
    const pct = total > 0 ? (ev / total) * 100 : 0;
    out.push('');
    out.push(
      `⚠ attribution is degraded: ${formatInt(ev)} of ${formatInt(total)} sessions (${pct.toFixed(1)}%) have no sized`,
    );
    out.push(
      '  tool-result data, so file / bash / subagent costs for those sessions are approximate',
    );
    out.push(
      "  (even-split over turn N+1 input/cacheCreate). Run 'burn state rebuild content'",
    );
    out.push(
      "  to backfill source-derived sizes, or see 'burn state' for",
    );
    out.push('  why capture is disabled.');
    out.push('');
    out.push(
      `attributed ≈ ${formatUsd(result.attributedTotal)}  (approximate — see above)`,
    );
    out.push(
      `unattributed ${formatUsd(result.unattributedTotal)}  (output, system overhead, untracked)`,
    );
  } else {
    out.push(
      `attributed to tool calls: ${formatUsd(result.attributedTotal)}  /  unattributed (output, system overhead, untracked): ${formatUsd(result.unattributedTotal)}`,
    );
    if (
      evenSplitSessions.length > 0 &&
      evenSplitSessions.length === result.sessionTotals.length
    ) {
      out.push(
        'note: no user-turn or content sidecar sizes found — using even-split (initial cost only). Run burn state rebuild content or enable content.store=full to improve attribution.',
      );
    } else if (evenSplitSessions.length > 0) {
      out.push(
        `note: ${evenSplitSessions.length}/${result.sessionTotals.length} sessions used even-split (no user-turn or content sidecar sizes).`,
      );
    }
  }
  out.push('');

  const approxSuffix = degraded ? ' (approximate)' : '';
  out.push(`Top files by cumulative cost${approxSuffix}`);
  if (files.length === 0) {
    out.push('  (no Read/Edit/Write tool calls)');
  } else {
    out.push(renderFileTable(files, limit, result.attributedTotal));
  }
  out.push('');

  out.push(`Top Bash verbs by cost${approxSuffix}`);
  if (bashVerbs.length === 0) {
    out.push('  (no Bash tool calls)');
  } else {
    out.push(renderBashVerbTable(bashVerbs, limit));
  }
  out.push('');

  out.push(`Top exact Bash commands by cost${approxSuffix}`);
  if (bashes.length === 0) {
    out.push('  (no Bash tool calls)');
  } else {
    out.push(renderBashTable(bashes, limit));
  }
  out.push('');

  out.push(`Top subagent calls by cost${approxSuffix}`);
  if (subagents.length === 0) {
    out.push('  (no Agent/Task tool calls)');
  } else {
    out.push(renderSubagentTable(subagents, limit));
  }
  out.push('');

  return out.join('\n');
}

function renderFileTable(files: FileAggregation[], limit: number, attributed: number): string {
  const rows: string[][] = [
    ['path', 'firstTurn', 'initial(tok)', 'persist(tok)', 'rideTurns', 'cost', '%attr'],
  ];
  const slice = files.slice(0, limit);
  for (const f of slice) {
    const pct = attributed > 0 ? (f.totalCost / attributed) * 100 : 0;
    rows.push([
      f.path,
      String(f.firstEmitTurnIndex),
      formatInt(f.initialTokens),
      formatInt(f.persistenceTokens),
      formatInt(f.ridingTurns),
      formatUsd(f.totalCost),
      `${pct.toFixed(1)}%`,
    ]);
  }
  return table(rows);
}

function renderBashVerbTable(bashVerbs: BashVerbAggregation[], limit: number): string {
  const rows: string[][] = [
    ['verb', 'calls', 'commands', 'initial(tok)', 'persist(tok)', 'avgRide', 'cost', 'examples'],
  ];
  const slice = bashVerbs.slice(0, limit);
  for (const b of slice) {
    rows.push([
      b.verb,
      formatInt(b.callCount),
      formatInt(b.distinctCommands),
      formatInt(b.initialTokens),
      formatInt(b.persistenceTokens),
      b.avgPersistenceTurns.toFixed(1),
      formatUsd(b.totalCost),
      truncate(b.topExamples.map((example) => truncate(example, 40)).join('; '), 90),
    ]);
  }
  return table(rows);
}

function renderBashTable(bashes: BashAggregation[], limit: number): string {
  const rows: string[][] = [['command', 'calls', 'initial(tok)', 'persist(tok)', 'cost']];
  const slice = bashes.slice(0, limit);
  for (const b of slice) {
    rows.push([
      truncate(b.command ?? `(hash ${b.argsHash.slice(0, 8)})`, 60),
      formatInt(b.callCount),
      formatInt(b.initialTokens),
      formatInt(b.persistenceTokens),
      formatUsd(b.totalCost),
    ]);
  }
  return table(rows);
}

function renderSubagentTable(subagents: SubagentAggregation[], limit: number): string {
  const rows: string[][] = [['subagent', 'calls', 'initial(tok)', 'persist(tok)', 'cost']];
  const slice = subagents.slice(0, limit);
  for (const s of slice) {
    rows.push([
      s.subagentType,
      formatInt(s.callCount),
      formatInt(s.initialTokens),
      formatInt(s.persistenceTokens),
      formatUsd(s.totalCost),
    ]);
  }
  return table(rows);
}

function truncate(s: string, n: number): string {
  if (s.length <= n) return s;
  return s.slice(0, n - 1) + '…';
}

export function resolvePatternSelection(flag: string | true): Set<PatternKind> {
  if (flag === true) return new Set(PATTERN_KINDS);
  const set = new Set<PatternKind>();
  for (const raw of flag.split(',').map((s) => s.trim()).filter(Boolean)) {
    if ((PATTERN_KINDS as readonly string[]).includes(raw)) {
      set.add(raw as PatternKind);
    } else {
      throw new Error(
        `unknown --patterns value "${raw}". Valid: ${PATTERN_KINDS.join(', ')}`,
      );
    }
  }
  if (set.size === 0) return new Set(PATTERN_KINDS);
  return set;
}

// Per-detector coverage prerequisites. `compaction` is intentionally absent —
// the compaction sidecar is loaded directly from the ledger via
// `queryCompactions` and is independent of `TurnRecord.fidelity`.
// `tool-output-bloat` is also absent — it reads the tool-result-event ledger
// stream directly (execution-graph substrate) and merges Claude settings.json
// without consulting TurnRecord coverage flags.
//
// The revert detector needs editPreHash / editPostHash, which require
// hasRawContent upstream (the parser computes the hashes from the raw
// strings). hasToolCalls is the obvious prereq.
export const PATTERN_REQUIRED: Record<
  Exclude<PatternKind, 'compaction' | 'ghost-surface' | 'tool-output-bloat'>,
  ReadonlyArray<keyof Coverage>
> = {
  retries: ['hasToolCalls'],
  failures: ['hasToolCalls'],
  cancellations: ['hasToolCalls'],
  reverts: ['hasToolCalls', 'hasRawContent'],
  // Edit-heavy only needs the tool-call stream (counts of read vs edit).
  // tool_result is not consulted, so `hasToolResultEvents` isn't required.
  'edit-heavy': ['hasToolCalls'],
  'opencode-skill-recall': ['hasToolCalls', 'hasToolResultEvents'],
  'opencode-skill-pruning': ['hasToolCalls', 'hasToolResultEvents'],
  'opencode-system-prompt': ['hasToolCalls', 'hasToolResultEvents'],
  // Reads only `TurnRecord.toolCalls` (tool names + Bash targets).
  'tool-call-pattern': ['hasToolCalls'],
};
// `ghost-surface` is filesystem-bound and only needs `hasToolCalls` to
// derive observed-names. We treat it as a soft prerequisite — turns missing
// tool-call records contribute zero to the observed set, which is fine; the
// detector just over-reports ghosts on those turns. The orchestrator
// therefore runs ghost-surface on the full slice (no per-detector filtering)
// like `compaction` does.

interface PatternDetectorCoverage {
  kind: PatternKind;
  analyzed: number;
  excluded: number;
  // Only set when this detector required coverage (compaction never does).
  breakdown?: CoverageGapBreakdown;
  // Whether the detector ran on any turns at all.
  refused: boolean;
}

export interface PatternsModeDeps {
  // Tests can inject a fixture slice; production loads from the ledger using
  // `query` and then filters to the selected turn sessions.
  toolResultEvents?: ToolResultEventRecord[];
  query?: Query;
}

export async function runPatternsMode(
  args: ParsedArgs,
  turns: EnrichedTurn[],
  pricing: Awaited<ReturnType<typeof loadPricing>>,
  compactions: Awaited<ReturnType<typeof queryCompactions>>,
  selected: Set<PatternKind>,
  deps: PatternsModeDeps = {},
): Promise<number> {
  const total = turns.length;
  const fidelityAll = summarizeFidelity(turns);

  // Per-detector filtered slices. `compaction` always runs on the full slice
  // because its data path (the sidecar) doesn't go through TurnRecord at all.
  const perDetector = new Map<PatternKind, EnrichedTurn[]>();
  const perDetectorCoverage: PatternDetectorCoverage[] = [];

  for (const kind of selected) {
    if (kind === 'compaction' || kind === 'ghost-surface' || kind === 'tool-output-bloat') {
      // None of these consult TurnRecord.fidelity. `compaction` reads the
      // ledger compaction stream; `ghost-surface` is filesystem-bound;
      // `tool-output-bloat` reads `tool_result_events` and (for Signal A)
      // a static settings.json. All three run on the full slice.
      perDetector.set(kind, turns);
      perDetectorCoverage.push({
        kind,
        analyzed: total,
        excluded: 0,
        refused: false,
      });
      continue;
    }
    const required = PATTERN_REQUIRED[kind];
    const eligible: EnrichedTurn[] = [];
    const excluded: EnrichedTurn[] = [];
    for (const t of turns) {
      if (turnPassesCoverage(t, required)) eligible.push(t);
      else excluded.push(t);
    }
    perDetector.set(kind, eligible);
    const coverage: PatternDetectorCoverage = {
      kind,
      analyzed: eligible.length,
      excluded: excluded.length,
      refused: total > 0 && eligible.length === 0,
    };
    if (excluded.length > 0) {
      coverage.breakdown = describeExcluded(excluded, required);
    }
    perDetectorCoverage.push(coverage);
  }

  // Refusal: every selected detector refused. `compaction`, `ghost-surface`,
  // and `tool-output-bloat` have no fidelity prereq and are recorded with
  // refused:false unconditionally, so their presence in `selected`
  // short-circuits this — we only refuse when the entire selection is
  // fidelity-gated and every detector lost its slice.
  const refusableSelected = perDetectorCoverage.filter(
    (d) =>
      d.kind !== 'compaction' &&
      d.kind !== 'ghost-surface' &&
      d.kind !== 'tool-output-bloat',
  );
  const allRefused =
    perDetectorCoverage.length > 0 &&
    perDetectorCoverage.every((d) => d.refused);

  if (allRefused) {
    const lines: string[] = [];
    for (const d of refusableSelected) {
      const required = PATTERN_REQUIRED[d.kind as Exclude<PatternKind, 'compaction' | 'ghost-surface' | 'tool-output-bloat'>];
      const sourcesClause = d.breakdown ? renderSourcesClause(d.breakdown) : '(unknown sources)';
      lines.push(
        `  ${d.kind}: ${total}/${total} turns lack ${required.map(fmtCoverageKey).join(' + ')} (sources: ${sourcesClause})`,
      );
    }
    const message =
      `burn hotspots --patterns: no selected detectors can run on this slice.\n` +
      lines.join('\n') +
      `\nNo pattern analysis was performed.`;

    if (args.flags['json'] === true) {
      process.stdout.write(
        JSON.stringify(
          {
            turnsAnalyzed: 0,
            retryLoops: [],
            failureRuns: [],
            cancelledRuns: [],
            compactions: [],
            editReverts: [],
            editHeavySessions: [],
            skillRecallDups: [],
            skillPruningProtection: [],
            systemPromptTaxes: [],
            toolOutputBloats: [],
            toolCallPatterns: [],
            ghostSurface: [],
            sessionSummaries: [],
            findings: [],
            fidelity: {
              analyzed: 0,
              excluded: total,
              summary: fidelityAll,
              refused: true,
              perDetector: perDetectorCoverage.map(toJsonDetector),
            },
            refusalReason: message,
          },
          null,
          2,
        ) + '\n',
      );
    }
    process.stderr.write(message + '\n');
    return 2;
  }

  // Run each enabled detector on its own filtered slice.
  let retryLoops: PatternsResult['retryLoops'] = [];
  let failureRuns: PatternsResult['failureRuns'] = [];
  let cancelledRuns: PatternsResult['cancelledRuns'] = [];
  let compactionLosses: PatternsResult['compactions'] = [];
  let editReverts: PatternsResult['editReverts'] = [];
  let skillRecallDups: PatternsResult['skillRecallDups'] = [];
  let skillPruningProtection: PatternsResult['skillPruningProtection'] = [];
  let systemPromptTaxes: PatternsResult['systemPromptTaxes'] = [];
  let editHeavySessions: PatternsResult['editHeavySessions'] = [];
  let toolOutputBloats: ToolOutputBloat[] = [];
  let toolCallPatterns: ToolCallPatternFinding[] = [];
  let sessionSummaries: PatternsResult['sessionSummaries'] = [];

  // Load user turns when any detector that consumes them is selected:
  //   - opencode-system-prompt: needs first user message size to estimate the
  //     system prompt / skill catalog tax.
  //   - tool-output-bloat: joins per-call `approxTokens` from user-turn
  //     `tool_result` blocks for cl100k-accurate sizing of oversized output.
  const needUserTurns =
    selected.has('opencode-system-prompt') || selected.has('tool-output-bloat');
  const userTurnsBySession = needUserTurns
    ? await withProgress('reading user turns for pattern detectors', async (task) => {
        const rows = await userTurnsForPatternDetectors(perDetector, deps.query);
        task.succeed(`read user turns for ${formatInt(rows.size)} session${rows.size === 1 ? '' : 's'}`);
        return rows;
      })
    : undefined;

  // Load content sidecars for the four detectors that surface content-derived
  // enrichment fields. Detectors fire identically without content; only
  // the optional enrichment fields (errorSignature, errorSignatures, lostWork,
  // samplePreview) are absent. We only pay the I/O cost when one of these
  // detectors is selected.
  const enrichableDetectors: PatternKind[] = ['retries', 'failures', 'compaction', 'reverts'];
  const needContent = enrichableDetectors.some((d) => selected.has(d));
  const contentBySession = needContent
    ? await withProgress('reading content for pattern detectors', async (task) => {
        const rows = await contentForPatternDetectors(perDetector, enrichableDetectors);
        task.succeed(`read content for ${formatInt(rows.size)} session${rows.size === 1 ? '' : 's'}`);
        return rows;
      })
    : undefined;

  const needToolResultEvents =
    selected.has('retries') || selected.has('failures') || selected.has('cancellations');
  const toolResultEvents = needToolResultEvents
    ? (
        deps.toolResultEvents ??
        await withProgress('reading tool-result events for patterns', async (task) => {
          const rows = await loadToolResultEventsForTurns(turns, deps.query);
          task.succeed(`read ${formatInt(rows.length)} tool-result event${rows.length === 1 ? '' : 's'}`);
          return rows;
        })
      )
    : undefined;

  if (selected.has('retries')) {
    const r = detectPatterns(perDetector.get('retries')!, {
      pricing,
      userTurnsBySession,
      contentBySession,
      toolResultEvents,
    });
    retryLoops = r.retryLoops;
  }
  if (selected.has('failures')) {
    const r = detectPatterns(perDetector.get('failures')!, {
      pricing,
      userTurnsBySession,
      contentBySession,
      toolResultEvents,
    });
    failureRuns = r.failureRuns;
  }
  if (selected.has('cancellations')) {
    const r = detectPatterns(perDetector.get('cancellations')!, {
      pricing,
      userTurnsBySession,
      contentBySession,
      toolResultEvents,
    });
    cancelledRuns = r.cancelledRuns;
  } else if (selected.has('retries') || selected.has('failures')) {
    const statusTurns =
      perDetector.get('retries') ?? perDetector.get('failures') ?? [];
    const r = detectPatterns(statusTurns, {
      pricing,
      userTurnsBySession,
      contentBySession,
      toolResultEvents,
    });
    cancelledRuns = r.cancelledRuns;
  }
  if (selected.has('compaction')) {
    const r = detectPatterns(perDetector.get('compaction')!, { pricing, compactions, userTurnsBySession, contentBySession });
    compactionLosses = r.compactions;
  }
  if (selected.has('reverts')) {
    const r = detectPatterns(perDetector.get('reverts')!, { pricing, userTurnsBySession, contentBySession });
    editReverts = r.editReverts;
  }
  if (selected.has('edit-heavy')) {
    const r = detectPatterns(perDetector.get('edit-heavy')!, { pricing, userTurnsBySession });
    editHeavySessions = r.editHeavySessions;
  }
  if (selected.has('opencode-skill-recall')) {
    const r = detectPatterns(perDetector.get('opencode-skill-recall')!, { pricing, compactions, userTurnsBySession });
    skillRecallDups = r.skillRecallDups;
  }
  if (selected.has('opencode-skill-pruning')) {
    const r = detectPatterns(perDetector.get('opencode-skill-pruning')!, { pricing, compactions, userTurnsBySession });
    skillPruningProtection = r.skillPruningProtection;
  }
  if (selected.has('opencode-system-prompt')) {
    const r = detectPatterns(perDetector.get('opencode-system-prompt')!, { pricing, userTurnsBySession });
    systemPromptTaxes = r.systemPromptTaxes;
  }
  if (selected.has('tool-output-bloat')) {
    // Signal A inputs: read both ~/.claude/settings.json and the project's
    // .claude/settings.json. Project comes last so it overrides the user
    // file in `detectStaticConfigBloat`'s last-wins merge. Missing or
    // malformed files yield `undefined` from the loader and are dropped from
    // the input list — see `loadClaudeSettings`.
    const settings = await withProgress('loading Claude settings', async (task) => {
      const loadedSettings: LoadedClaudeSettings[] = [];
      const userLoaded = await loadClaudeSettings(userClaudeSettingsPath());
      if (userLoaded) loadedSettings.push(userLoaded);
      const projectLoaded = await loadClaudeSettings(projectClaudeSettingsPath());
      if (projectLoaded) loadedSettings.push(projectLoaded);
      task.succeed(
        `loaded ${formatInt(loadedSettings.length)} Claude settings file` +
          `${loadedSettings.length === 1 ? '' : 's'}`,
      );
      return loadedSettings;
    });

    // Signal B inputs: stream `tool_result_events` from the ledger. We pass
    // the full TurnRecord set so the detector can join tool_use_ids back to
    // tool names + price the carry cost at the correct model rate. We also
    // pass userTurns so the detector can join per-call cl100k `approxTokens`
    // from the content-sidecar enrichment instead of re-deriving from
    // `contentLength`. The map is loaded once at the top of the function and
    // reused across detectors — flatten its values here since the detector
    // keys lookups by `(source|sessionId|toolUseId)` and doesn't need the
    // per-session structure preserved.
    const toolResultEvents = await withProgress('reading tool-result events for output bloat', async (task) => {
      const rows = await loadToolResultEventsForTurns(turns, deps.query);
      task.succeed(`read ${formatInt(rows.length)} tool-result event${rows.length === 1 ? '' : 's'}`);
      return rows;
    });
    const allUserTurns: UserTurnRecord[] = userTurnsBySession
      ? [...userTurnsBySession.values()].flat()
      : [];
    toolOutputBloats = await withProgress('detecting tool output bloat', async (task) => {
      const rows = detectToolOutputBloat({
        settings,
        toolResultEvents,
        userTurns: allUserTurns,
        turns,
        pricing,
      });
      task.succeed(`detected ${formatInt(rows.length)} tool-output bloat finding${rows.length === 1 ? '' : 's'}`);
      return rows;
    });
  }

  if (selected.has('tool-call-pattern')) {
    toolCallPatterns = detectToolCallPatterns(perDetector.get('tool-call-pattern')!, { pricing });
  }

  // Ghost-surface runs against the on-disk user-installed surface and
  // cross-references basenames against observed names mined from the turn
  // stream. Filesystem-bound and harness-aware: each adapter pulls its own
  // home directory and observed-names slice. Cost is computed against a
  // representative cacheRead rate (the prefix rides in cache on every turn).
  let ghostFindings: GhostSurfaceFinding[] = [];
  if (selected.has('ghost-surface')) {
    const allTurns = perDetector.get('ghost-surface') ?? turns;
    const ghostInputs = await withProgress('building ghost-surface inputs', async (task) => {
      const inputs = await buildGhostSurfaceInputs(allTurns, pricing);
      task.succeed('built ghost-surface inputs');
      return inputs;
    });
    ghostFindings = await withProgress('detecting ghost surfaces', async (task) => {
      const findings = await detectGhostSurface(ghostInputs);
      task.succeed(`detected ${formatInt(findings.length)} ghost-surface finding${findings.length === 1 ? '' : 's'}`);
      return findings;
    });
  }

  // Build session summaries on the union — anything attributed by *any*
  // detector counts. Re-running detectPatterns on a single union slice
  // doesn't work because each detector has its own coverage threshold; instead
  // synthesize the summary from the per-detector results.
  sessionSummaries = buildSessionSummaries(
    retryLoops,
    failureRuns,
    cancelledRuns,
    compactionLosses,
    editReverts,
    skillRecallDups,
    skillPruningProtection,
    systemPromptTaxes,
    editHeavySessions,
  );

  // For the "turns analyzed" headline we report the union of analyzed slices —
  // a turn that survived any detector counts. Compaction has no fidelity
  // prereq and runs on the full slice, so every turn is "analyzed" by it
  // whenever it's selected.
  const analyzedUnion = new Set<string>();
  for (const d of perDetectorCoverage) {
    const slice = perDetector.get(d.kind)!;
    for (const t of slice) analyzedUnion.add(`${t.sessionId}|${t.messageId}`);
  }
  const analyzedCount = analyzedUnion.size;

  const findings: WasteFinding[] = sortFindings([
    ...retryLoops.map(retryLoopToFinding),
    ...failureRuns.map(failureRunToFinding),
    ...cancelledRuns.map(cancellationRunToFinding),
    ...compactionLosses.map(compactionLossToFinding),
    ...editReverts.map(editRevertToFinding),
    ...editHeavySessions.map(editHeavyToFinding),
    ...skillRecallDups.map(skillRecallDupToFinding),
    ...skillPruningProtection.map(skillPruningProtectionToFinding),
    ...systemPromptTaxes.map(systemPromptTaxToFinding),
    ...ghostFindings.map((g) => ghostSurfaceToFinding(g)),
    ...toolOutputBloats.map(toolOutputBloatToFinding),
    ...toolCallPatterns.map(toolCallPatternToFinding),
  ]);

  if (args.flags['json'] === true) {
    process.stdout.write(
      JSON.stringify(
        {
          turnsAnalyzed: analyzedCount,
          retryLoops,
          failureRuns,
          cancelledRuns,
          compactions: compactionLosses,
          editReverts,
          skillRecallDups,
          skillPruningProtection,
          systemPromptTaxes,
          editHeavySessions,
          toolOutputBloats,
          toolCallPatterns,
          ghostSurface: ghostFindings,
          sessionSummaries,
          findings,
          fidelity: {
            analyzed: analyzedCount,
            excluded: total - analyzedCount,
            summary: fidelityAll,
            refused: false,
            perDetector: perDetectorCoverage.map(toJsonDetector),
          },
        },
        null,
        2,
      ) + '\n',
    );
    return 0;
  }

  if (args.flags['findings'] === true) {
    process.stdout.write(formatFindingsReport(findings, analyzedCount));
    return 0;
  }

  const showAll = args.flags['all'] === true;
  const limit = showAll ? Number.POSITIVE_INFINITY : DEFAULT_TOP_N;

  const out: string[] = [];
  out.push('');
  out.push(`turns analyzed: ${formatInt(analyzedCount)}`);
  for (const d of perDetectorCoverage) {
    const notice = formatPerDetectorNotice(d, total);
    if (notice) out.push(notice);
  }
  out.push(
    `sessions with patterns: ${formatInt(sessionSummaries.length)}  /  total pattern cost: ${formatUsd(
      sessionSummaries.reduce((s, r) => s + r.totalPatternCost, 0),
    )}`,
  );
  out.push('');

  if (selected.has('retries')) {
    out.push('Retry loops (≥3 identical failing tool calls in a row)');
    out.push(renderRetryTable(retryLoops, limit));
    out.push('');
  }
  if (selected.has('failures')) {
    out.push('Consecutive tool-failure runs (≥3 distinct tools failing in sequence)');
    out.push(renderFailureTable(failureRuns, limit));
    out.push('');
  }
  if (selected.has('cancellations') || cancelledRuns.length > 0) {
    out.push('Cancelled tool/subagent runs');
    out.push(renderCancellationTable(cancelledRuns, limit));
    out.push('');
  }
  if (selected.has('compaction')) {
    out.push('Compaction-loss events');
    out.push(renderCompactionTable(compactionLosses, limit));
    out.push('');
  }
  if (selected.has('reverts')) {
    out.push('Edit-revert cycles (file returned to a prior state)');
    out.push(renderRevertTable(editReverts, limit));
    out.push('');
  }
  if (selected.has('edit-heavy')) {
    out.push('Edit-heavy sessions (edits/reads > 4, ≥5 edits)');
    out.push(renderEditHeavyTable(editHeavySessions, limit));
    out.push('');
  }
  if (selected.has('opencode-skill-recall')) {
    out.push('OpenCode skill recall duplicates (same skill called ≥2 times, content not deduplicated)');
    out.push(renderSkillRecallTable(skillRecallDups, limit));
    out.push('');
  }
  if (selected.has('opencode-skill-pruning')) {
    out.push('OpenCode skill pruning protection (skill content never evicted from cache)');
    out.push(renderSkillPruningTable(skillPruningProtection, limit));
    out.push('');
  }
  if (selected.has('opencode-system-prompt')) {
    out.push('OpenCode system prompt / skill catalog tax (fixed prefix riding in cache on every turn)');
    out.push(renderSystemPromptTable(systemPromptTaxes, limit));
    out.push('');
  }
  if (selected.has('ghost-surface')) {
    out.push('Ghost user-installed surface (agents/skills/commands/prompts/rules/memories never invoked)');
    out.push(renderGhostSurfaceTable(ghostFindings, limit));
    out.push('');
  }
  if (selected.has('tool-output-bloat')) {
    out.push('Oversized tool output bloat (BASH_MAX_OUTPUT_LENGTH config + cross-harness >15k tok tool_results)');
    out.push(renderToolOutputBloatTable(toolOutputBloats, limit));
    out.push('');
  }
  if (selected.has('tool-call-pattern')) {
    out.push('Tool-call patterns (vanilla call sequences with consolidatable overhead)');
    out.push(renderToolCallPatternsTable(toolCallPatterns, limit));
    out.push('');
  }

  process.stdout.write(out.join('\n'));
  return 0;
}

function toJsonDetector(d: PatternDetectorCoverage): {
  kind: PatternKind;
  analyzed: number;
  excluded: number;
  refused: boolean;
  required: ReadonlyArray<keyof Coverage>;
  excludedBySource?: Array<{
    source: SourceKind;
    count: number;
    granularities: string[];
    missingCoverage: Array<keyof Coverage>;
  }>;
} {
  const required: ReadonlyArray<keyof Coverage> =
    d.kind === 'compaction' || d.kind === 'ghost-surface' || d.kind === 'tool-output-bloat'
      ? []
      : PATTERN_REQUIRED[d.kind];
  const out: ReturnType<typeof toJsonDetector> = {
    kind: d.kind,
    analyzed: d.analyzed,
    excluded: d.excluded,
    refused: d.refused,
    required,
  };
  if (d.breakdown && d.breakdown.sources.size > 0) {
    out.excludedBySource = [...d.breakdown.sources].map(([source, row]) => ({
      source,
      count: row.count,
      granularities: [...row.granularities].sort(),
      missingCoverage: [...row.missing],
    }));
  }
  return out;
}

function formatPerDetectorNotice(
  d: PatternDetectorCoverage,
  total: number,
): string | undefined {
  if (d.excluded === 0) return undefined;
  if (d.kind === 'compaction' || d.kind === 'ghost-surface' || d.kind === 'tool-output-bloat') return undefined;
  const required = PATTERN_REQUIRED[d.kind as Exclude<PatternKind, 'compaction' | 'ghost-surface' | 'tool-output-bloat'>];
  const sourceClauses = d.breakdown ? renderInlineSourceClauses(d.breakdown) : [];
  const requirements = required.map(fmtCoverageKey).join(' + ');
  return `${d.kind}: analyzed ${formatInt(d.analyzed)} of ${formatInt(total)} turns; ${formatInt(d.excluded)} excluded (needs ${requirements}; ${sourceClauses.join(' and ') || 'no source breakdown'})`;
}

function buildSessionSummaries(
  retryLoops: PatternsResult['retryLoops'],
  failureRuns: PatternsResult['failureRuns'],
  cancelledRuns: PatternsResult['cancelledRuns'],
  compactions: PatternsResult['compactions'],
  editReverts: PatternsResult['editReverts'],
  skillRecallDups: PatternsResult['skillRecallDups'],
  skillPruningProtection: PatternsResult['skillPruningProtection'],
  systemPromptTaxes: PatternsResult['systemPromptTaxes'],
  editHeavySessions: PatternsResult['editHeavySessions'],
): PatternsResult['sessionSummaries'] {
  const by = new Map<string, PatternsResult['sessionSummaries'][number]>();
  const get = (sessionId: string): PatternsResult['sessionSummaries'][number] => {
    let row = by.get(sessionId);
    if (!row) {
      row = {
        sessionId,
        retryLoopCount: 0,
        failureRunCount: 0,
        cancellationRunCount: 0,
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
  for (const c of cancelledRuns) {
    const row = get(c.sessionId);
    row.cancellationRunCount++;
    row.totalPatternCost += c.cost;
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
    // Cost intentionally omitted from totalPatternCost — see the matching
    // note in patterns.ts: edit-heavy turns also feed retry-loop and
    // edit-revert costs, so adding them again would double-count.
  }
  return [...by.values()].sort((a, b) => b.totalPatternCost - a.totalPatternCost);
}

function renderRetryTable(loops: PatternsResult['retryLoops'], limit: number): string {
  if (loops.length === 0) return '  (none)';
  const rows: string[][] = [
    ['session', 'tool', 'target', 'attempts', 'turns', 'cost'],
  ];
  const slice = [...loops].sort((a, b) => b.cost - a.cost).slice(0, limit);
  for (const r of slice) {
    rows.push([
      r.sessionId.slice(0, 8),
      r.tool,
      truncate(r.target ?? '—', 40),
      String(r.attempts),
      `${r.startTurnIndex}–${r.endTurnIndex}`,
      formatUsd(r.cost),
    ]);
  }
  return table(rows);
}

function renderFailureTable(runs: PatternsResult['failureRuns'], limit: number): string {
  if (runs.length === 0) return '  (none)';
  const rows: string[][] = [
    ['session', 'length', 'turns', 'tools', 'cost'],
  ];
  const slice = [...runs].sort((a, b) => b.cost - a.cost).slice(0, limit);
  for (const r of slice) {
    rows.push([
      r.sessionId.slice(0, 8),
      String(r.length),
      `${r.startTurnIndex}–${r.endTurnIndex}`,
      truncate(r.toolsInvolved.join(', '), 40),
      formatUsd(r.cost),
    ]);
  }
  return table(rows);
}

function renderCancellationTable(runs: PatternsResult['cancelledRuns'], limit: number): string {
  if (runs.length === 0) return '  (none)';
  const rows: string[][] = [
    ['session', 'length', 'turns', 'tools', 'source', 'cost'],
  ];
  const slice = [...runs].sort((a, b) => b.cost - a.cost).slice(0, limit);
  for (const r of slice) {
    rows.push([
      r.sessionId.slice(0, 8),
      String(r.length),
      `${r.startTurnIndex}–${r.endTurnIndex}`,
      truncate(r.toolsInvolved.join(', '), 40),
      r.eventSource,
      formatUsd(r.cost),
    ]);
  }
  return table(rows);
}

function renderCompactionTable(
  events: PatternsResult['compactions'],
  limit: number,
): string {
  if (events.length === 0) return '  (none)';
  const rows: string[][] = [
    ['session', 'ts', 'cacheLost(tok)', 'cost'],
  ];
  const slice = [...events]
    .sort((a, b) => b.cacheLostCost - a.cacheLostCost)
    .slice(0, limit);
  for (const e of slice) {
    rows.push([
      e.sessionId.slice(0, 8),
      e.ts,
      formatInt(e.tokensBeforeCompact),
      formatUsd(e.cacheLostCost),
    ]);
  }
  return table(rows);
}

function renderRevertTable(
  cycles: PatternsResult['editReverts'],
  limit: number,
): string {
  if (cycles.length === 0) return '  (none)';
  const rows: string[][] = [
    ['session', 'file', 'firstEdit', 'revert', 'span', 'cost'],
  ];
  const slice = [...cycles].sort((a, b) => b.cost - a.cost).slice(0, limit);
  for (const c of slice) {
    rows.push([
      c.sessionId.slice(0, 8),
      truncate(c.filePath, 40),
      String(c.firstEditTurnIndex),
      String(c.revertTurnIndex),
      String(c.spanTurns),
      formatUsd(c.cost),
    ]);
  }
  return table(rows);
}

function renderSkillRecallTable(
  dups: PatternsResult['skillRecallDups'],
  limit: number,
): string {
  if (dups.length === 0) return '  (none)';
  const rows: string[][] = [
    ['session', 'skill', 'calls', 'turns', 'cost'],
  ];
  const slice = [...dups].sort((a, b) => b.cost - a.cost).slice(0, limit);
  for (const d of slice) {
    rows.push([
      d.sessionId.slice(0, 8),
      truncate(d.skillName, 30),
      String(d.callCount),
      `${d.firstTurnIndex}–${d.lastTurnIndex}`,
      formatUsd(d.cost),
    ]);
  }
  return table(rows);
}

function renderSkillPruningTable(
  events: PatternsResult['skillPruningProtection'],
  limit: number,
): string {
  if (events.length === 0) return '  (none)';
  const rows: string[][] = [
    ['session', 'skill', 'invokedAt', 'ridingTurns', 'lastCached', 'cost'],
  ];
  const slice = [...events].sort((a, b) => b.cost - a.cost).slice(0, limit);
  for (const e of slice) {
    rows.push([
      e.sessionId.slice(0, 8),
      truncate(e.skillName, 30),
      String(e.invokedTurnIndex),
      String(e.ridingTurns),
      String(e.lastCachedTurnIndex),
      formatUsd(e.cost),
    ]);
  }
  return table(rows);
}

function renderSystemPromptTable(
  taxes: PatternsResult['systemPromptTaxes'],
  limit: number,
): string {
  if (taxes.length === 0) return '  (none)';
  const rows: string[][] = [
    ['session', 'prefix(tok)', 'userMsg(tok)', 'systemPrompt(tok)', 'ridingTurns', 'cost'],
  ];
  const slice = [...taxes].sort((a, b) => b.totalCost - a.totalCost).slice(0, limit);
  for (const t of slice) {
    rows.push([
      t.sessionId.slice(0, 8),
      formatInt(t.firstTurnCacheCreate),
      formatInt(t.firstUserMessageTokens),
      formatInt(t.estimatedSystemPromptTokens),
      formatInt(t.ridingTurns),
      formatUsd(t.totalCost),
    ]);
  }
  return table(rows);
}

// Unified findings table — one row per WasteFinding, sorted by severity then
// usdPerSession. Lets callers see retry-loop / failure-run / compaction-loss /
// edit-revert / edit-heavy / skill-* / system-prompt findings ranked together
// instead of flipping between four bespoke tables. Per-detector tables
// remain the default render path; this is opt-in via `--findings`.
export function formatFindingsReport(findings: WasteFinding[], analyzed: number): string {
  const out: string[] = [];
  out.push('');
  out.push(`turns analyzed: ${formatInt(analyzed)}`);
  out.push(`findings: ${formatInt(findings.length)}`);
  out.push('');
  if (findings.length === 0) {
    out.push('  (no hotspot findings)');
    out.push('');
    return out.join('\n');
  }
  const rows: string[][] = [['severity', 'kind', 'session', 'usd', 'title']];
  for (const f of findings) {
    const usd = f.estimatedSavings.usdPerSession;
    rows.push([
      f.severity,
      f.kind,
      f.sessionId.slice(0, 8),
      usd !== undefined ? formatUsd(usd) : '—',
      truncate(f.title, 80),
    ]);
  }
  out.push(table(rows));
  out.push('');
  return out.join('\n');
}

function renderEditHeavyTable(
  sessions: PatternsResult['editHeavySessions'],
  limit: number,
): string {
  if (sessions.length === 0) return '  (none)';
  const rows: string[][] = [
    ['source', 'session', 'reads', 'edits', 'ratio', 'retries', 'cost'],
  ];
  const slice = [...sessions].sort((a, b) => b.editCount - a.editCount).slice(0, limit);
  for (const s of slice) {
    rows.push([
      s.source,
      s.sessionId.slice(0, 8),
      formatInt(s.readCount),
      formatInt(s.editCount),
      Number.isFinite(s.ratio) ? s.ratio.toFixed(1) : '∞',
      String(s.likelyRetries),
      formatUsd(s.cost),
    ]);
  }
  return table(rows);
}

// One ledger pass + in-memory bucket. The per-session form
// `queryUserTurns({sessionId})` re-streams the entire ledger.jsonl on every
// call, so issuing it once per session costs O(sessions × ledger-size). Used
// by both the attribution loader and the patterns helper below.
//
// `q.since` / `q.source` are forwarded so the streaming filter narrows the
// in-memory buffer to the same window the eligible turns live in. This is
// safe because the user turn that carries tool_results for an eligible
// assistant turn arrives immediately after it: `userTurn.ts >= turn.ts`,
// so any user turn whose blocks join an eligible turn also passes the same
// `since` cutoff. We deliberately do NOT pass `q.until` — a user turn may
// lag a few seconds past a hard until cutoff while still carrying the
// tool_results for the last eligible turn — and we do NOT pass `sessionId`
// (defeats the bulk call) or `project` (per `userTurnPasses`, it does not
// filter user turns).
export async function bulkUserTurnsBySession(
  sessionIds: Set<string>,
  q: Query = {},
): Promise<Map<string, UserTurnRecord[]>> {
  const out = new Map<string, UserTurnRecord[]>();
  if (sessionIds.size === 0) return out;
  const filter: Query = {};
  if (q.since !== undefined) filter.since = q.since;
  if (q.source !== undefined) filter.source = q.source;
  const all = await queryUserTurns(filter);
  for (const ut of all) {
    if (!sessionIds.has(ut.sessionId)) continue;
    const list = out.get(ut.sessionId);
    if (list) list.push(ut);
    else out.set(ut.sessionId, [ut]);
  }
  return out;
}

// Fallback for callers that don't supply a loader. Per-session form so peak
// memory stays bounded on long historical ledgers; the production path
// (`runHotspots`) overrides this with a q-bound bulk loader that issues a
// single ledger pass narrowed to the current `since`/`source` window.
async function defaultLoadUserTurnsBySession(
  sessionIds: Set<string>,
): Promise<Map<string, UserTurnRecord[]>> {
  const out = new Map<string, UserTurnRecord[]>();
  for (const sessionId of sessionIds) {
    const rows = await queryUserTurns({ sessionId });
    if (rows.length > 0) out.set(sessionId, rows);
  }
  return out;
}

// Default loader for content sidecars. Each sidecar is its own file under
// ~/.relayburn/content/, so this stays per-session — just wrapped in a
// progress task with a periodic counter so a long loop doesn't look frozen.
async function defaultLoadContentBySession(
  sessionIds: Set<string>,
): Promise<Map<string, ContentRecord[]>> {
  return withProgress('reading content sidecars', async (task) => {
    const out = new Map<string, ContentRecord[]>();
    const total = sessionIds.size;
    let i = 0;
    for (const sessionId of sessionIds) {
      i++;
      task.update(`reading content sidecars (${formatInt(i)}/${formatInt(total)})`);
      const records = await readContent({ sessionId });
      if (records.length > 0) out.set(sessionId, records);
    }
    task.succeed(
      `read content for ${formatInt(out.size)} session${out.size === 1 ? '' : 's'}`,
    );
    return out;
  });
}

async function userTurnsForPatternDetectors(
  perDetector: Map<PatternKind, EnrichedTurn[]>,
  q?: Query,
): Promise<Map<string, UserTurnRecord[]>> {
  const sessionIds = new Set<string>();
  for (const turns of perDetector.values()) {
    for (const t of turns) sessionIds.add(t.sessionId);
  }
  return bulkUserTurnsBySession(sessionIds, q);
}

// Reads the per-session content sidecar for every session that lands in any
// of the requested detector slices. Sessions whose sidecar is empty (content
// store is hash-only / off, or content was pruned) are silently omitted —
// `detectPatterns` keys enrichment off the map being non-empty per session,
// so the absent entry yields the graceful-degradation behavior the
// enrichment layer promises.
async function contentForPatternDetectors(
  perDetector: Map<PatternKind, EnrichedTurn[]>,
  detectors: PatternKind[],
): Promise<Map<string, ContentRecord[]>> {
  const sessionIds = new Set<string>();
  for (const d of detectors) {
    const slice = perDetector.get(d);
    if (!slice) continue;
    for (const t of slice) sessionIds.add(t.sessionId);
  }
  const out = new Map<string, ContentRecord[]>();
  for (const sessionId of sessionIds) {
    const records = await readContent({ sessionId });
    if (records.length > 0) out.set(sessionId, records);
  }
  return out;
}

// Pull every `ToolResultEventRecord` whose session appears in `turns`.
// Pattern graph detectors and tool-output-bloat both use this. We filter
// post-query rather than issuing one per session so we avoid N round-trips on
// large slices; the ledger reader streams a single pass over the JSONL file.
async function loadToolResultEventsForTurns(
  turns: EnrichedTurn[],
  q: Query = {},
): Promise<ToolResultEventRecord[]> {
  if (turns.length === 0) return [];
  const sessionIds = new Set<string>();
  for (const t of turns) sessionIds.add(t.sessionId);
  const events = await queryToolResultEvents(q);
  return events.filter((e) => sessionIds.has(e.sessionId));
}

function renderToolCallPatternsTable(
  findings: ToolCallPatternFinding[],
  limit: number,
): string {
  if (findings.length === 0) return '  (none)';
  const rows: string[][] = [
    ['session', 'category', 'count', 'tokensSaved', 'usdSaved'],
  ];
  const slice = [...findings].sort((a, b) => b.estimatedUsdSaved - a.estimatedUsdSaved).slice(0, limit);
  for (const f of slice) {
    rows.push([
      f.sessionId.slice(0, 8),
      f.category,
      String(f.occurrenceCount),
      formatInt(f.estimatedTokensSaved),
      formatUsd(f.estimatedUsdSaved),
    ]);
  }
  return table(rows);
}

function renderToolOutputBloatTable(bloats: ToolOutputBloat[], limit: number): string {
  if (bloats.length === 0) return '  (none)';
  const rows: string[][] = [
    ['source', 'tool', 'kind', 'count', 'max(tok)', 'p95(tok)', 'cost'],
  ];
  const slice = [...bloats].sort((a, b) => b.cost - a.cost).slice(0, limit);
  for (const b of slice) {
    rows.push([
      b.source,
      b.toolName,
      b.kind,
      String(b.occurrenceCount),
      formatInt(b.evidencedMaxOutput),
      b.evidencedP95Output !== undefined ? formatInt(b.evidencedP95Output) : '—',
      formatUsd(b.cost),
    ]);
  }
  return table(rows);
}

function renderGhostSurfaceTable(ghosts: GhostSurfaceFinding[], limit: number): string {
  if (ghosts.length === 0) return '  (none)';
  const rows: string[][] = [
    ['source', 'kind', 'path', 'tokens', 'sessions', 'cost', 'note'],
  ];
  const slice = ghosts.slice(0, limit);
  for (const g of slice) {
    rows.push([
      g.source,
      g.kind.replace('ghost-', ''),
      truncate(g.path, 60),
      formatInt(g.sizeTokens),
      formatInt(g.sessionCount),
      formatUsd(g.cost),
      g.countedByCatalogBloat ? 'catalog' : '',
    ]);
  }
  return table(rows);
}
