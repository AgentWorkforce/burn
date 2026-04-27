import {
  aggregateByBash,
  aggregateByFile,
  aggregateBySubagent,
  attributeWaste,
  detectPatterns,
  loadPricing,
  summarizeFidelity,
  type BashAggregation,
  type FidelitySummary,
  type FileAggregation,
  type PatternsResult,
  type SubagentAggregation,
  type WasteResult,
} from '@relayburn/analyze';
import {
  queryAll,
  queryCompactions,
  queryUserTurns,
  readContent,
  type EnrichedTurn,
  type Query,
} from '@relayburn/ledger';
import type {
  ContentRecord,
  Coverage,
  SourceKind,
  UserTurnRecord,
} from '@relayburn/reader';

import { ingestAll } from '../ingest.js';
import { formatInt, formatUsd, parseSinceArg, table } from '../format.js';
import type { ParsedArgs } from '../args.js';
import { filterTurnsByProvider, parseProviderFilter } from '../provider.js';

const DEFAULT_TOP_N = 10;
const PATTERN_KINDS = ['retries', 'failures', 'compaction', 'reverts'] as const;
type PatternKind = (typeof PATTERN_KINDS)[number];

// When even-split sessions reach this fraction of the matched set, the
// attribution caveat is promoted from a footer note to a top banner and
// every dollar table heading is suffixed with "(approximate)". Below this
// fraction the current footer note is preserved. (#60)
const EVEN_SPLIT_DEGRADED_THRESHOLD = 0.5;

export function isAttributionDegraded(
  result: WasteResult,
  threshold: number = EVEN_SPLIT_DEGRADED_THRESHOLD,
): boolean {
  if (result.sessionTotals.length === 0) return false;
  const evenSplit = result.sessionTotals.filter(
    (s) => s.attributionMethod === 'even-split',
  ).length;
  return evenSplit / result.sessionTotals.length >= threshold;
}

// Coverage flags a turn must carry to participate in `attributeWaste` and the
// matching aggregators. A turn missing either flag has no chronology we can
// allocate cost against (no per-call records, or no result-side bytes to
// allocate the next-turn input delta over). Records without `fidelity` (older
// ledger writers, foreign sources) are treated as best-effort full per #41 —
// they pass the gate.
export const ATTRIBUTION_REQUIRED: ReadonlyArray<keyof Coverage> = [
  'hasToolCalls',
  'hasToolResultEvents',
];

// Returns `true` if the turn carries every coverage flag in `required`.
// Records without `fidelity` (older ledger writers, foreign sources) are
// treated as best-effort full per #41 — they pass regardless of `required`.
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

export async function runWaste(args: ParsedArgs): Promise<number> {
  const q: Query = {};
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);
  if (typeof args.flags['project'] === 'string') q.project = args.flags['project'];
  if (typeof args.flags['session'] === 'string') q.sessionId = args.flags['session'];
  if (typeof args.flags['workflow'] === 'string') q.enrichment = { workflowId: args.flags['workflow'] };
  const providerFilter = parseProviderFilter(args.flags['provider']);
  if (providerFilter instanceof Error) {
    process.stderr.write(providerFilter.message);
    return 2;
  }

  await ingestAll();
  const pricing = await loadPricing();
  const turns = filterTurnsByProvider(await queryAll(q), providerFilter);

  const patternsFlag = args.flags['patterns'];
  if (patternsFlag !== undefined) {
    const selected = resolvePatternSelection(patternsFlag);
    const sessionIds = new Set(turns.map((t) => t.sessionId));
    const compactions = selected.has('compaction')
      ? (await queryCompactions(q)).filter((c) => sessionIds.has(c.sessionId))
      : [];
    return runPatternsMode(args, turns, pricing, compactions, selected);
  }

  return runWasteAttribution(args, turns, pricing);
}

// Exposed for tests so they can drive the orchestration with fixture turns
// and a mocked content/userTurns loader. Production callers go through
// `runWaste`, which fetches both via the ledger.
export interface WasteAttributionDeps {
  loadContentForSession?: (sessionId: string) => Promise<ContentRecord[]>;
  loadUserTurnsForSession?: (sessionId: string) => Promise<UserTurnRecord[]>;
}

