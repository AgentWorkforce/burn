import {
  aggregateSubagentTypeStats,
  buildSubagentTree,
  computeQuality,
  loadPricing,
  summarizeFidelity,
} from '@relayburn/analyze';
import { costForTurn, sumCosts } from '@relayburn/analyze';
import type {
  CostBreakdown,
  FidelitySummary,
  OutcomeLabel,
  QualityResult,
  SubagentTreeNode,
} from '@relayburn/analyze';
import { queryAll, readContent, type Query } from '@relayburn/ledger';
import type { EnrichedTurn } from '@relayburn/ledger';
import type { ContentRecord, Coverage } from '@relayburn/reader';

import { ingestAll } from '../ingest.js';
import { formatInt, formatUsd, parseSinceArg, table } from '../format.js';
import type { ParsedArgs } from '../args.js';

export async function runSummary(args: ParsedArgs): Promise<number> {
  const q: Query = {};
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);
  if (typeof args.flags['project'] === 'string') q.project = args.flags['project'];
  if (typeof args.flags['session'] === 'string') q.sessionId = args.flags['session'];
  if (typeof args.flags['workflow'] === 'string') q.enrichment = { workflowId: args.flags['workflow'] };
  if (typeof args.flags['agent'] === 'string') q.enrichment = { ...(q.enrichment ?? {}), agentId: args.flags['agent'] };

  const subagentTreeFlag = args.flags['subagent-tree'];
  const subagentTypeFlag = args.flags['by-subagent-type'] === true;

  const ingestReport = await ingestAll();
  const pricing = await loadPricing();
  const turns = await queryAll(q);

  if (subagentTreeFlag !== undefined) {
    return renderSubagentTreeMode(args, turns, pricing, subagentTreeFlag, q);
  }
  if (subagentTypeFlag) {
    return renderSubagentTypeMode(args, turns, pricing);
  }

  const rowsByModel = aggregateByModel(turns, pricing);
  const totalCost = sumCosts(rowsByModel.map((r) => r.cost));
  const fidelity = summarizeFidelity(turns);

  if (args.flags['json'] === true) {
    // JSON contract: numeric usage fields are always numbers, but the
    // companion `fidelity` block is the only honest answer to "are these
    // zeros real?". Programmatic consumers should consult `missingCoverage`
    // before trusting any aggregate.
    const payload = {
      ingest: {
        ingestedSessions: ingestReport.ingestedSessions,
        appendedTurns: ingestReport.appendedTurns,
      },
      turns: turns.length,
      pricedTurns: rowsByModel.reduce((sum, r) => sum + r.pricedTurns, 0),
      totalCost,
      byModel: rowsByModel.map((r) => ({
        model: r.model,
        turns: r.turns,
        usage: r.usage,
        usageCoverage: r.usageCoverage,
        pricedTurns: r.pricedTurns,
        cost: r.cost,
      })),
      fidelity,
    };
    process.stdout.write(JSON.stringify(payload, null, 2) + '\n');
    return 0;
  }

  const lines: string[] = [];
  lines.push('');
  lines.push(
    `ingested ${ingestReport.ingestedSessions} new session${ingestReport.ingestedSessions === 1 ? '' : 's'} (+${formatInt(ingestReport.appendedTurns)} turns)`,
  );
  lines.push('');
  lines.push(`turns analyzed: ${formatInt(turns.length)}`);
  lines.push('');

  if (turns.length === 0) {
    lines.push('no turns match the current filters.');
    process.stdout.write(lines.join('\n') + '\n');
    return 0;
  }

  const header = ['model', 'turns', 'input', 'output', 'reasoning', 'cacheRead', 'cacheCreate', 'cost'];
  const dataRows: string[][] = [header];
  for (const r of rowsByModel) {
    dataRows.push([
      r.model,
      formatInt(r.turns),
      formatUsageCell(r, 'input'),
      formatUsageCell(r, 'output'),
      formatUsageCell(r, 'reasoning'),
      formatUsageCell(r, 'cacheRead'),
      formatUsageCell(r, 'cacheCreate'),
      formatCostCell(r),
    ]);
  }
  lines.push(table(dataRows));
  lines.push('');
  const costPartial = rowsByModel.some((r) => r.pricedTurns < r.turns);
  const pricedTurns = rowsByModel.reduce((sum, r) => sum + r.pricedTurns, 0);
  if (pricedTurns === 0) {
    lines.push('total cost: —');
    lines.push('  cost breakdown unavailable');
  } else {
    lines.push(`total cost: ${formatUsd(totalCost.total)}${costPartial ? '*' : ''}`);
    lines.push(
      `  input ${formatUsd(totalCost.input)} / output ${formatUsd(totalCost.output)} / reasoning ${formatUsd(totalCost.reasoning)} / cacheRead ${formatUsd(totalCost.cacheRead)} / cacheCreate ${formatUsd(totalCost.cacheCreate)}`,
    );
  }
  lines.push('');

  // Only print a fidelity line when *something* is below full — the common
  // all-Claude case is full fidelity for every turn, and noise there would
  // train people to ignore the line in cases that actually matter.
  const fidelityNotice = renderFidelityNotice(fidelity);
  if (fidelityNotice) {
    lines.push(fidelityNotice);
    lines.push('');
  }
  if (rowsByModel.some(hasPartialDisplayCoverage)) {
    lines.push(
      '* partial coverage: some numeric fields or prices are unknown for at least one row (use --json for counts).',
    );
    lines.push('');
  }

  if (args.flags['quality'] === true) {
    const contentBySession = await loadContentForQuality(turns);
    const quality = computeQuality(turns, { contentBySession });
    lines.push(renderQuality(quality));
    lines.push('');
  }

  process.stdout.write(lines.join('\n'));
  return 0;
}

