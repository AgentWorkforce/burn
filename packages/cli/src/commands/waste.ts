import {
  aggregateByBash,
  aggregateByFile,
  aggregateBySubagent,
  attributeWaste,
  detectPatterns,
  loadPricing,
  summarizeFidelity,
  type BashAggregation,
  type FileAggregation,
  type PatternsResult,
  type SubagentAggregation,
  type WasteResult,
} from '@relayburn/analyze';
import { queryAll, queryCompactions, readContent, type Query } from '@relayburn/ledger';
import type { ContentRecord, Fidelity, FidelityClass } from '@relayburn/reader';

import { ingestAll } from '../ingest.js';
import { formatInt, formatUsd, parseSinceArg, table } from '../format.js';
import type { ParsedArgs } from '../args.js';

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

export async function runWaste(args: ParsedArgs): Promise<number> {
  const q: Query = {};
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);
  if (typeof args.flags['project'] === 'string') q.project = args.flags['project'];
  if (typeof args.flags['session'] === 'string') q.sessionId = args.flags['session'];
  if (typeof args.flags['workflow'] === 'string') q.enrichment = { workflowId: args.flags['workflow'] };

  await ingestAll();
  const pricing = await loadPricing();
  const turns = await queryAll(q);
  const fidelitySupport = checkWasteFidelity(turns);
  if (!fidelitySupport.supported) {
    if (args.flags['json'] === true) {
      process.stdout.write(JSON.stringify(fidelitySupport, null, 2) + '\n');
    } else {
      process.stderr.write(renderWasteFidelityError(fidelitySupport));
    }
    return 2;
  }

  const patternsFlag = args.flags['patterns'];
  if (patternsFlag !== undefined) {
    const selected = resolvePatternSelection(patternsFlag);
    const compactions = selected.has('compaction')
      ? await queryCompactions(q)
      : [];
    const patterns = detectPatterns(turns, { pricing, compactions });
    return renderPatterns(args, patterns, selected, turns.length);
  }

  const sessionIds = new Set(turns.map((t) => t.sessionId));
  const contentBySession = new Map<string, ContentRecord[]>();
  for (const sessionId of sessionIds) {
    const records = await readContent({ sessionId });
    if (records.length > 0) contentBySession.set(sessionId, records);
  }

  const result = attributeWaste(turns, { pricing, contentBySession });
  const files = aggregateByFile(result.attributions);
  const bashes = aggregateByBash(result.attributions);
  const subagents = aggregateBySubagent(result.attributions);
  const degraded = isAttributionDegraded(result);

  if (args.flags['json'] === true) {
    process.stdout.write(
      JSON.stringify(
        {
          turnsAnalyzed: turns.length,
          grandTotal: result.grandTotal,
          attributedTotal: result.attributedTotal,
          unattributedTotal: result.unattributedTotal,
          attributionDegraded: degraded,
          sessions: result.sessionTotals,
          files,
          bash: bashes,
          subagents,
        },
        null,
        2,
      ) + '\n',
    );
    return 0;
  }

  const showAll = args.flags['all'] === true;
  const limit = showAll ? Number.POSITIVE_INFINITY : DEFAULT_TOP_N;

  process.stdout.write(
    formatWasteReport({
      turnsAnalyzed: turns.length,
      result,
      files,
      bashes,
      subagents,
      limit,
      degraded,
    }),
  );
  return 0;
}

export interface WasteFidelitySupport {
  supported: boolean;
  turnsAnalyzed: number;
  unsupportedTurns: number;
  missingPrerequisites: string[];
  unsupportedByClass: Record<FidelityClass, number>;
  fidelity: ReturnType<typeof summarizeFidelity>;
}

const WASTE_PREREQ_LABELS = {
  toolCalls: 'tool calls',
  toolResultEvents: 'tool result events',
  contentLengths: 'content lengths',
  sessionRelationships: 'session relationships',
  perTurnUsage: 'per-turn usage',
} as const;