export async function runWasteAttribution(
  args: ParsedArgs,
  turns: EnrichedTurn[],
  pricing: Awaited<ReturnType<typeof loadPricing>>,
  deps: WasteAttributionDeps = {},
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
  // mirrors the "hard-fail with a clear message" wording from #41.
  if (total > 0 && eligible.length === 0) {
    const breakdown = describeExcluded(excluded, ATTRIBUTION_REQUIRED);
    const sourcesClause = renderSourcesClause(breakdown);
    const message =
      `burn waste: ${total}/${total} turns lack tool-call/tool-result coverage required for waste attribution. ` +
      `Sources: ${sourcesClause}. No waste analysis was performed.`;
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

  const loadContent =
    deps.loadContentForSession ??
    ((sessionId: string) => readContent({ sessionId }));
  const loadUserTurns =
    deps.loadUserTurnsForSession ??
    ((sessionId: string) => queryUserTurns({ sessionId }));

  const sessionIds = new Set(eligible.map((t) => t.sessionId));
  const contentBySession = new Map<string, ContentRecord[]>();
  const userTurnsBySession = new Map<string, UserTurnRecord[]>();
  for (const sessionId of sessionIds) {
    const records = await loadContent(sessionId);
    if (records.length > 0) contentBySession.set(sessionId, records);
    const userTurns = await loadUserTurns(sessionId);
    if (userTurns.length > 0) userTurnsBySession.set(sessionId, userTurns);
  }

  const result = attributeWaste(eligible, {
    pricing,
    contentBySession,
    userTurnsBySession,
  });
  const files = aggregateByFile(result.attributions);
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

  const reportInput: FormatWasteReportInput = {
    turnsAnalyzed: eligible.length,
    result,
    files,
    bashes,
    subagents,
    limit,
    degraded,
  };
  if (coverageNotice !== undefined) reportInput.coverageNotice = coverageNotice;
  process.stdout.write(formatWasteReport(reportInput));
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

interface FormatWasteReportInput {
  turnsAnalyzed: number;
  result: WasteResult;
  files: FileAggregation[];
  bashes: BashAggregation[];
  subagents: SubagentAggregation[];
  limit: number;
  degraded: boolean;
  coverageNotice?: string;
}

export function formatWasteReport(input: FormatWasteReportInput): string {
  const { turnsAnalyzed, result, files, bashes, subagents, limit, degraded, coverageNotice } = input;
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
      `⚠ attribution is degraded: ${formatInt(ev)} of ${formatInt(total)} sessions (${pct.toFixed(1)}%) have no content`,
    );
    out.push(
      '  sidecar, so file / bash / subagent costs for those sessions are approximate',
    );
    out.push(
      "  (even-split over turn N+1 input/cacheCreate). Run 'burn rebuild --content'",
    );
    out.push(
      "  to backfill sidecars from source session files, or see 'burn content' for",
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
        'note: no content sidecar data found — using even-split (initial cost only). Enable content.store=full in your config to get persistence attribution.',
      );
    } else if (evenSplitSessions.length > 0) {
      out.push(
        `note: ${evenSplitSessions.length}/${result.sessionTotals.length} sessions used even-split (no content sidecar).`,
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

  out.push(`Top Bash commands by cost${approxSuffix}`);
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
//
// The revert detector needs editPreHash / editPostHash, which require
// hasRawContent upstream (the parser computes the hashes from the raw
// strings). hasToolCalls is the obvious prereq.
export const PATTERN_REQUIRED: Record<
  Exclude<PatternKind, 'compaction'>,
  ReadonlyArray<keyof Coverage>
> = {
  retries: ['hasToolCalls', 'hasToolResultEvents'],
  failures: ['hasToolCalls', 'hasToolResultEvents'],
  reverts: ['hasToolCalls', 'hasRawContent'],
};

interface PatternDetectorCoverage {
  kind: PatternKind;
  analyzed: number;
  excluded: number;
  // Only set when this detector required coverage (compaction never does).
  breakdown?: CoverageGapBreakdown;
  // Whether the detector ran on any turns at all.
  refused: boolean;
}

export async function runPatternsMode(
  args: ParsedArgs,
  turns: EnrichedTurn[],
  pricing: Awaited<ReturnType<typeof loadPricing>>,
  compactions: Awaited<ReturnType<typeof queryCompactions>>,
  selected: Set<PatternKind>,
): Promise<number> {
  const total = turns.length;
  const fidelityAll = summarizeFidelity(turns);

  // Per-detector filtered slices. `compaction` always runs on the full slice
  // because its data path (the sidecar) doesn't go through TurnRecord at all.
  const perDetector = new Map<PatternKind, EnrichedTurn[]>();
  const perDetectorCoverage: PatternDetectorCoverage[] = [];

  for (const kind of selected) {
    if (kind === 'compaction') {
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

  // Refusal: every selected detector refused. Compaction has no fidelity
  // prereq and is recorded with refused:false unconditionally, so its
  // presence in `selected` short-circuits this — we only refuse when the
  // entire selection is fidelity-gated and every detector lost its slice.
  const refusableSelected = perDetectorCoverage.filter(
    (d) => d.kind !== 'compaction',
  );
  const allRefused =
    perDetectorCoverage.length > 0 &&
    perDetectorCoverage.every((d) => d.refused);

  if (allRefused) {
    const lines: string[] = [];
    for (const d of refusableSelected) {
      const required = PATTERN_REQUIRED[d.kind as Exclude<PatternKind, 'compaction'>];
      const sourcesClause = d.breakdown ? renderSourcesClause(d.breakdown) : '(unknown sources)';
      lines.push(
        `  ${d.kind}: ${total}/${total} turns lack ${required.map(fmtCoverageKey).join(' + ')} (sources: ${sourcesClause})`,
      );
    }
    const message =
      `burn waste --patterns: no selected detectors can run on this slice.\n` +
      lines.join('\n') +
      `\nNo pattern analysis was performed.`;

    if (args.flags['json'] === true) {
      process.stdout.write(
        JSON.stringify(
          {
            turnsAnalyzed: 0,
            retryLoops: [],
            failureRuns: [],
            compactions: [],
            editReverts: [],
            sessionSummaries: [],
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
  let compactionLosses: PatternsResult['compactions'] = [];
  let editReverts: PatternsResult['editReverts'] = [];
  let sessionSummaries: PatternsResult['sessionSummaries'] = [];

  if (selected.has('retries')) {
    const r = detectPatterns(perDetector.get('retries')!, { pricing });
    retryLoops = r.retryLoops;
  }
  if (selected.has('failures')) {
    const r = detectPatterns(perDetector.get('failures')!, { pricing });
    failureRuns = r.failureRuns;
  }
  if (selected.has('compaction')) {
    const r = detectPatterns(perDetector.get('compaction')!, { pricing, compactions });
    compactionLosses = r.compactions;
  }
  if (selected.has('reverts')) {
    const r = detectPatterns(perDetector.get('reverts')!, { pricing });
    editReverts = r.editReverts;
  }

  // Build session summaries on the union — anything attributed by *any*
  // detector counts. Re-running detectPatterns on a single union slice
  // doesn't work because each detector has its own coverage threshold; instead
  // synthesize the summary from the per-detector results.
  sessionSummaries = buildSessionSummaries(
    retryLoops,
    failureRuns,
    compactionLosses,
    editReverts,
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

  if (args.flags['json'] === true) {
    process.stdout.write(
      JSON.stringify(
        {
          turnsAnalyzed: analyzedCount,
          retryLoops,
          failureRuns,
          compactions: compactionLosses,
          editReverts,
          sessionSummaries,
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
    d.kind === 'compaction' ? [] : PATTERN_REQUIRED[d.kind];
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
  if (d.kind === 'compaction') return undefined;
  const required = PATTERN_REQUIRED[d.kind as Exclude<PatternKind, 'compaction'>];
  const sourceClauses = d.breakdown ? renderInlineSourceClauses(d.breakdown) : [];
  const requirements = required.map(fmtCoverageKey).join(' + ');
  return `${d.kind}: analyzed ${formatInt(d.analyzed)} of ${formatInt(total)} turns; ${formatInt(d.excluded)} excluded (needs ${requirements}; ${sourceClauses.join(' and ') || 'no source breakdown'})`;
}

function buildSessionSummaries(
  retryLoops: PatternsResult['retryLoops'],
  failureRuns: PatternsResult['failureRuns'],
  compactions: PatternsResult['compactions'],
  editReverts: PatternsResult['editReverts'],
): PatternsResult['sessionSummaries'] {
  const by = new Map<string, PatternsResult['sessionSummaries'][number]>();
  const get = (sessionId: string): PatternsResult['sessionSummaries'][number] => {
    let row = by.get(sessionId);
    if (!row) {
      row = {
        sessionId,
        retryLoopCount: 0,
        failureRunCount: 0,
        consecutiveFailureMax: 0,
        compactionCount: 0,
        editRevertCount: 0,
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