async function loadContentForQuality(
  turns: EnrichedTurn[],
): Promise<Map<string, ContentRecord[]>> {
  const sessionIds = [...new Set(turns.map((t) => t.sessionId))];
  const bySession = new Map<string, ContentRecord[]>();
  // Sequential reads across thousands of sessions (many with no sidecar at
  // all → ENOENT path) dominate runtime on large summaries. Cap concurrency
  // so we don't fan out unboundedly on huge ledgers but still overlap I/O.
  const concurrency = Math.min(8, sessionIds.length);
  let next = 0;
  async function worker(): Promise<void> {
    while (next < sessionIds.length) {
      const sessionId = sessionIds[next++]!;
      const records = await readContent({ sessionId });
      if (records.length > 0) bySession.set(sessionId, records);
    }
  }
  await Promise.all(Array.from({ length: concurrency }, () => worker()));
  return bySession;
}

function renderQuality(q: QualityResult): string {
  if (q.outcomes.length === 0) return 'quality: (no sessions)';
  const counts = outcomeCounts(q);
  const oneShotOverall = weightedOneShotRate(q);
  const summary = [
    `quality — sessions: ${q.outcomes.length}`,
    `  outcomes: ${counts.completed} completed / ${counts.abandoned} abandoned / ${counts.errored} errored / ${counts.unknown} unknown`,
    oneShotOverall === undefined
      ? '  one-shot rate: n/a (no edit turns)'
      : `  one-shot rate: ${(oneShotOverall * 100).toFixed(1)}% across ${counts.editTurns} edit turns`,
  ];
  return summary.join('\n');
}

function outcomeCounts(q: QualityResult): Record<OutcomeLabel, number> & {
  editTurns: number;
} {
  const counts: Record<OutcomeLabel, number> & { editTurns: number } = {
    completed: 0,
    abandoned: 0,
    errored: 0,
    unknown: 0,
    editTurns: 0,
  };
  for (const o of q.outcomes) counts[o.outcome]++;
  for (const m of q.oneShot) counts.editTurns += m.editTurns;
  return counts;
}

function weightedOneShotRate(q: QualityResult): number | undefined {
  let edit = 0;
  let oneShot = 0;
  for (const m of q.oneShot) {
    edit += m.editTurns;
    oneShot += m.oneShotTurns;
  }
  return edit > 0 ? oneShot / edit : undefined;
}

interface ModelRow {
  model: string;
  turns: number;
  usage: EnrichedTurn['usage'];
  usageCoverage: Record<UsageField, FieldCoverage>;
  pricedTurns: number;
  cost: CostBreakdown;
}

type UsageField = 'input' | 'output' | 'reasoning' | 'cacheRead' | 'cacheCreate';

interface FieldCoverage {
  knownTurns: number;
  missingTurns: number;
  unknownTurns: number;
}