export function checkWasteFidelity(
  turns: readonly { fidelity?: Fidelity; toolCalls: { name: string }[] }[],
): WasteFidelitySupport {
  const missing = new Set<string>();
  const unsupportedByClass: Record<FidelityClass, number> = {
    full: 0,
    'usage-only': 0,
    'aggregate-only': 0,
    'cost-only': 0,
    partial: 0,
  };
  let unsupportedTurns = 0;
  const hasSubagentCalls = turns.some((t) =>
    t.toolCalls.some((tc) => tc.name === 'Agent' || tc.name === 'Task'),
  );

  for (const t of turns) {
    const f = t.fidelity;
    if (!f) continue;
    let turnUnsupported = false;
    if (f.granularity !== 'per-turn' && f.granularity !== 'per-message') {
      missing.add(WASTE_PREREQ_LABELS.perTurnUsage);
      turnUnsupported = true;
    }
    if (!f.coverage.hasToolCalls) {
      missing.add(WASTE_PREREQ_LABELS.toolCalls);
      turnUnsupported = true;
    }
    if (!f.coverage.hasToolResultEvents) {
      missing.add(WASTE_PREREQ_LABELS.toolResultEvents);
      turnUnsupported = true;
    }
    if (!f.coverage.hasRawContent) {
      missing.add(WASTE_PREREQ_LABELS.contentLengths);
      turnUnsupported = true;
    }
    if (hasSubagentCalls && !f.coverage.hasSessionRelationships) {
      missing.add(WASTE_PREREQ_LABELS.sessionRelationships);
      turnUnsupported = true;
    }
    if (turnUnsupported) {
      unsupportedTurns++;
      unsupportedByClass[f.class]++;
    }
  }

  return {
    supported: missing.size === 0,
    turnsAnalyzed: turns.length,
    unsupportedTurns,
    missingPrerequisites: [...missing].sort(),
    unsupportedByClass,
    fidelity: summarizeFidelity(turns),
  };
}

function renderWasteFidelityError(support: WasteFidelitySupport): string {
  const byClass = Object.entries(support.unsupportedByClass)
    .filter(([, n]) => n > 0)
    .map(([cls, n]) => `${n} ${cls}`)
    .join(', ');
  const detail = byClass.length > 0 ? ` (${byClass})` : '';
  return [
    'burn waste: selected turns do not preserve enough fidelity for attribution.',
    `missing prerequisites: ${support.missingPrerequisites.join(', ')}`,
    `unsupported turns: ${support.unsupportedTurns}/${support.turnsAnalyzed}${detail}`,
    'No attribution was computed; re-ingest with a reader that preserves tool results and content lengths.',
    '',
  ].join('\n');
}

interface FormatWasteReportInput {
  turnsAnalyzed: number;
  result: WasteResult;
  files: FileAggregation[];
  bashes: BashAggregation[];
  subagents: SubagentAggregation[];
  limit: number;
  degraded: boolean;
}

export function formatWasteReport(input: FormatWasteReportInput): string {
  const { turnsAnalyzed, result, files, bashes, subagents, limit, degraded } = input;
  const evenSplitSessions = result.sessionTotals.filter(
    (s) => s.attributionMethod === 'even-split',
  );

  const out: string[] = [];
  out.push('');
  out.push(`turns analyzed: ${formatInt(turnsAnalyzed)}`);
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

function resolvePatternSelection(flag: string | true): Set<PatternKind> {
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

function renderPatterns(
  args: ParsedArgs,
  patterns: PatternsResult,
  selected: Set<PatternKind>,
  turnsAnalyzed: number,
): number {
  const retryLoops = selected.has('retries') ? patterns.retryLoops : [];
  const failureRuns = selected.has('failures') ? patterns.failureRuns : [];
  const compactions = selected.has('compaction') ? patterns.compactions : [];
  const editReverts = selected.has('reverts') ? patterns.editReverts : [];

  if (args.flags['json'] === true) {
    process.stdout.write(
      JSON.stringify(
        {
          turnsAnalyzed,
          retryLoops,
          failureRuns,
          compactions,
          editReverts,
          sessionSummaries: patterns.sessionSummaries,
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
  out.push(`turns analyzed: ${formatInt(turnsAnalyzed)}`);
  out.push(
    `sessions with patterns: ${formatInt(patterns.sessionSummaries.length)}  /  total pattern cost: ${formatUsd(
      patterns.sessionSummaries.reduce((s, r) => s + r.totalPatternCost, 0),
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
    out.push(renderCompactionTable(compactions, limit));
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
