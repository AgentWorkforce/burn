import { computeQuality, loadPricing } from '@relayburn/analyze';
import { costForTurn, sumCosts } from '@relayburn/analyze';
import type { CostBreakdown, OutcomeLabel, QualityResult } from '@relayburn/analyze';
import { queryAll, readContent, type Query } from '@relayburn/ledger';
import type { EnrichedTurn } from '@relayburn/ledger';
import type { ContentRecord } from '@relayburn/reader';

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

  const ingestReport = await ingestAll();
  const pricing = await loadPricing();
  const turns = await queryAll(q);

  const rowsByModel = aggregateByModel(turns, pricing);
  const totalCost = sumCosts(rowsByModel.map((r) => r.cost));

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
      formatInt(r.usage.input),
      formatInt(r.usage.output),
      formatInt(r.usage.reasoning),
      formatInt(r.usage.cacheRead),
      formatInt(r.usage.cacheCreate5m + r.usage.cacheCreate1h),
      formatUsd(r.cost.total),
    ]);
  }
  lines.push(table(dataRows));
  lines.push('');
  lines.push(`total cost: ${formatUsd(totalCost.total)}`);
  lines.push(
    `  input ${formatUsd(totalCost.input)} / output ${formatUsd(totalCost.output)} / reasoning ${formatUsd(totalCost.reasoning)} / cacheRead ${formatUsd(totalCost.cacheRead)} / cacheCreate ${formatUsd(totalCost.cacheCreate)}`,
  );
  lines.push('');

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
  const sessionIds = new Set(turns.map((t) => t.sessionId));
  const bySession = new Map<string, ContentRecord[]>();
  for (const sessionId of sessionIds) {
    const records = await readContent({ sessionId });
    if (records.length > 0) bySession.set(sessionId, records);
  }
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
  cost: CostBreakdown;
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
    const c = costForTurn(t, pricing);
    if (c) {
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