function renderSubagentTreeMode(
  args: ParsedArgs,
  turns: EnrichedTurn[],
  pricing: Parameters<typeof costForTurn>[1],
  flag: string | true,
  q: Query,
): number {
  // Accept either `--subagent-tree <id>` or `--subagent-tree` with --session.
  const sessionId = typeof flag === 'string' ? flag : q.sessionId;
  if (!sessionId) {
    process.stderr.write('burn: --subagent-tree requires a session id (positional or --session)\n');
    return 2;
  }
  const sessionTurns = turns.filter((t) => t.sessionId === sessionId);
  if (sessionTurns.length === 0) {
    process.stdout.write(`no turns found for session ${sessionId}\n`);
    return 0;
  }
  const trees = buildSubagentTree(sessionTurns, { pricing });
  const root = trees.get(sessionId);
  if (!root) {
    process.stdout.write(`no turns found for session ${sessionId}\n`);
    return 0;
  }
  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify(root, null, 2) + '\n');
    return 0;
  }
  const out: string[] = [];
  out.push('');
  out.push(`session: ${sessionId}`);
  out.push(`total: ${formatUsd(root.cumulativeCost)} across ${formatInt(root.cumulativeTurns)} turn${root.cumulativeTurns === 1 ? '' : 's'}`);
  out.push('');
  for (const line of renderTree(root)) out.push(line);
  out.push('');
  process.stdout.write(out.join('\n'));
  return 0;
}

function renderSubagentTypeMode(
  args: ParsedArgs,
  turns: EnrichedTurn[],
  pricing: Parameters<typeof costForTurn>[1],
): number {
  const stats = aggregateSubagentTypeStats(turns, { pricing });
  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify(stats, null, 2) + '\n');
    return 0;
  }
  const out: string[] = [];
  out.push('');
  out.push(`subagent invocations: ${formatInt(stats.reduce((a, s) => a + s.invocations, 0))}`);
  out.push('');
  if (stats.length === 0) {
    out.push('  (no subagent turns in range)');
    out.push('');
    process.stdout.write(out.join('\n'));
    return 0;
  }
  const rows: string[][] = [
    ['subagentType', 'invocations', 'turns', 'total', 'median', 'p95', 'mean'],
  ];
  for (const s of stats) {
    rows.push([
      s.subagentType,
      formatInt(s.invocations),
      formatInt(s.turns),
      formatUsd(s.totalCost),
      formatUsd(s.medianCost),
      formatUsd(s.p95Cost),
      formatUsd(s.meanCost),
    ]);
  }
  out.push(table(rows));
  out.push('');
  process.stdout.write(out.join('\n'));
  return 0;
}

function renderTree(root: SubagentTreeNode): string[] {
  const out: string[] = [];
  out.push(renderNodeLine(root, ''));
  renderChildren(root, '', out);
  return out;
}

function renderChildren(node: SubagentTreeNode, prefix: string, out: string[]): void {
  const n = node.children.length;
  for (let i = 0; i < n; i++) {
    const c = node.children[i]!;
    const isLast = i === n - 1;
    const branch = isLast ? '└─ ' : '├─ ';
    out.push(renderNodeLine(c, prefix + branch));
    const childPrefix = prefix + (isLast ? '   ' : '│  ');
    renderChildren(c, childPrefix, out);
  }
}

function renderNodeLine(node: SubagentTreeNode, indent: string): string {
  const label = node.label;
  const model = node.models.length > 0 ? ` (${node.models.join(', ')})` : '';
  const cost = formatUsd(node.cumulativeCost);
  const turns = `[${formatInt(node.cumulativeTurns)} turn${node.cumulativeTurns === 1 ? '' : 's'}]`;
  return `${indent}${label}${model}  ${cost}  ${turns}`;
}

function renderFidelityNotice(f: FidelitySummary): string | undefined {
  // Returns undefined when every classified turn is full fidelity *and* no
  // unknown turns exist — i.e. every number above is trustworthy. Otherwise
  // surfaces a one-liner so the user knows which buckets to be skeptical of.
  const nonFull =
    f.byClass['usage-only'] +
    f.byClass['aggregate-only'] +
    f.byClass['cost-only'] +
    f.byClass.partial;
  if (nonFull === 0 && f.unknown === 0) return undefined;
  const parts: string[] = [];
  if (f.byClass.full > 0) parts.push(`${f.byClass.full} full`);
  if (f.byClass['usage-only'] > 0) parts.push(`${f.byClass['usage-only']} usage-only`);
  if (f.byClass['aggregate-only'] > 0) {
    parts.push(`${f.byClass['aggregate-only']} aggregate-only`);
  }
  if (f.byClass['cost-only'] > 0) parts.push(`${f.byClass['cost-only']} cost-only`);
  if (f.byClass.partial > 0) parts.push(`${f.byClass.partial} partial`);
  if (f.unknown > 0) parts.push(`${f.unknown} unknown`);
  return `fidelity: ${parts.join(' / ')} (use --json for per-field coverage)`;
}

function aggregateByModel(turns: EnrichedTurn[], pricing: Parameters<typeof costForTurn>[1]): ModelRow[] {
  const byModel = new Map<string, ModelRow>();
  for (const t of turns) {
    const key = t.model || 'unknown';
    let row = byModel.get(key);
    if (!row) {
      row = {
        model: key,
        turns: 0,
        usage: { input: 0, output: 0, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
        usageCoverage: emptyUsageCoverage(),
        pricedTurns: 0,
        cost: { model: key, total: 0, input: 0, output: 0, reasoning: 0, cacheRead: 0, cacheCreate: 0 },
      };
      byModel.set(key, row);
    }
    row.turns++;
    row.usage.input += t.usage.input;
    row.usage.output += t.usage.output;
    row.usage.reasoning += t.usage.reasoning;
    row.usage.cacheRead += t.usage.cacheRead;
    row.usage.cacheCreate5m += t.usage.cacheCreate5m;
    row.usage.cacheCreate1h += t.usage.cacheCreate1h;
    updateUsageCoverage(row, t);
    const c = hasCostCoverage(t) ? costForTurn(t, pricing) : null;
    if (c) {
      row.pricedTurns++;
      row.cost.total += c.total;
      row.cost.input += c.input;
      row.cost.output += c.output;
      row.cost.reasoning += c.reasoning;
      row.cost.cacheRead += c.cacheRead;
      row.cost.cacheCreate += c.cacheCreate;
    }
  }
  return [...byModel.values()].sort((a, b) => b.cost.total - a.cost.total);
}

function emptyUsageCoverage(): Record<UsageField, FieldCoverage> {
  return {
    input: emptyFieldCoverage(),
    output: emptyFieldCoverage(),
    reasoning: emptyFieldCoverage(),
    cacheRead: emptyFieldCoverage(),
    cacheCreate: emptyFieldCoverage(),
  };
}

function emptyFieldCoverage(): FieldCoverage {
  return { knownTurns: 0, missingTurns: 0, unknownTurns: 0 };
}

function updateUsageCoverage(row: ModelRow, turn: EnrichedTurn): void {
  updateField(row.usageCoverage.input, turn, 'hasInputTokens');
  updateField(row.usageCoverage.output, turn, 'hasOutputTokens');
  updateField(row.usageCoverage.reasoning, turn, 'hasReasoningTokens');
  updateField(row.usageCoverage.cacheRead, turn, 'hasCacheReadTokens');
  updateField(row.usageCoverage.cacheCreate, turn, 'hasCacheCreateTokens');
}

function updateField(
  field: FieldCoverage,
  turn: EnrichedTurn,
  coverageKey: keyof Coverage,
): void {
  if (!turn.fidelity) {
    field.unknownTurns++;
    return;
  }
  if (turn.fidelity.coverage[coverageKey]) field.knownTurns++;
  else field.missingTurns++;
}

function formatUsageCell(row: ModelRow, field: UsageField): string {
  const c = row.usageCoverage[field];
  const value = usageValue(row, field);
  if (c.knownTurns === 0 && c.unknownTurns === 0 && c.missingTurns > 0) return '—';
  const suffix = c.knownTurns === row.turns && c.unknownTurns === 0 ? '' : '*';
  return `${formatInt(value)}${suffix}`;
}

function usageValue(row: ModelRow, field: UsageField): number {
  switch (field) {
    case 'input':
      return row.usage.input;
    case 'output':
      return row.usage.output;
    case 'reasoning':
      return row.usage.reasoning;
    case 'cacheRead':
      return row.usage.cacheRead;
    case 'cacheCreate':
      return row.usage.cacheCreate5m + row.usage.cacheCreate1h;
  }
}

function formatCostCell(row: ModelRow): string {
  if (row.pricedTurns === 0) return '—';
  const suffix = row.pricedTurns === row.turns ? '' : '*';
  return `${formatUsd(row.cost.total)}${suffix}`;
}

function hasPartialDisplayCoverage(row: ModelRow): boolean {
  if (row.pricedTurns < row.turns) return true;
  return Object.values(row.usageCoverage).some(
    (c) => c.missingTurns > 0 || c.unknownTurns > 0,
  );
}

function hasCostCoverage(turn: EnrichedTurn): boolean {
  const c = turn.fidelity?.coverage;
  if (!c) return true;
  return c.hasInputTokens && c.hasOutputTokens;
}
